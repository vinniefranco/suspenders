//! `read_file(path, start_line = 1, pages?)`: returns a file's content. Text is
//! the common case - a window of lines from `start_line` on, returned as one
//! Text block via [`Tool::run`]. Media rides [`Tool::run_rich`] (ADR-0059, P3
//! 3b): a `.ipynb` and a text-extracted PDF become Text blocks; an image and a
//! native PDF become media blocks when the captured Model accepts the modality,
//! else they degrade to the VERBATIM unsupported-modality placeholder at read
//! time.
//!
//! Size is not this tool's concern for the text branch: `Tools::run` shapes
//! every Tool Result to the Result Cap, and read_file's shaping cut names the
//! `start_line` that continues a truncated read - `start_line` is windowing
//! (WHICH part), the cap stays the size authority (HOW MUCH). Media blocks pass
//! the shaping fold uncapped.
//!
//! Multimodal branches port qwen v0.16.0 `utils/fileUtils.ts`
//! `processSingleFileContent` (the svg / image / pdf / notebook switch arms, the
//! 9.9MB base64 guard, the 1MB SVG cap) and `tools/read-file.ts`
//! `validateToolParamValues` (the ipynb / pages rejections). Detection, notebook
//! formatting, and pdftotext extraction live in the submodules below.

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

