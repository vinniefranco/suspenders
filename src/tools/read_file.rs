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

use detect::FileType;

pub struct ReadFile;

/// VERBATIM from qwen v0.16.0 `tools/read-file.ts` (the description passed to the
/// `ReadFileTool` constructor).
const DESCRIPTION: &str = "Reads and returns the content of a specified file. If the file is large, the content will be truncated. The tool's response will clearly indicate if truncation has occurred and will provide details on how to read more of the file using the 'offset' and 'limit' parameters. Handles text, images (PNG, JPG, GIF, WEBP, SVG, BMP), PDF files, and Jupyter notebooks (.ipynb). For text files, it can read specific line ranges. For PDF files, use the 'pages' parameter to extract specific page ranges as text (e.g. '1-5'). Max 20 pages per request. This tool can read Jupyter notebooks (.ipynb) and returns structured cell content with outputs.";

#[async_trait::async_trait]
impl Tool for ReadFile {
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
                        "type": "number",
                        "description": "Optional: For text files, the 0-based line number to start reading from. Requires 'limit' to be set. Use for paginating through large files."
                    },
                    "limit": {
                        "type": "number",
                        "description": "Optional: For text files, maximum number of lines to read. Use with 'offset' to paginate through large files. If omitted, reads the entire file (if feasible, up to a default limit)."
                    },
                    "pages": {
                        "type": "string",
                        "description": "Optional: For PDF files, the page range to extract as text (e.g., '1-5', '3', '10-20'). Pages are 1-indexed. Max 20 pages per request. Open-ended ranges like '3-' are not supported. When provided, PDF content is extracted as text regardless of model capabilities."
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

        let display_name = params::basename(&path);
        // The file_unchanged fast-path (qwen read-file.ts): a repeat FULL read of
        // an unchanged file quotes the prior read back rather than re-emitting.
        if window.is_full()
            && let Some(out) = cache::unchanged_placeholder(ctx, &abs, &display_name)
            && let [crate::content::ResultBlock::Text { text }] = out.blocks.as_slice()
        {
            return Ok(text.clone());
        }

        let (content, truncated) = media::read_from(&abs, &path, window)?;
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

        let file_type = params::detect_file_type(&abs, &path)?;
        let display_name = params::basename(&path);

        // A FULL text read of an unchanged file short-circuits to the placeholder
        // (qwen's fast-path fires for the text arm only - media / notebook reads
        // are not text-cacheable). Detect that here before the heavier read.
        if window.is_full()
            && pages.is_none()
            && matches!(file_type, FileType::Text | FileType::Svg)
            && let Some(out) = cache::unchanged_placeholder(ctx, &abs, &display_name)
        {
            return Ok(out);
        }

        // Each branch produces its output and whether the read was FULL (the
        // whole current content, not windowed or internally truncated). After a
        // successful read we record into the Run's read cache (F6, ADR-0060):
        // `cacheable` is whether the result is a single Text block (a media /
        // native-PDF block is not text-cacheable), `full` is per-branch.
        let (output, full) = match file_type {
            FileType::Text => {
                let (text, truncated) = media::read_from(&abs, &path, window)?;
                (ToolOutput::text(text), window.is_full() && !truncated)
            }
            FileType::Svg => (
                ToolOutput::text(media::read_svg(&abs, &path, &display_name)?),
                true,
            ),
            FileType::Notebook => {
                let raw = media::read_text(&abs, &path)?;
                let read = notebook::read_with_meta(&raw)?;
                // A notebook whose rendered cell listing was truncated is NOT a
                // full read: notebook_edit refuses to cell-edit it (qwen's
                // `lastReadWasFull = !isTruncated`).
                (ToolOutput::text(read.content), !read.is_truncated)
            }
            FileType::Image => (media::read_image(&abs, &path, &display_name, ctx)?, true),
            FileType::Pdf => (
                media::read_pdf(&abs, &path, &display_name, pages.as_deref(), ctx).await?,
                true,
            ),
        };

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
