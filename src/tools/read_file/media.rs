//! read_file's per-kind readers: the text-window slicer plus the svg, image, and
//! PDF branches that emit blocks. Split out of `read_file.rs` so the dispatch
//! module reads as a switch over these readers rather than carrying every
//! reader's body inline.

use base64::Engine;

use super::pdf;
use crate::content::{ResultBlock, unsupported_modality_placeholder};
use crate::tool::path::{FileError, file_error};
use crate::tool::{ToolCtx, ToolOutput};

/// The base64-after-encoding size guard (qwen's `9.9` MB, margin under 10MB).
const MAX_BASE64_MB: f64 = 9.9;
/// The SVG-as-text size cap (qwen `SVG_MAX_SIZE_BYTES`, 1MB).
const SVG_MAX_SIZE_BYTES: u64 = 1024 * 1024;
/// Bytes per megabyte, for the MB size math the base64 and PDF-extraction guards
/// read against (`size / BYTES_PER_MB`).
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

// ---- text branch (String) ---------------------------------------------------

pub(super) fn read_from(abs: &std::path::Path, path: &str, start: i64) -> Result<String, String> {
    match std::fs::read_to_string(abs) {
        Ok(content) => slice_from(&content, start, path),
        Err(err) => Err(file_error("read", path, FileError::from_io(&err))),
    }
}

fn slice_from(content: &str, start: i64, path: &str) -> Result<String, String> {
    if start == 1 {
        return Ok(content.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline splits into a final empty string that is not a line.
    let count = if content.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    } as i64;

    if start > count {
        Err(format!(
            "start_line {start} is past the end of {path} ({count} lines)"
        ))
    } else {
        Ok(lines[(start - 1) as usize..].join("\n"))
    }
}

pub(super) fn read_text(abs: &std::path::Path, path: &str) -> Result<String, String> {
    std::fs::read_to_string(abs).map_err(|err| file_error("read", path, FileError::from_io(&err)))
}

// ---- svg branch (Text, 1MB cap) ---------------------------------------------

/// SVG is read as text (qwen returns `'svg'` and reads it with the text reader),
/// capped at 1MB (qwen `SVG_MAX_SIZE_BYTES`) with the verbatim skip message.
pub(super) fn read_svg(abs: &std::path::Path, path: &str) -> Result<String, String> {
    let size = std::fs::metadata(abs)
        .map(|m| m.len())
        .map_err(|err| file_error("read", path, FileError::from_io(&err)))?;
    if size > SVG_MAX_SIZE_BYTES {
        return Ok(format!(
            "Cannot display content of SVG file larger than 1MB: {path}"
        ));
    }
    read_text(abs, path)
}

// ---- image branch (media block or read-time degrade) ------------------------

/// An image rides as a media block when the Model accepts image input, else it
/// degrades to the VERBATIM unsupported-modality placeholder Text block at read
/// time (P3 3b). A base64 payload past 9.9MB is a hard error (qwen's data-URI
/// guard).
pub(super) fn read_image(
    abs: &std::path::Path,
    path: &str,
    display_name: &str,
    ctx: &ToolCtx,
) -> Result<ToolOutput, String> {
    if !ctx.input_modalities.image {
        return Ok(ToolOutput::text(unsupported_modality_placeholder(
            "image",
            display_name,
        )));
    }

    let bytes =
        std::fs::read(abs).map_err(|err| file_error("read", path, FileError::from_io(&err)))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if let Some(err) = base64_too_large(&data, path) {
        return Err(err);
    }
    Ok(ToolOutput {
        blocks: vec![ResultBlock::Image {
            mime: super::detect::image_mime(path).to_string(),
            data,
        }],
    })
}

// ---- pdf branch (native document block, or pdftotext text) ------------------

/// A PDF with no `pages` on a PDF-capable Model rides as a native Document block;
/// otherwise (a `pages` request, or a Model without PDF support) it is extracted
/// to text via pdftotext. The oversized-for-extraction guard and the base64
/// guard mirror qwen's `processSingleFileContent` PDF arm.
pub(super) async fn read_pdf(
    abs: &std::path::Path,
    path: &str,
    display_name: &str,
    pages: Option<&str>,
    ctx: &ToolCtx,
) -> Result<ToolOutput, String> {
    let native = pages.is_none() && ctx.input_modalities.pdf;

    if native {
        let bytes =
            std::fs::read(abs).map_err(|err| file_error("read", path, FileError::from_io(&err)))?;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        if let Some(err) = base64_too_large(&data, path) {
            return Err(err);
        }
        return Ok(ToolOutput {
            blocks: vec![ResultBlock::Document {
                mime: "application/pdf".to_string(),
                data,
            }],
        });
    }

    // Text-extraction path: guard the on-disk size first (qwen PDF_EXTRACTION_MAX_MB).
    let size = std::fs::metadata(abs)
        .map(|m| m.len())
        .map_err(|err| file_error("read", path, FileError::from_io(&err)))?;
    let size_mb = size as f64 / BYTES_PER_MB;
    if size_mb > pdf::PDF_EXTRACTION_MAX_MB {
        return Err(format!(
            "PDF file is too large for text extraction: {size_mb:.2}MB exceeds the {:.0}MB \
limit. Use the 'pages' parameter to read a narrower range, or split the document.",
            pdf::PDF_EXTRACTION_MAX_MB
        ));
    }

    let range = pages.and_then(pdf::parse_page_range);
    match pdf::extract_text(abs, range).await {
        pdf::PdfText::Ok(text) => Ok(ToolOutput::text(text)),
        pdf::PdfText::Failed(error) => Err(format!(
            "[Cannot extract text from PDF: \"{display_name}\". {error}]"
        )),
    }
}

// ---- shared media helpers ---------------------------------------------------

/// The verbatim data-URI-limit error when a base64 payload exceeds 9.9MB, or
/// `None` when it fits (qwen's `File exceeds the 10MB data URI limit...`).
fn base64_too_large(data: &str, _path: &str) -> Option<String> {
    let mb = data.len() as f64 / BYTES_PER_MB;
    if mb > MAX_BASE64_MB {
        Some(format!(
            "File exceeds the 10MB data URI limit after base64 encoding ({mb:.2}MB encoded)."
        ))
    } else {
        None
    }
}

/// The 1MB SVG-as-text cap, for the read_file test that builds an over-cap SVG.
#[cfg(test)]
pub(super) fn svg_max_size_bytes() -> u64 {
    SVG_MAX_SIZE_BYTES
}
