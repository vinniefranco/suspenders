//! `read_file(file_path, offset?, limit?, pages?)`: returns a file's content.
//! Text is the common case - a `[offset, offset+limit)` line window, returned as
//! one Text block via [`Tool::run`]. Media rides [`Tool::run_rich`] (ADR-0059,
//! P3 3b): a `.ipynb` and a text-extracted PDF become Text blocks; an image and
//! a native PDF become media blocks when the captured Model accepts the
//! modality, else they degrade to the VERBATIM unsupported-modality placeholder
//! at read time.
//!
//! The model-facing contract is a FAITHFUL port of qwen v0.16.0
//! `tools/read-file.ts`: an ABSOLUTE `file_path` (a relative one is refused with
//! qwen's verbatim message), a 0-based `offset` line, a `limit` line count, and a
//! PDF `pages` range. A partial text read is prefixed with qwen's
//! `"Showing lines X-Y of Z total lines."` notice. A repeat FULL read of an
//! unchanged file returns qwen's `file_unchanged` placeholder from the Run's
//! read cache (F6, ADR-0060) instead of re-emitting the bytes. A `file_path`
//! matching `.qwenignore` is refused with qwen's verbatim message.
//!
//! Size is not this tool's concern for the text branch: `Tools::run` shapes
//! every Tool Result to the Result Cap (the analog of qwen's char-limit
//! truncation), while offset/limit windowing (WHICH part) lives here. Media
//! blocks pass the shaping fold uncapped.
//!
//! Multimodal branches port qwen v0.16.0 `utils/fileUtils.ts`
//! `processSingleFileContent` (the svg / image / pdf / notebook switch arms, the
//! 9.9MB base64 guard, the 1MB SVG cap) and `tools/read-file.ts`
//! `validateToolParamValues` (the ipynb / pages rejections). Detection, notebook
//! formatting, and pdftotext extraction live in the submodules below.

use crate::tool::path::PathReject;
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

mod cache;
mod detect;
mod media;
mod notebook;
mod params;
mod pdf;
pub(crate) mod reader;

use detect::FileType;

pub struct ReadFile;

/// VERBATIM from qwen v0.21.4 `tools/read-file.ts` (the description passed to the
/// `ReadFileTool` constructor).
const DESCRIPTION: &str = "Reads and returns the content of a specified file. The file_path argument MUST be an absolute path. Always construct it by combining the project root with the file's relative path (e.g. project root '/path/to/project/' + relative 'foo/bar.txt' = '/path/to/project/foo/bar.txt'). If the user provides a relative path, resolve it against the project root first. If the file is large, the content will be truncated. The tool's response will clearly indicate if truncation has occurred and will provide details on how to read more of the file using the 'offset' and 'limit' parameters. Handles text, images (PNG, JPG, GIF, WEBP, SVG, BMP), PDF files, and Jupyter notebooks (.ipynb). For text files, it can read specific line ranges. For PDF files, use the 'pages' parameter to extract specific page ranges as text (e.g. '1-5'). Max 20 pages per request. Large PDFs cannot be read all at once when the model does not support native PDF input; retry with narrower page ranges if the tool reports a PDF is too large. With a configured vision bridge, failed PDF text extraction or an irreducibly large single page may be transcribed automatically, at most four pages per call; this transcription is lossy and marked as untrusted. This tool can read Jupyter notebooks (.ipynb) and returns structured cell content with outputs.";

