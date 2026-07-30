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

/// Strip ANSI escape sequences (qwen `stripAnsi`): terminal control codes
/// ipykernel writes into stream/traceback output must not leak into the prompt.
/// Covers the four families notebook.ts matches - CSI, OSC, DCS/APC/PM/SOS, and
/// lone C1 Fe two-byte escapes. Each escape family is skipped by a helper that
/// returns the index just past the sequence; a plain char passes through.
fn strip_ansi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(next) = escape_skip(&chars, i) {
            i = next;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// If an escape sequence starts at `i`, the index just past it; else `None` (a
/// plain char the caller emits). Dispatches on the byte after `ESC` to the right
/// family skipper.
fn escape_skip(chars: &[char], i: usize) -> Option<usize> {
    if chars[i] != '\u{1b}' || i + 1 >= chars.len() {
        return None;
    }
    match chars[i + 1] {
        '[' => Some(skip_csi(chars, i)),
        ']' => Some(skip_osc(chars, i)),
        'P' | '_' | '^' | 'X' => Some(skip_string_terminated(chars, i)),
        '\u{40}'..='\u{5a}' | '\u{5c}'..='\u{5f}' => Some(skip_c1(i)),
        _ => None,
    }
}

/// CSI: `ESC [` params intermediates final. Returns the index past the final
/// byte (`0x40..=0x7e`), consuming the whole sequence.
fn skip_csi(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j < chars.len() && matches!(chars[j], '\u{30}'..='\u{3f}') {
        j += 1;
    }
    while j < chars.len() && matches!(chars[j], '\u{20}'..='\u{2f}') {
        j += 1;
    }
    if j < chars.len() && matches!(chars[j], '\u{40}'..='\u{7e}') {
        j += 1;
    }
    j
}

/// OSC: `ESC ]` ... terminated by BEL (`0x07`) or ST (`ESC \`). Returns the
/// index past whichever terminator ended it (or end-of-input if unterminated).
fn skip_osc(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j < chars.len() && chars[j] != '\u{07}' && chars[j] != '\u{1b}' {
        j += 1;
    }
    if j < chars.len() && chars[j] == '\u{07}' {
        j + 1
    } else if j + 1 < chars.len() && chars[j] == '\u{1b}' && chars[j + 1] == '\\' {
        j + 2
    } else {
        j
    }
}

/// DCS / APC / PM / SOS: `ESC P/_/^/X` ... terminated by ST (`ESC \`). Returns
/// the index past the ST, or end-of-input when the sequence is unterminated.
fn skip_string_terminated(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < chars.len() && !(chars[j] == '\u{1b}' && chars[j + 1] == '\\') {
        j += 1;
    }
    if j + 1 < chars.len() {
        j + 2
    } else {
        chars.len()
    }
}

/// A lone two-byte C1 Fe escape (`ESC` + a `0x40..=0x5f` byte, minus `[` which
/// the CSI arm owns): always exactly two chars wide.
fn skip_c1(i: usize) -> usize {
    i + 2
}

/// A `text` field on an output rendered to a string (qwen `processOutputText`).
fn output_text(text: &Option<Source>) -> String {
    text.as_ref().map(Source::normalize).unwrap_or_default()
}

/// IANA MIME-type grammar guard (qwen `MIME_TYPE_RE`/`sanitizeMimeTypes`):
/// accept a permissive ASCII-printable `type/subtree.subtype(+suffix)` shape and
/// reject anything else, so an attacker-authored notebook cannot break out of
/// the `[non-text output: ...]` placeholder with prompt-shaped `data` keys.
fn is_valid_mime(key: &str) -> bool {
    fn token(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c))
    }
    let (ty, rest) = match key.split_once('/') {
        Some(parts) => parts,
        None => return false,
    };
    if !token(ty) {
        return false;
    }
    // subtree.subtype optionally followed by a single `+suffix`.
    match rest.split_once('+') {
        Some((base, suffix)) => token(base) && token(suffix),
        None => token(rest),
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
mod tests {
    use super::*;

    fn parse(raw: &str) -> Notebook {
        Notebook::parse(raw).unwrap()
    }

    #[test]
    fn empty_notebook_is_the_verbatim_placeholder() {
        assert_eq!(format(&parse(r#"{"cells":[]}"#)).content, "(empty notebook)");
    }

    #[test]
    fn code_cell_with_execution_count_and_output() {
        let nb = parse(
            r#"{"metadata":{"language_info":{"name":"python"}},
               "cells":[{"cell_type":"code","execution_count":5,"source":"print(1)",
                         "outputs":[{"output_type":"stream","text":"1\n"}]}]}"#,
        );
        let out = format(&nb).content;
        assert_eq!(
            out,
            "Jupyter Notebook (python, 1 cells)\n\n\
--- Code Cell cell-0 [5] ---\n\
```python\n\
print(1)\n\
```\n\
Output:\n\
1\n"
        );
    }

    #[test]
    fn code_cell_without_execution_count_has_no_label() {
        let nb = parse(
            r#"{"cells":[{"cell_type":"code","execution_count":null,"source":"x=1"}]}"#,
        );
        let out = format(&nb).content;
        assert!(out.contains("--- Code Cell cell-0 ---"));
        assert!(!out.contains('['));
    }

    #[test]
    fn markdown_and_raw_cells_use_their_own_markers() {
        let nb = parse(
            r##"{"cells":[{"cell_type":"markdown","source":"# Title","id":"md1"},
                        {"cell_type":"raw","source":"raw text"}]}"##,
        );
        let out = format(&nb).content;
        assert!(out.contains("--- Markdown Cell md1 ---\n# Title"));
        assert!(out.contains("--- Raw Cell cell-1 ---\nraw text"));
    }

    #[test]
    fn execute_result_text_plain_is_rendered_ansi_stripped() {
        let nb = parse(
            r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"execute_result",
                           "data":{"text/plain":"\u001b[31mred\u001b[0m"}}]}]}"#,
        );
        let out = format(&nb).content;
        assert!(out.contains("Output:\nred"));
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn non_text_output_surfaces_a_sanitized_mime_placeholder() {
        let nb = parse(
            r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"display_data",
                           "data":{"image/png":"...","not a mime":"x"}}]}]}"#,
        );
        let out = format(&nb).content;
        assert!(out.contains("[non-text output: image/png]"));
        assert!(!out.contains("not a mime"));
    }

    #[test]
    fn error_output_joins_ename_evalue_traceback() {
        let nb = parse(
            r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"error","ename":"ValueError","evalue":"bad",
                           "traceback":["\u001b[31mline1","line2"]}]}]}"#,
        );
        let out = format(&nb).content;
        assert!(out.contains("ValueError: bad: line1\nline2"));
    }

    #[test]
    fn large_code_output_is_cut_with_the_verbatim_marker() {
        let big = "a".repeat(LARGE_OUTPUT_THRESHOLD + 50);
        let raw = format!(
            r#"{{"cells":[{{"cell_type":"code","source":"x",
               "outputs":[{{"output_type":"stream","text":{}}}]}}]}}"#,
            serde_json::to_string(&big).unwrap()
        );
        let out = format(&parse(&raw)).content;
        assert!(out.contains(&format!(
            "... [output truncated, total {} chars. Use shell: cat <notebook_path> | jq '.cells[0].outputs']",
            LARGE_OUTPUT_THRESHOLD + 50
        )));
    }

    #[test]
    fn whole_notebook_cell_budget_truncates_with_the_verbatim_marker() {
        // Each cell's source is big enough that a handful blow the 100k budget.
        let big_source = serde_json::to_string(&"z".repeat(30_000)).unwrap();
        let cell = format!(r#"{{"cell_type":"code","source":{big_source}}}"#);
        let cells: Vec<String> = std::iter::repeat_n(cell, 5).collect();
        let raw = format!(r#"{{"cells":[{}]}}"#, cells.join(","));
        let read = format(&parse(&raw));
        assert!(read.is_truncated);
        assert!(read.content.contains("remaining cells truncated, total 5 cells"));
        assert!(read.content.contains("jq '.cells["));
    }

    #[test]
    fn strip_ansi_removes_csi_color_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        // A bare ESC with no recognized family passes through untouched.
        assert_eq!(strip_ansi("a\u{1b}"), "a\u{1b}");
    }

    #[test]
    fn strip_ansi_removes_osc_with_bel_and_st_terminators() {
        // OSC terminated by BEL (0x07): a window-title set.
        assert_eq!(
            strip_ansi("\u{1b}]0;title\u{07}text"),
            "text"
        );
        // OSC terminated by ST (ESC \\): a hyperlink open.
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"),
            "link"
        );
        // An unterminated OSC consumes to end-of-input.
        assert_eq!(strip_ansi("keep\u{1b}]0;never-ends"), "keep");
    }

    #[test]
    fn strip_ansi_removes_dcs_apc_pm_sos_string_sequences() {
        // DCS (ESC P) terminated by ST.
        assert_eq!(strip_ansi("\u{1b}Pq...data...\u{1b}\\after"), "after");
        // APC (ESC _) terminated by ST.
        assert_eq!(strip_ansi("pre\u{1b}_payload\u{1b}\\post"), "prepost");
        // An unterminated DCS consumes to end-of-input.
        assert_eq!(strip_ansi("head\u{1b}Pno-end"), "head");
    }

    #[test]
    fn strip_ansi_removes_a_lone_c1_fe_escape() {
        // ESC + a 0x40..=0x5f byte (here `M`, reverse line feed) is a two-char
        // C1 escape, stripped whole.
        assert_eq!(strip_ansi("a\u{1b}Mb"), "ab");
    }

    #[test]
    fn read_surfaces_parse_errors() {
        assert!(read_with_meta("not json").is_err());
        assert!(
            read_with_meta("{}")
                .unwrap_err()
                .contains("missing cells array")
        );
    }
}
