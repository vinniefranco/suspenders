//! Format a `.ipynb` into the read_file text projection - the VERBATIM port of
//! qwen v0.16.0 `packages/core/src/utils/notebook.ts` (`processOutput`,
//! `processCell`, `readNotebookWithMetadata`). The shared data model lives in
//! [`crate::notebook`]; this file is the formatting rule read_file's notebook
//! branch emits as one Text block.
//!
//! Every literal here (headers, cell markers, truncation markers, char budgets)
//! is copied byte-for-byte from notebook.ts so a Suspenders notebook read is
//! indistinguishable from qwen's.

use crate::notebook::{Cell, CellOutput, Notebook, Source};

use crate::text::strip_ansi;

/// A single code cell's combined-output cut (qwen `LARGE_OUTPUT_THRESHOLD`).
const LARGE_OUTPUT_THRESHOLD: usize = 10_000;
/// The whole-notebook cell-listing budget (qwen `MAX_NOTEBOOK_OUTPUT_CHARS`).
const MAX_NOTEBOOK_OUTPUT_CHARS: usize = 100_000;

/// The result of reading a notebook: the formatted text plus whether the cell
/// listing was truncated (qwen `NotebookReadResult`).
#[derive(Debug)]
pub struct NotebookRead {
    pub content: String,
    pub is_truncated: bool,
}

/// A `text` field on an output rendered to a string (qwen `processOutputText`).
fn output_text(text: &Option<Source>) -> String {
    text.as_ref().map(Source::normalize).unwrap_or_default()
}

/// One MIME grammar token: a non-empty run of the permissive ASCII-printable
/// set qwen's `MIME_TYPE_RE` allows. Pure decision, no calls out.
fn is_mime_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c))
}

/// The `subtree.subtype(+suffix)` half of a MIME type: a token, optionally
/// followed by a single `+suffix` token. Pure decision over already-split parts.
fn is_mime_subtype(rest: &str) -> bool {
    match rest.split_once('+') {
        Some((base, suffix)) => is_mime_token(base) && is_mime_token(suffix),
        None => is_mime_token(rest),
    }
}

/// IANA MIME-type grammar guard (qwen `MIME_TYPE_RE`/`sanitizeMimeTypes`):
/// accept a permissive ASCII-printable `type/subtree.subtype(+suffix)` shape and
/// reject anything else, so an attacker-authored notebook cannot break out of
/// the `[non-text output: ...]` placeholder with prompt-shaped `data` keys.
/// Orchestration only: split on `/`, then delegate each half to a pure predicate.
fn is_valid_mime(key: &str) -> bool {
    match key.split_once('/') {
        Some((ty, rest)) => is_mime_token(ty) && is_mime_subtype(rest),
        None => false,
    }
}

/// Render one cell output to text (qwen `processOutput`). Images are skipped;
/// non-textual outputs surface a sanitized `[non-text output: <mimes>]`
/// placeholder so the model knows a payload was present.
fn process_output(output: &CellOutput) -> String {
    match output.output_type.as_deref() {
        Some("stream") => strip_ansi(&output_text(&output.text)),
        Some("execute_result") | Some("display_data") => {
            if let Some(data) = &output.data {
                match data.get("text/plain") {
                    Some(serde_json::Value::String(s)) => return strip_ansi(s),
                    Some(serde_json::Value::Array(arr)) => {
                        let joined: String = arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .concat();
                        return strip_ansi(&joined);
                    }
                    _ => {}
                }
                let mimes: Vec<&str> = data
                    .keys()
                    .map(String::as_str)
                    .filter(|k| is_valid_mime(k))
                    .collect();
                if !mimes.is_empty() {
                    return format!("[non-text output: {}]", mimes.join(", "));
                }
            }
            String::new()
        }
        Some("error") => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(ename) = &output.ename {
                parts.push(ename.clone());
            }
            if let Some(evalue) = &output.evalue {
                parts.push(evalue.clone());
            }
            if let Some(tb) = &output.traceback
                && !tb.is_empty()
            {
                parts.push(tb.join("\n"));
            }
            strip_ansi(&parts.join(": "))
        }
        _ => String::new(),
    }
}

