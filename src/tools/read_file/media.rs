//! read_file's per-kind readers: the text-window slicer plus the svg, image, and
//! PDF branches that emit blocks. Split out of `read_file.rs` so the dispatch
//! module reads as a switch over these readers rather than carrying every
//! reader's body inline.

use base64::Engine;

use super::params::Window;
use super::pdf;
use super::reader::SourceFile;
use crate::content::{Modalities, ResultBlock, unsupported_modality_placeholder};
use crate::tool::ToolOutput;
use crate::tool::path::{FileError, file_error};

/// The base64-after-encoding size guard (qwen's `9.9` MB, margin under 10MB).
const MAX_BASE64_MB: f64 = 9.9;
/// The SVG-as-text size cap (qwen `SVG_MAX_SIZE_BYTES`, 1MB). Widened to
/// `pub(super)` so the read_file tests can size an over-cap SVG against it.
pub(super) const SVG_MAX_SIZE_BYTES: u64 = 1024 * 1024;
/// Bytes per megabyte, for the MB size math the base64 and PDF-extraction guards
/// read against (`size / BYTES_PER_MB`).
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

// ---- text branch (String) ---------------------------------------------------

/// Read a text file and apply qwen's `offset` / `limit` line window
/// (`readFileWithLineAndLimit` in fileUtils.ts). When the window is not the
/// whole file, prepend qwen's `"Showing lines X-Y of Z total lines."` notice
/// (read-file.ts), so a partial read is self-describing. Returns the windowed
/// content and whether it was a truncated (partial) read.
pub(super) fn read_from(source: &SourceFile<'_>, window: Window) -> Result<(String, bool), String> {
    match std::fs::read_to_string(source.abs) {
        Ok(content) => Ok(slice_window(&content, window)),
        Err(err) => Err(file_error("read", source.path, FileError::from_io(&err))),
    }
}

/// qwen's text branch (`processSingleFileContent` case 'text' +
/// `readFileWithLineAndLimit`) and the read-file.ts truncation notice, mirroring
/// qwen's variable names so the byte-level result matches:
///
///  - `start_line` = `offset || 0` (pre-clamp; the notice's first number is
///    `start_line + 1`).
///  - The slice bounds are clamped: `actual_start = min(start_line, total)`,
///    `end_line = min(start_line + limit, total)`. `readTextFile` slices
///    `lines[actual_start .. end_line]`.
///  - Every selected line is `trim_end`ed (qwen's
///    `content.split('\n').map(line => line.trimEnd())`, fileUtils.ts:1029),
///    stripping ALL trailing whitespace, not just newlines.
///  - `lines_included = selected_lines.len()` (qwen's no-char-limit branch;
///    the char cap is suspenders' Result Cap, applied later by Shaping, so this
///    tool is always in qwen's `else` branch). `actual_end_line = start_line +
///    lines_included`.
///  - `linesShown = [start_line + 1, actual_end_line]` feeds the notice, so both
///    numbers derive from the SAME clamp, not the raw slice bounds.
///
/// A read is truncated when `start_line > 0 || actual_end_line < total`
/// (qwen's `contentRangeTruncated`); a truncated read is prefixed with
/// `"Showing lines {start}-{end} of {total} total lines.\n\n---\n\n"`.
fn slice_window(content: &str, window: Window) -> (String, bool) {
    let lines: Vec<&str> = content.split('\n').collect();
    // `content.split('\n')` matches qwen's `content.split('\n')`: a trailing
    // newline yields a final empty element that IS counted in
    // `originalLineCount`, exactly as qwen does.
    let total = lines.len() as u64;

    // The pre-clamp start (qwen `startLine = offset || 0`) drives the notice's
    // first number; the clamp drives the slice.
    let start_line = window.offset.unwrap_or(0);
    let actual_start = start_line.min(total);
    let end_line = match window.limit {
        Some(limit) => start_line.saturating_add(limit).min(total),
        None => total,
    };

    // qwen windows in TWO stages: `readTextFile` slices + joins the lines into a
    // `content` string, THEN `processSingleFileContent` re-splits that string on
    // '\n' and trims each line. The re-split matters at the empty-window edge:
    // an empty slice joins to `""`, and `"".split('\n')` is `[""]` (length 1),
    // not `[]` - so `lines_included` is 1 there, matching qwen's
    // `selectedLines.length === 1`. Reproduce the same two stages.
    let windowed = lines[actual_start as usize..end_line as usize].join("\n");
    let selected: Vec<&str> = windowed.split('\n').map(|line| line.trim_end()).collect();
    let body = selected.join("\n");

    // qwen's no-char-limit branch: linesIncluded = selectedLines.length.
    let lines_included = selected.len() as u64;
    let actual_end_line = start_line.saturating_add(lines_included);

    // Truncated iff the window does not cover the whole file (qwen's
    // `contentRangeTruncated = startLine > 0 || actualEndLine < originalLineCount`).
    let truncated = start_line > 0 || actual_end_line < total;
    if truncated {
        let notice = format!(
            "Showing lines {}-{} of {} total lines.\n\n---\n\n",
            start_line + 1,
            actual_end_line,
            total
        );
        (format!("{notice}{body}"), true)
    } else {
        (body, false)
    }
}