#[async_trait::async_trait]
impl Tool for ReadFile {
    // Read-only (qwen read-file.ts:544 `Kind.Read`): allowed in plan mode.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Read
    }

    // A cut names the file-absolute 0-based `offset` that continues the read
    // (Shaping reads this Call's `offset` input so the marker's line numbers
    // stay file-absolute).
    fn cut_policy(&self) -> crate::tool::CutPolicy {
        crate::tool::CutPolicy::HeadWithResume
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: DESCRIPTION.into(),
            // Schema property set + each description string are VERBATIM from
            // qwen read-file.ts's `parameterSchema`.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to read (e.g., '/home/user/project/file.txt'). Relative paths are not supported. You must provide an absolute path."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Optional: For text files, the 0-based line number to start reading from. Requires 'limit' to be set. Use for paginating through large files."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Optional: For text files, maximum number of lines to read. Use with 'offset' to paginate through large files. If omitted, reads the entire file (if feasible, up to a default limit)."
                    },
                    "pages": {
                        "type": "string",
                        "description": "Optional: For PDF files, the page range to extract as text (e.g., '1-5', '3', '10-20'). Pages are 1-indexed. Max 20 pages per request. Open-ended ranges like '3-' are not supported. Use this for large PDFs or when the model does not support native PDF input."
                    }
                },
                "required": ["file_path"]
            }),
        }
    }

    /// The text branch: a `[offset, offset+limit)` line window. `run_rich`
    /// handles every non-text kind, so a call that reaches `run` directly (the
    /// default `run_rich` path, or a test) still reads text files exactly as the
    /// dispatch does.
    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = params::file_path(input)?;
        let window = params::window(input)?;
        let abs = resolve(&path, ctx)?;
        reject_qwenignored(&abs, ctx)?;

        let source = reader::SourceFile::new(&abs, &path);
        // The file_unchanged fast-path (qwen read-file.ts): a repeat FULL read of
        // an unchanged file quotes the prior read back rather than re-emitting.
        if window.is_full()
            && let Some(out) = cache::unchanged_placeholder(ctx, &source)
            && let [crate::content::ResultBlock::Text { text }] = out.blocks.as_slice()
        {
            return Ok(text.clone());
        }

        let (content, truncated) = media::read_from(&source, window)?;
        // A text read is cacheable; it is FULL when the whole file was returned
        // (no windowing AND not truncated). The result_cap truncation the Tools
        // dispatch applies after this return is not visible here - matching qwen,
        // whose read cache records at the read site (F6, ADR-0060).
        cache::record_read(ctx, &abs, window.is_full() && !truncated, true);
        Ok(content)
    }

    /// The multimodal dispatch (ADR-0059): detect the file kind and emit the
    /// right block list. Text/svg/notebook/text-PDF become one Text block; an
    /// image or native PDF becomes a media block when the Model accepts it, else
    /// the verbatim placeholder Text block (read-time modality degrade).
    async fn run_rich(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let path = params::file_path(input)?;
        let window = params::window(input)?;
        let pages = params::pages(input)?;

        // Resolve + confine, then reject a .qwenignore'd path (qwen validates
        // this before reading).
        let abs = resolve(&path, ctx)?;
        reject_qwenignored(&abs, ctx)?;
        params::validate_media_params(&path, &abs, window, pages.as_deref())?;

        let source = reader::SourceFile::new(&abs, &path);
        let file_type = source.detect()?;

        // A FULL text read of an unchanged file short-circuits to the placeholder
        // (qwen's fast-path fires for the text arm only - media / notebook reads
        // are not text-cacheable). Detect that here before the heavier read.
        if window.is_full()
            && pages.is_none()
            && matches!(file_type, FileType::Text | FileType::Svg)
            && let Some(out) = cache::unchanged_placeholder(ctx, &source)
        {
            return Ok(out);
        }

        // The per-kind reader switch is shared with read_many_files (ADR-0068):
        // it produces the output and whether the read was FULL (the whole current
        // content, not windowed or internally truncated), gating media on the
        // captured Model's modalities. After a successful read we record into the
        // Run's read cache (F6, ADR-0060): `cacheable` is whether the result is a
        // single Text block (a media / native-PDF block is not text-cacheable),
        // `full` is per-branch (read_many_files ignores it - it holds no cache).
        let (output, full) = reader::read_blocks(
            file_type,
            &source,
            window,
            pages.as_deref(),
            ctx.input_modalities,
        )
        .await?;

        let cacheable = cache::is_single_text_block(&output);
        cache::record_read(ctx, &abs, full, cacheable);
        Ok(output)
    }
}

/// Resolve a model-supplied ABSOLUTE `file_path` and confine it to the Project
/// Root (or the trusted memory subtree), rendering qwen's verbatim validation
/// messages for the two rejection shapes (read-file.ts `validateToolParams`).
fn resolve(path: &str, ctx: &ToolCtx) -> Result<std::path::PathBuf, String> {
    crate::tool::path::resolve_absolute_in(path, &ctx.root, ctx.memory_root.as_deref()).map_err(
        |reject| match reject {
            // VERBATIM qwen read-file.ts: the absolute-path requirement.
            PathReject::Relative => {
                format!("File path must be absolute, but was relative: {path}. You must provide an absolute path.")
            }
            // qwen has no read-file.ts message for a path outside the workspace
            // (it asks for confirmation via getDefaultPermission instead).
            // Suspenders confines every tool path to the Project Root, so an
            // escape is a hard refusal with the shared confinement wording.
            PathReject::Escapes => "path escapes project root".to_string(),
        },
    )
}

/// Refuse a `file_path` matching `.qwenignore` with qwen's verbatim message
/// (read-file.ts `validateToolParamValues` `shouldQwenIgnoreFile` arm). `abs` is
/// already root-confined; the message names the original absolute path.
fn reject_qwenignored(abs: &std::path::Path, ctx: &ToolCtx) -> Result<(), String> {
    if crate::walk::qwenignore::is_ignored(&ctx.root, abs) {
        // VERBATIM qwen read-file.ts.
        Err(format!(
            "File path '{}' is ignored by .qwenignore pattern(s).",
            abs.display()
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/tools/read_file.rs"]
mod tests;
