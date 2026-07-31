//! read_file's parameter decoding, media-param validation, and file-kind
//! detection - the pure input-shaping half of the tool, split out of
//! `read_file.rs` so the dispatch module stays focused on the block-emitting
//! branches. Both `run` (text) and `run_rich` (multimodal) decode through here.

use serde_json::Value;

use super::detect::{self, FileType};
use super::pdf;

/// The maximum PDF page span a single `pages` request may name (qwen's 20-page
/// limit), enforced in [`pages`].
const MAX_PDF_PAGES: u64 = 20;

pub(super) fn read_path(input: &Value) -> Result<&str, String> {
    input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: read_file requires a string \"path\"".to_string())
}

// The model may supply start_line; default 1, and reject non-positive/non-int.
pub(super) fn start_line(input: &Value) -> Result<i64, String> {
    match input.get("start_line") {
        None | Some(Value::Null) => Ok(1),
        // A number binds its `i64` in the arm (no `is_i64` + unwrap): a positive
        // integer wins, and a non-integer or non-positive number falls to the
        // error arm because its `as_i64()` is `None`/`< 1` and the guard fails.
        Some(Value::Number(n)) if n.as_i64().is_some_and(|v| v >= 1) => {
            Ok(n.as_i64().filter(|v| *v >= 1).unwrap_or(1))
        }
        Some(other) => Err(format!(
            "invalid input: start_line must be a positive integer, got {}",
            inspect(other)
        )),
    }
}

// Optional PDF page range, validated with qwen's rules: parseable, not
// open-ended, within the 20-page limit. An empty string is treated as absent.
pub(super) fn pages(input: &Value) -> Result<Option<String>, String> {
    let raw = match input.get("pages") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => {
            return Err(format!(
                "invalid input: pages must be a string, got {}",
                inspect(other)
            ));
        }
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let parsed = pdf::parse_page_range(&raw).ok_or_else(|| {
        format!("Invalid pages parameter: '{raw}'. Use formats like '5' or '1-10'.")
    })?;
    // A closed range names its end; an open-ended range (`3-`) is rejected. The
    // `let else` binds `last` on the closed path so there is no unwrap.
    let Some(last) = parsed.last else {
        return Err(
            "Open-ended page ranges (e.g. '3-') are not supported; specify an \
explicit end page within the 20-page limit (e.g. '3-22')."
                .to_string(),
        );
    };
    if last - parsed.first + 1 > MAX_PDF_PAGES {
        return Err("Pages range exceeds maximum of 20 pages per request.".to_string());
    }
    Ok(Some(raw))
}

// ---- validation of media params against the file kind -----------------------

/// Reject start_line/pages on kinds that do not window (qwen's ipynb rejections
/// generalized to the media kinds): notebooks / images / PDFs are always read in
/// full, so a windowing param on them is a hard error naming the reason.
pub(super) fn validate_media_params(
    path: &str,
    abs: &std::path::Path,
    start: i64,
    pages: Option<&str>,
) -> Result<(), String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext == "ipynb" {
        if start != 1 {
            return Err(
                "start_line is not supported for Jupyter notebook (.ipynb) files. \
Notebooks are always read in full with structured cell output."
                    .to_string(),
            );
        }
        if pages.is_some() {
            return Err(
                "pages is not supported for Jupyter notebook (.ipynb) files. \
Notebooks are always read in full with structured cell output."
                    .to_string(),
            );
        }
        return Ok(());
    }

    // For image / PDF, a start_line makes no sense (the file is not read by
    // line). Detect the kind from a head sample so a mislabeled binary is caught
    // too. `pages` is valid only for PDFs.
    if start != 1 || pages.is_some() {
        let head = read_head(abs);
        match detect::detect(path, &head) {
            FileType::Image => {
                if start != 1 {
                    return Err("start_line is not supported for image files. Images are \
read in full."
                        .to_string());
                }
                if pages.is_some() {
                    return Err("pages is only supported for PDF files.".to_string());
                }
            }
            FileType::Pdf => {
                if start != 1 {
                    return Err(
                        "start_line is not supported for PDF files. Use the 'pages' \
parameter to read a specific page range as text."
                            .to_string(),
                    );
                }
            }
            _ => {
                if pages.is_some() {
                    return Err("pages is only supported for PDF files.".to_string());
                }
            }
        }
    }
    Ok(())
}

// ---- file-kind detection (reads a head sample) ------------------------------

pub(super) fn detect_file_type(abs: &std::path::Path, path: &str) -> Result<FileType, String> {
    let head = read_head(abs);
    Ok(detect::detect(path, &head))
}

pub(super) fn read_head(abs: &std::path::Path) -> Vec<u8> {
    use std::io::Read;
    match std::fs::File::open(abs) {
        Ok(mut f) => {
            let mut buf = vec![0u8; 512];
            match f.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    buf
                }
                Err(_) => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    }
}

pub(super) fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

// Elixir `inspect/1` for the values start_line can carry: a quoted string, or
// a JSON-ish rendering for anything else.
fn inspect(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}