pub(super) fn read_text(source: &SourceFile<'_>) -> Result<String, String> {
    std::fs::read_to_string(source.abs)
        .map_err(|err| file_error("read", source.path, FileError::from_io(&err)))
}

// ---- svg branch (Text, 1MB cap) ---------------------------------------------

/// SVG is read as text (qwen returns `'svg'` and reads it with the text reader),
/// capped at 1MB (qwen `SVG_MAX_SIZE_BYTES`) with the verbatim skip message. The
/// skip message names the `display_name` (basename), matching qwen's
/// display-path wording without leaking the absolute path.
pub(super) fn read_svg(source: &SourceFile<'_>) -> Result<String, String> {
    let size = std::fs::metadata(source.abs)
        .map(|m| m.len())
        .map_err(|err| file_error("read", source.path, FileError::from_io(&err)))?;
    if size > SVG_MAX_SIZE_BYTES {
        return Ok(format!(
            "Cannot display content of SVG file larger than 1MB: {}",
            source.display_name()
        ));
    }
    read_text(source)
}

// ---- image branch (media block or read-time degrade) ------------------------

/// An image rides as a media block when the Model accepts image input, else it
/// degrades to the VERBATIM unsupported-modality placeholder Text block at read
/// time (P3 3b). A base64 payload past 9.9MB is a hard error (qwen's data-URI
/// guard).
pub(super) fn read_image(
    source: &SourceFile<'_>,
    modalities: Modalities,
) -> Result<ToolOutput, String> {
    if !modalities.image {
        return Ok(ToolOutput::text(unsupported_modality_placeholder(
            "image",
            source.display_name(),
        )));
    }

    let bytes = std::fs::read(source.abs)
        .map_err(|err| file_error("read", source.path, FileError::from_io(&err)))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if let Some(err) = base64_too_large(&data, source.path) {
        return Err(err);
    }
    Ok(ToolOutput {
        blocks: vec![ResultBlock::Image {
            mime: super::detect::image_mime(source.path).to_string(),
            data,
        }],
        is_error: false,
        artifacts: std::collections::HashMap::new(),
    })
}

// ---- pdf branch (native document block, or pdftotext text) ------------------

/// A PDF with no `pages` on a PDF-capable Model rides as a native Document block;
/// otherwise (a `pages` request, or a Model without PDF support) it is extracted
/// to text via pdftotext. The oversized-for-extraction guard and the base64
/// guard mirror qwen's `processSingleFileContent` PDF arm.
pub(super) async fn read_pdf(
    source: &SourceFile<'_>,
    pages: Option<&str>,
    modalities: Modalities,
) -> Result<ToolOutput, String> {
    let native = pages.is_none() && modalities.pdf;

    if native {
        let bytes = std::fs::read(source.abs)
            .map_err(|err| file_error("read", source.path, FileError::from_io(&err)))?;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        if let Some(err) = base64_too_large(&data, source.path) {
            return Err(err);
        }
        return Ok(ToolOutput {
            blocks: vec![ResultBlock::Document {
                mime: "application/pdf".to_string(),
                data,
            }],
            is_error: false,
            artifacts: std::collections::HashMap::new(),
        });
    }

    // Text-extraction path: guard the on-disk size first (qwen PDF_EXTRACTION_MAX_MB).
    let size = std::fs::metadata(source.abs)
        .map(|m| m.len())
        .map_err(|err| file_error("read", source.path, FileError::from_io(&err)))?;
    let size_mb = size as f64 / BYTES_PER_MB;
    if size_mb > pdf::PDF_EXTRACTION_MAX_MB {
        return Err(format!(
            "PDF file is too large for text extraction: {size_mb:.2}MB exceeds the {:.0}MB \
limit. Use the 'pages' parameter to read a narrower range, or split the document.",
            pdf::PDF_EXTRACTION_MAX_MB
        ));
    }

    let range = pages.and_then(pdf::parse_page_range);
    match pdf::extract_text(source.abs, range).await {
        pdf::PdfText::Ok(text) => Ok(ToolOutput::text(text)),
        pdf::PdfText::Failed(error) => Err(format!(
            "[Cannot extract text from PDF: \"{}\". {error}]",
            source.display_name()
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