const DESCRIPTION: &str = "\
Reads and returns the content of a specified file. If the file is large, the content will be \
truncated. The tool's response will clearly indicate if truncation has occurred and will provide \
the `start_line` value you can pass back to read more of the file. Handles text, images (PNG, JPG, \
GIF, WEBP, SVG, BMP), PDF files, and Jupyter notebooks (.ipynb). For text files, it can read a \
specific window of a file by line. For PDF files, use the `pages` parameter to extract specific \
page ranges as text (e.g. \"1-5\"). Max 20 pages per request. This tool can read Jupyter notebooks \
(.ipynb) and returns structured cell content with outputs.\n\
\n\
Usage:\n\
- Always read a file with this tool before you edit it, so that edit_file's `old_str` matches the \
real text exactly, including whitespace and indentation.\n\
- The `path` is relative to the project root (e.g. \"src/main.rs\" or \"Cargo.toml\"), NOT an \
absolute path.\n\
- Large files are truncated to keep the response manageable. When that happens, a note at the cut \
tells you the exact `start_line` to continue from; call read_file again with that `start_line` to \
page through the rest of the file. Repeat until you have read everything you need.\n\
- `start_line` is 1-based and defaults to 1 (the top of the file). It applies to text files only; \
notebooks, images, and PDFs are always read in full.\n\
- For PDF files, use `pages` (e.g. \"1-5\", \"3\", \"10-20\", 1-indexed, max 20 pages) to extract a \
page range as text regardless of model capabilities.\n\
- If you are unsure the file exists, run list_files or grep first.";

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The path of the file to read, relative to the project root \
                            (e.g. \"src/main.rs\" or \"Cargo.toml\"). Do not pass an absolute \
                            path."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based line number to start reading from \
                            (default 1). Text files only. Use it to page through a large file: \
                            when a read is truncated, the truncation note names the exact \
                            start_line that continues where it left off - pass that value back to \
                            read the next window."
                    },
                    "pages": {
                        "type": "string",
                        "description": "Optional: for PDF files, the page range to extract as text \
                            (e.g. \"1-5\", \"3\", \"10-20\"). Pages are 1-indexed. Max 20 pages per \
                            request. Open-ended ranges like \"3-\" are not supported. When \
                            provided, PDF content is extracted as text regardless of model \
                            capabilities."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    /// The text branch (unchanged from the pre-3b behavior): a window of lines
    /// from `start_line` on. `run_rich` handles every non-text kind, so a call
    /// that reaches `run` directly (the default `run_rich` path, or a test) still
    /// reads text files exactly as before.
    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = params::read_path(input)?;
        let start = params::start_line(input)?;
        let abs = crate::tool::path::resolve_path(path, &ctx.root)?;
        let content = media::read_from(&abs, path, start)?;
        // A text read is cacheable; it is FULL when it starts at the top (no
        // windowing). The result_cap truncation the Tools dispatch applies after
        // this return is not visible here - matching qwen, whose read cache
        // records at the read site (F6, ADR-0060). notebook_edit, the only
        // enforcement consumer, never edits a plain-text read anyway.
        cache::record_read(ctx, &abs, start == 1, true);
        Ok(content)
    }

    /// The multimodal dispatch (ADR-0059): detect the file kind and emit the
    /// right block list. Text/svg/notebook/text-PDF become one Text block; an
    /// image or native PDF becomes a media block when the Model accepts it, else
    /// the verbatim placeholder Text block (read-time modality degrade).
    async fn run_rich(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let path = params::read_path(input)?.to_string();
        let start = params::start_line(input)?;
        let pages = params::pages(input)?;

        // Resolve + read a head sample and stat once, confined to the root.
        let abs = crate::tool::path::resolve_path(&path, &ctx.root)?;
        params::validate_media_params(&path, &abs, start, pages.as_deref())?;

        let file_type = params::detect_file_type(&abs, &path)?;
        let display_name = params::basename(&path);

        // Each branch produces its output and whether the read was FULL (the
        // whole current content, not windowed or internally truncated). After a
        // successful read we record into the Run's read cache (F6, ADR-0060):
        // `cacheable` is whether the result is a single Text block (a media /
        // native-PDF block is not text-cacheable), `full` is per-branch.
        let (output, full) = match file_type {
            FileType::Text => {
                let text = media::read_from(&abs, &path, start)?;
                (ToolOutput::text(text), start == 1)
            }
            FileType::Svg => (ToolOutput::text(media::read_svg(&abs, &path)?), true),
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
    fn spec_requires_path_and_advertises_pages() {
        let spec = ReadFile.spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.input_schema["required"], json!(["path"]));
        assert!(spec.input_schema["properties"]["pages"].is_object());
    }

    // ---- text branch UNCHANGED --------------------------------------------

    #[tokio::test]
    async fn reads_a_file_relative_to_the_project_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hi there\n").unwrap();
        assert_eq!(
            run(json!({"path": "hello.txt"}), &ctx(tmp.path())).await,
            Ok("hi there\n".into())
        );
    }

    #[tokio::test]
    async fn reads_a_nested_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub/dir")).unwrap();
        std::fs::write(tmp.path().join("sub/dir/a.txt"), "nested").unwrap();
        assert_eq!(
            run(json!({"path": "sub/dir/a.txt"}), &ctx(tmp.path())).await,
            Ok("nested".into())
        );
    }

    #[tokio::test]
    async fn returns_large_files_whole() {
        let tmp = TempDir::new().unwrap();
        let content = "a".repeat(50_123);
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();
        assert_eq!(
            run(json!({"path": "big.txt"}), &ctx(tmp.path())).await,
            Ok(content)
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"path": "nope.txt"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("nope.txt"));
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn reading_a_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();
        let err = run(json!({"path": "somedir"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("somedir"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"path": "../../etc/passwd"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
        assert_eq!(
            run(json!({"path": "/etc/passwd"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_path_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(
            crate::tools::execute("read_file", &json!({}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute("read_file", &json!({"path": 42}), &c)
                .await
                .is_error
        );
    }

    // ---- start_line windowing UNCHANGED -----------------------------------

    #[tokio::test]
    async fn returns_the_file_from_start_line_on() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(
            run(
                json!({"path": "lines.txt", "start_line": 3}),
                &ctx(tmp.path())
            )
            .await,
            Ok("three\nfour\n".into())
        );
    }

    #[tokio::test]
    async fn start_line_1_returns_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();
        assert_eq!(
            run(
                json!({"path": "lines.txt", "start_line": 1}),
                &ctx(tmp.path())
            )
            .await,
            Ok("one\ntwo\n".into())
        );
    }

    #[tokio::test]
    async fn the_last_line_is_reachable_with_or_without_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nl.txt"), "one\ntwo\n").unwrap();
        std::fs::write(tmp.path().join("no_nl.txt"), "one\ntwo").unwrap();
        assert_eq!(
            run(json!({"path": "nl.txt", "start_line": 2}), &ctx(tmp.path())).await,
            Ok("two\n".into())
        );
        assert_eq!(
            run(
                json!({"path": "no_nl.txt", "start_line": 2}),
                &ctx(tmp.path())
            )
            .await,
            Ok("two".into())
        );
    }

    #[tokio::test]
    async fn a_start_line_past_the_end_is_an_error_naming_the_line_count() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();
        let err = run(
            json!({"path": "lines.txt", "start_line": 3}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("past the end"));
        assert!(err.contains("2 lines"));
    }

    #[tokio::test]
    async fn a_non_integer_or_non_positive_start_line_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        let err = run(json!({"path": "x.txt", "start_line": 0}), &c)
            .await
            .unwrap_err();
        assert!(err.contains("start_line"));
        assert!(
            run(json!({"path": "x.txt", "start_line": "3"}), &c)
                .await
                .is_err()
        );
    }

    // ---- run_rich text stays one Text block --------------------------------

    #[tokio::test]
    async fn run_rich_on_text_is_one_text_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hi.txt"), "hello\n").unwrap();
        let out = run_rich(json!({"path": "hi.txt"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(out.blocks, vec![ResultBlock::text("hello\n")]);
    }

    // ---- svg branch (Text, 1MB cap) ----------------------------------------

    #[tokio::test]
    async fn svg_is_read_as_a_text_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("i.svg"), "<svg>hi</svg>").unwrap();
        let out = run_rich(json!({"path": "i.svg"}), &ctx(tmp.path()))
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
        let out = run_rich(json!({"path": "big.svg"}), &ctx(tmp.path()))
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
        let out = run_rich(json!({"path": "nb.ipynb"}), &ctx(tmp.path()))
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
    async fn notebook_with_start_line_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nb.ipynb"), r#"{"cells":[]}"#).unwrap();
        let err = run_rich(
            json!({"path": "nb.ipynb", "start_line": 2}),
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
            json!({"path": "nb.ipynb", "pages": "1-2"}),
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
            json!({"path": "pic.png"}),
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
        let out = run_rich(json!({"path": "pic.png"}), &ctx(tmp.path()))
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
            json!({"path": "huge.png"}),
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
            json!({"path": "doc.pdf"}),
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
            json!({"path": "doc.pdf", "pages": "1-2"}),
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
        let out = run_rich(json!({"path": "doc.pdf"}), &ctx(tmp.path())).await;
        // pdf=false so no Document block; text path (Ok text or Failed error).
        match out {
            Ok(o) => assert!(matches!(o.blocks[0], ResultBlock::Text { .. })),
            Err(e) => assert!(e.contains("Cannot extract text from PDF")),
        }
    }
}