/// Format a single cell into a readable text block (qwen `processCell`).
fn process_cell(cell: &Cell, index: usize, language: &str) -> String {
    let cell_id = cell.display_id(index);
    let source = cell.source.normalize();
    let mut parts: Vec<String> = Vec::new();

    match cell.cell_type.as_str() {
        "code" => {
            // qwen: `execution_count != null ? [count] : ''`. A `Value` here so
            // an absent count and the JSON literal `null` (a never-run code cell)
            // both yield no label, while a number yields ` [n]`.
            let exec_label = match cell.execution_count.as_ref().and_then(|v| v.as_i64()) {
                Some(n) => format!(" [{n}]"),
                None => String::new(),
            };
            parts.push(format!("--- Code Cell {cell_id}{exec_label} ---"));
            parts.push(format!("```{language}"));
            parts.push(source);
            parts.push("```".to_string());

            let output_texts: Vec<String> = cell
                .outputs
                .iter()
                .flatten()
                .map(process_output)
                .filter(|t| !t.is_empty())
                .collect();
            if !output_texts.is_empty() {
                let mut combined = output_texts.join("\n");
                if combined.chars().count() > LARGE_OUTPUT_THRESHOLD {
                    let total = combined.chars().count();
                    let head: String = combined.chars().take(LARGE_OUTPUT_THRESHOLD).collect();
                    combined = format!(
                        "{head}\n... [output truncated, total {total} chars. \
Use shell: cat <notebook_path> | jq '.cells[{index}].outputs']"
                    );
                }
                parts.push("Output:".to_string());
                parts.push(combined);
            }
        }
        "markdown" => {
            parts.push(format!("--- Markdown Cell {cell_id} ---"));
            parts.push(source);
        }
        "raw" => {
            parts.push(format!("--- Raw Cell {cell_id} ---"));
            parts.push(source);
        }
        _ => {
            parts.push(format!("--- Cell {cell_id} ---"));
            parts.push(source);
        }
    }

    parts.join("\n")
}

/// Format a parsed notebook into the read_file text projection plus whether the
/// cell listing was truncated (qwen `readNotebookWithMetadata`, minus the file
/// read - read_file owns the I/O and BOM strip via [`Notebook::parse`]).
pub fn format(notebook: &Notebook) -> NotebookRead {
    let language = notebook.language();

    if notebook.cells.is_empty() {
        return NotebookRead {
            content: "(empty notebook)".to_string(),
            is_truncated: false,
        };
    }

    let header = format!(
        "Jupyter Notebook ({language}, {} cells)",
        notebook.cells.len()
    );
    let mut cell_texts: Vec<String> = Vec::new();
    let mut total_length = header.chars().count();
    let mut is_truncated = false;

    for (i, cell) in notebook.cells.iter().enumerate() {
        let cell_text = process_cell(cell, i, &language);
        total_length += cell_text.chars().count() + 2; // +2 for the "\n\n" separator
        if total_length > MAX_NOTEBOOK_OUTPUT_CHARS {
            is_truncated = true;
            cell_texts.push(format!(
                "... [{} remaining cells truncated, total {} cells. \
Use shell to inspect: cat <path> | jq '.cells[{i}:]']",
                notebook.cells.len() - i,
                notebook.cells.len()
            ));
            break;
        }
        cell_texts.push(cell_text);
    }

    NotebookRead {
        content: format!("{header}\n\n{}", cell_texts.join("\n\n")),
        is_truncated,
    }
}

/// Parse + format a notebook's raw JSON text into the read_file text projection
/// plus the `is_truncated` flag (qwen `readNotebookWithMetadata`, minus the file
/// read - read_file owns the I/O). The single entry read_file's notebook branch
/// calls; a parse error surfaces qwen's "Invalid notebook: ..." wording. The
/// truncation flag lets the read cache record `full = !is_truncated`: a notebook
/// whose rendered output was truncated is NOT a full read, and notebook_edit
/// refuses to cell-edit it (F6, ADR-0060).
pub fn read_with_meta(raw: &str) -> Result<NotebookRead, String> {
    let notebook = Notebook::parse(raw)?;
    Ok(format(&notebook))
}

#[cfg(test)]
#[path = "../../../../tests/tools/read_file/notebook.rs"]
mod tests;
