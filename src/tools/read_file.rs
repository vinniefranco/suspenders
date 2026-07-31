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
mod tests {
    use super::*;
    use crate::content::{Modalities, ResultBlock, unsupported_modality_placeholder};
    use base64::Engine;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    fn ctx_with(root: &std::path::Path, image: bool, pdf: bool) -> ToolCtx {
        ToolCtx::for_test_with_modalities(root.to_path_buf(), 10_000, Modalities { image, pdf })
    }

    // An absolute path to `rel` inside `root`, as a JSON string, since the tool
    // now requires an absolute file_path.
    fn abs(root: &std::path::Path, rel: &str) -> String {
        root.join(rel).to_string_lossy().into_owned()
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        ReadFile.run(&input, ctx).await
    }

    async fn run_rich(input: Value, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        ReadFile.run_rich(&input, ctx).await
    }

    // A 1x1 transparent PNG (68 bytes decoded), so an image read produces a real
    // base64 payload with the right magic bytes.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn spec_requires_file_path_and_advertises_offset_limit_pages() {
        let spec = ReadFile.spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.input_schema["required"], json!(["file_path"]));
        let props = &spec.input_schema["properties"];
        assert!(props["file_path"].is_object());
        assert!(props["offset"].is_object());
        assert!(props["limit"].is_object());
        assert!(props["pages"].is_object());
        // The old relative `path` param is gone.
        assert!(props["path"].is_null());
    }

    #[test]
    fn description_is_the_verbatim_qwen_string() {
        let spec = ReadFile.spec();
        let desc = spec.description;
        assert!(desc.starts_with("Reads and returns the content of a specified file."));
        assert!(desc.contains("using the 'offset' and 'limit' parameters"));
        // No suspenders-only additions.
        assert!(!desc.contains("Usage:"));
        assert!(!desc.contains("relative to the project root"));
    }

    // ---- absolute file_path contract --------------------------------------

    #[tokio::test]
    async fn reads_a_file_by_absolute_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hi there\n").unwrap();
        assert_eq!(
            run(
                json!({"file_path": abs(tmp.path(), "hello.txt")}),
                &ctx(tmp.path())
            )
            .await,
            Ok("hi there\n".into())
        );
    }

    #[tokio::test]
    async fn reads_a_nested_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub/dir")).unwrap();
        std::fs::write(tmp.path().join("sub/dir/a.txt"), "nested").unwrap();
        assert_eq!(
            run(
                json!({"file_path": abs(tmp.path(), "sub/dir/a.txt")}),
                &ctx(tmp.path())
            )
            .await,
            Ok("nested".into())
        );
    }

    #[tokio::test]
    async fn a_padded_file_path_is_trimmed_before_validation() {
        // qwen unescapePath(file_path.trim()) (read-file.ts): surrounding
        // whitespace is stripped, so a padded absolute path reads fine rather
        // than being rejected as relative.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("only.txt"), "body\n").unwrap();
        let padded = format!("  {}  ", abs(tmp.path(), "only.txt"));
        assert_eq!(
            run(json!({"file_path": padded}), &ctx(tmp.path())).await,
            Ok("body\n".into())
        );
    }

    #[tokio::test]
    async fn a_relative_path_is_the_verbatim_absolute_required_message() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"file_path": "hello.txt"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "File path must be absolute, but was relative: hello.txt. You must provide an absolute path."
        );
    }

    #[tokio::test]
    async fn returns_large_files_whole() {
        let tmp = TempDir::new().unwrap();
        let content = "a".repeat(50_123);
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();
        assert_eq!(
            run(
                json!({"file_path": abs(tmp.path(), "big.txt")}),
                &ctx(tmp.path())
            )
            .await,
            Ok(content)
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"file_path": abs(tmp.path(), "nope.txt")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("nope.txt"));
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn reading_a_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();
        let err = run(
            json!({"file_path": abs(tmp.path(), "somedir")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("somedir"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"file_path": "/etc/passwd"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_file_path_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(
            crate::tools::execute("read_file", &json!({}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute("read_file", &json!({"file_path": 42}), &c)
                .await
                .is_error
        );
    }

    // ---- .qwenignore ------------------------------------------------------

    #[tokio::test]
    async fn a_qwenignored_file_is_the_verbatim_rejection() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "shh").unwrap();
        let target = abs(tmp.path(), "secret.txt");
        let err = run(json!({"file_path": &target}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            format!("File path '{target}' is ignored by .qwenignore pattern(s).")
        );
    }

    // ---- offset/limit windowing -------------------------------------------

    #[tokio::test]
    async fn offset_and_limit_window_the_file_with_the_showing_lines_notice() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("lines.txt"),
            "one\ntwo\nthree\nfour\nfive\n",
        )
        .unwrap();
        // offset=1 (0-based, so start at line 2), limit=2 -> lines "two", "three".
        // split('\n') on a trailing-newline file yields 6 elements (total=6).
        let out = run(
            json!({"file_path": abs(tmp.path(), "lines.txt"), "offset": 1, "limit": 2}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(out.starts_with("Showing lines 2-3 of 6 total lines.\n\n---\n\n"));
        assert!(out.ends_with("two\nthree"));
    }

    #[tokio::test]
    async fn a_window_covering_the_whole_file_has_no_notice() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo").unwrap();
        // offset=0, limit large enough to cover both lines -> no truncation notice.
        let out = run(
            json!({"file_path": abs(tmp.path(), "lines.txt"), "offset": 0, "limit": 10}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(out, "one\ntwo");
    }

    #[tokio::test]
    async fn no_offset_or_limit_returns_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();
        assert_eq!(
            run(
                json!({"file_path": abs(tmp.path(), "lines.txt")}),
                &ctx(tmp.path())
            )
            .await,
            Ok("one\ntwo\n".into())
        );
    }

    #[tokio::test]
    async fn an_offset_past_the_end_yields_an_empty_window_with_the_notice() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();
        // total=3 (trailing newline). offset=5 clamps to end -> empty windowed
        // slice. qwen re-splits the joined `""` content into `[""]` (length 1),
        // so linesIncluded=1 and actualEndLine = startLine + 1 = 6; the notice's
        // first number is the PRE-clamp startLine+1 = 6. Byte-faithful to qwen's
        // `linesShown = [startLine + 1, actualEndLine]`: "Showing lines 6-6 of 3".
        let out = run(
            json!({"file_path": abs(tmp.path(), "lines.txt"), "offset": 5, "limit": 2}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(out.starts_with("Showing lines 6-6 of 3 total lines."));
    }

    #[tokio::test]
    async fn every_line_is_trim_ended_on_a_full_read() {
        // qwen strips trailing whitespace from EVERY returned line
        // (content.split('\n').map(trimEnd), fileUtils.ts:1029), on full reads
        // too - not just windowed ones.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("ws.txt"), "one  \ntwo\t\nthree\n").unwrap();
        assert_eq!(
            run(
                json!({"file_path": abs(tmp.path(), "ws.txt")}),
                &ctx(tmp.path())
            )
            .await,
            Ok("one\ntwo\nthree\n".into())
        );
    }

    #[tokio::test]
    async fn a_negative_offset_is_the_verbatim_message() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"file_path": abs(tmp.path(), "x.txt"), "offset": -1, "limit": 2}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "Offset must be a non-negative number");
    }

    #[tokio::test]
    async fn a_non_positive_limit_is_the_verbatim_message() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"file_path": abs(tmp.path(), "x.txt"), "limit": 0}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "Limit must be a positive number");
    }

    // ---- file_unchanged fast-path -----------------------------------------

    #[tokio::test]
    async fn a_repeat_full_read_serves_the_unchanged_placeholder() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "hello\nworld\n").unwrap();
        let c = ctx(tmp.path());
        let target = abs(tmp.path(), "f.txt");
        // First full read records a full+cacheable entry.
        assert_eq!(
            run(json!({"file_path": &target}), &c).await,
            Ok("hello\nworld\n".into())
        );
        // Second identical read is served from cache as the placeholder.
        let second = run(json!({"file_path": &target}), &c).await.unwrap();
        assert!(second.starts_with("[File f.txt unchanged since last read in this session"));
        assert!(second.contains("re-read with explicit offset/limit"));
    }

    #[tokio::test]
    async fn a_windowed_read_does_not_serve_the_unchanged_placeholder() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "hello\nworld\n").unwrap();
        let c = ctx(tmp.path());
        let target = abs(tmp.path(), "f.txt");
        // A partial read never records a full entry, so a follow-up full read
        // re-reads rather than serving the placeholder.
        run(json!({"file_path": &target, "offset": 0, "limit": 1}), &c)
            .await
            .unwrap();
        assert_eq!(
            run(json!({"file_path": &target}), &c).await,
            Ok("hello\nworld\n".into())
        );
    }

    // ---- run_rich text stays one Text block --------------------------------

    #[tokio::test]
    async fn run_rich_on_text_is_one_text_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hi.txt"), "hello\n").unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "hi.txt")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(out.blocks, vec![ResultBlock::text("hello\n")]);
    }

    // ---- svg branch (Text, 1MB cap) ----------------------------------------

    #[tokio::test]
    async fn svg_is_read_as_a_text_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("i.svg"), "<svg>hi</svg>").unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "i.svg")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(out.blocks, vec![ResultBlock::text("<svg>hi</svg>")]);
    }

    #[tokio::test]
    async fn svg_over_1mb_is_the_verbatim_skip_message() {
        let tmp = TempDir::new().unwrap();
        let big = format!(
            "<svg>{}</svg>",
            "a".repeat(media::svg_max_size_bytes() as usize + 10)
        );
        std::fs::write(tmp.path().join("big.svg"), big).unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "big.svg")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(
            out.blocks,
            vec![ResultBlock::text(
                "Cannot display content of SVG file larger than 1MB: big.svg"
            )]
        );
    }

    // ---- notebook branch (Text) --------------------------------------------

    #[tokio::test]
    async fn notebook_is_read_as_a_formatted_text_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("nb.ipynb"),
            r##"{"cells":[{"cell_type":"markdown","source":"# Hi"}]}"##,
        )
        .unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "nb.ipynb")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        match &out.blocks[0] {
            ResultBlock::Text { text } => {
                assert!(text.contains("Jupyter Notebook (python, 1 cells)"));
                assert!(text.contains("--- Markdown Cell cell-0 ---\n# Hi"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notebook_with_offset_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nb.ipynb"), r#"{"cells":[]}"#).unwrap();
        let err = run_rich(
            json!({"file_path": abs(tmp.path(), "nb.ipynb"), "offset": 2, "limit": 1}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not supported for Jupyter notebook"));
    }

    #[tokio::test]
    async fn notebook_with_pages_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nb.ipynb"), r#"{"cells":[]}"#).unwrap();
        let err = run_rich(
            json!({"file_path": abs(tmp.path(), "nb.ipynb"), "pages": "1-2"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("pages is not supported for Jupyter notebook"));
    }

    // ---- image branch ------------------------------------------------------

    #[tokio::test]
    async fn image_with_capable_model_rides_as_a_base64_image_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pic.png"), PNG_1X1).unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "pic.png")}),
            &ctx_with(tmp.path(), true, false),
        )
        .await
        .unwrap();
        match &out.blocks[0] {
            ResultBlock::Image { mime, data } => {
                assert_eq!(mime, "image/png");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, PNG_1X1);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_with_incapable_model_degrades_to_the_verbatim_placeholder() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pic.png"), PNG_1X1).unwrap();
        // Default ctx has image=false.
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "pic.png")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(
            out.blocks,
            vec![ResultBlock::text(unsupported_modality_placeholder(
                "image", "pic.png"
            ))]
        );
    }

    #[tokio::test]
    async fn oversized_image_is_the_verbatim_data_uri_error() {
        let tmp = TempDir::new().unwrap();
        // 8MB of PNG-magic bytes -> base64 ~10.7MB, over the 9.9MB guard.
        let mut bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.resize(8 * 1024 * 1024, 0);
        std::fs::write(tmp.path().join("huge.png"), &bytes).unwrap();
        let err = run_rich(
            json!({"file_path": abs(tmp.path(), "huge.png")}),
            &ctx_with(tmp.path(), true, false),
        )
        .await
        .unwrap_err();
        assert!(err.contains("File exceeds the 10MB data URI limit after base64 encoding"));
        assert!(err.contains("MB encoded)."));
    }

    // ---- pdf branch --------------------------------------------------------

    #[tokio::test]
    async fn native_pdf_with_capable_model_and_no_pages_is_a_document_block() {
        let tmp = TempDir::new().unwrap();
        let mut bytes = Vec::from(*b"%PDF-1.7\n");
        bytes.extend_from_slice(b"body bytes");
        std::fs::write(tmp.path().join("doc.pdf"), &bytes).unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "doc.pdf")}),
            &ctx_with(tmp.path(), false, true),
        )
        .await
        .unwrap();
        match &out.blocks[0] {
            ResultBlock::Document { mime, data } => {
                assert_eq!(mime, "application/pdf");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, bytes);
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pdf_with_pages_forces_text_extraction_even_on_a_capable_model() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("doc.pdf"), b"%PDF-1.7\n").unwrap();
        // pdf-capable, but `pages` forces the pdftotext text path. On a machine
        // without pdftotext this yields the verbatim missing-binary error (still
        // the text path, not a Document block).
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "doc.pdf"), "pages": "1-2"}),
            &ctx_with(tmp.path(), false, true),
        )
        .await;
        match out {
            Ok(o) => {
                // If pdftotext exists and produced text, it's a Text block.
                assert!(matches!(o.blocks[0], ResultBlock::Text { .. }));
            }
            Err(e) => {
                // No Document block ever; the text path surfaced an error.
                assert!(e.contains("Cannot extract text from PDF"));
            }
        }
    }

    #[tokio::test]
    async fn pdf_without_pdf_modality_takes_the_text_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("doc.pdf"), b"%PDF-1.7\n").unwrap();
        let out = run_rich(
            json!({"file_path": abs(tmp.path(), "doc.pdf")}),
            &ctx(tmp.path()),
        )
        .await;
        // pdf=false so no Document block; text path (Ok text or Failed error).
        match out {
            Ok(o) => assert!(matches!(o.blocks[0], ResultBlock::Text { .. })),
            Err(e) => assert!(e.contains("Cannot extract text from PDF")),
        }
    }
}
