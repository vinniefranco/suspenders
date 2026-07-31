//! read_file's parameter decoding, media-param validation, and file-kind
//! detection - the pure input-shaping half of the tool, split out of
//! `read_file.rs` so the dispatch module stays focused on the block-emitting
//! branches. Both `run` (text) and `run_rich` (multimodal) decode through here.
//!
//! The model-facing contract is qwen v0.16.0 `tools/read-file.ts`: an ABSOLUTE
//! `file_path`, an optional 0-based `offset` line, an optional `limit` line
//! count, and an optional PDF `pages` range. The verbatim validation messages
//! this module returns are copied from that file's `validateToolParamValues`.

use serde_json::Value;

use super::detect::{self, FileType};
use super::pdf;

/// The maximum PDF page span a single `pages` request may name (qwen's 20-page
/// limit), enforced in [`pages`].
const MAX_PDF_PAGES: u64 = 20;

/// The decoded, validated read_file text-window request (qwen's `offset` /
/// `limit`). `offset` is a 0-based start line; `limit` is a line count. Absent
/// values leave the window open (read from the top / to the end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    /// 0-based start line (qwen `offset`). `None` reads from the top.
    pub offset: Option<u64>,
    /// Maximum number of lines to read (qwen `limit`). `None` reads to the end.
    pub limit: Option<u64>,
}

impl Window {
    /// Whether this is a whole-file TEXT window (no offset, no limit). This is
    /// the offset/limit axis ONLY; the PDF `pages` axis is a separate param the
    /// text `Window` does not model. The text `run` path has no `pages`, so
    /// `is_full` is the whole fullness there; the `run_rich` fast-path, which
    /// serves the unchanged placeholder for the text/svg arms, ANDs
    /// `pages.is_none()` at the call site to cover the PDF axis. Keeping the two
    /// axes separate is deliberate: pages is not a line window.
    pub fn is_full(self) -> bool {
        self.offset.is_none() && self.limit.is_none()
    }
}

pub(super) fn file_path(input: &Value) -> Result<String, String> {
    let raw = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: read_file requires a string \"file_path\"".to_string())?;
    // qwen's `unescapePath(params.file_path.trim())` (read-file.ts): trim
    // surrounding whitespace and strip shell-escaping backslashes BEFORE the
    // absolute-path check and before the path is echoed back, so a padded or
    // shell-escaped path validates and displays in its clean form.
    Ok(crate::tool::path::unescape_and_trim(raw))
}

/// Decode the `offset` / `limit` window, rejecting non-integers and the
/// out-of-range values with qwen's verbatim messages (read-file.ts
/// `validateToolParamValues`): "Offset must be a non-negative number" (offset
/// < 0) and "Limit must be a positive number" (limit <= 0).
pub(super) fn window(input: &Value) -> Result<Window, String> {
    let offset = optional_number("offset", input, "Offset must be a non-negative number", false)?;
    let limit = optional_number("limit", input, "Limit must be a positive number", true)?;
    Ok(Window { offset, limit })
}

/// Decode an optional non-negative integer param. A negative integer (offset) or
/// a non-positive integer (limit, when `positive`) is rejected with `oob_msg`
/// (qwen's per-param verbatim message); a non-integer number or a non-number is
/// a structured input error. Absent / null yields `None`.
fn optional_number(
    key: &str,
    input: &Value,
    oob_msg: &str,
    positive: bool,
) -> Result<Option<u64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_u64() {
            // `limit` must be strictly positive; `offset` may be 0.
            Some(0) if positive => Err(oob_msg.to_string()),
            Some(v) => Ok(Some(v)),
            // A negative integer fails `as_u64` but parses as `i64` -> qwen's
            // out-of-range message; a float fails both -> structured error.
            None if n.as_i64().is_some() => Err(oob_msg.to_string()),
            None => Err(format!("invalid input: {key} must be an integer, got {n}")),
        },
        Some(other) => Err(format!(
            "invalid input: {key} must be an integer, got {}",
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

/// Reject offset/limit/pages on kinds that do not window the way text does.
/// Notebooks are always read in full (qwen's `.ipynb` rejections, verbatim);
/// images / PDFs do not read by line, so a window on them is a hard error naming
/// the reason. `pages` is valid only for PDFs.
pub(super) fn validate_media_params(
    path: &str,
    abs: &std::path::Path,
    window: Window,
    pages: Option<&str>,
) -> Result<(), String> {
    let windowed = window.offset.is_some() || window.limit.is_some();
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext == "ipynb" {
        // qwen read-file.ts: the verbatim `.ipynb` rejections.
        if windowed {
            return Err(
                "offset and limit are not supported for Jupyter notebook (.ipynb) files. \
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

    // For image / PDF, a line window makes no sense (the file is not read by
    // line). Detect the kind from a head sample so a mislabeled binary is caught
    // too. `pages` is valid only for PDFs.
    if windowed || pages.is_some() {
        let head = read_head(abs);
        match detect::detect(path, &head) {
            FileType::Image => {
                if windowed {
                    return Err("offset and limit are not supported for image files. Images \
are read in full."
                        .to_string());
                }
                if pages.is_some() {
                    return Err("pages is only supported for PDF files.".to_string());
                }
            }
            FileType::Pdf => {
                if windowed {
                    return Err(
                        "offset and limit are not supported for PDF files. Use the 'pages' \
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

// Elixir `inspect/1` for the values a param can carry: a quoted string, or
// a JSON-ish rendering for anything else.
fn inspect(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}
