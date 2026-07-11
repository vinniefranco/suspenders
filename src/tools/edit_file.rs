//! `edit_file(path, old_str, new_str)`: replaces one uniquely-matched span of
//! `old_str` with `new_str` in an existing file.
//!
//! Matching is a two-pass pipeline (DESIGN.md): (1) exact match; (2) only when
//! exact finds zero matches, a whitespace-normalized fallback that compares
//! lines with leading/trailing whitespace stripped and applies the edit
//! preserving the file's real indentation — `new_str` is re-indented by the
//! per-line delta between `old_str` as given and the file's actual span.
//! Whichever pass matches must match exactly one span; more than one is an
//! error reporting the match count. When both passes find zero matches, the
//! error quotes the closest-matching real region of the file (Jaro-ranked).
//!
//! A unique exact match is rejected when it would splice into the middle of an
//! identifier (a boundary of the matched span falls inside a word): a truncated
//! `old_str` like `amount_off(bas` matched as a substring inside
//! `amount_off(base, code)` would silently corrupt the line. The fuzzy pass
//! matches whole trimmed lines, so its span is always line-aligned and cannot
//! split an identifier.

use crate::tool::{FileError, Tool, ToolCtx, ToolSpec, file_error, jaro_distance, with_path};
use serde_json::{Value, json};

pub struct EditFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Exact,
    Fuzzy,
}

// Threshold below which no line is "close enough" to be worth quoting.
const CLOSEST_THRESHOLD: f64 = 0.6;
// Context lines kept on each side of the best line.
const CONTEXT_LINES: usize = 2;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace one occurrence of old_str with new_str in an existing file. \
                old_str must match exactly one location: copy it verbatim from \
                read_file output, including whitespace and indentation, and include \
                enough surrounding lines to make it unique. Zero matches or more \
                than one match is an error. To create a new file, use write_file \
                instead. Make small, targeted edits, one change per call."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of an existing file, relative to the project root."
                    },
                    "old_str": {
                        "type": "string",
                        "description": "Exact text to replace, copied verbatim from the file. Must match \
                            exactly one location; include surrounding lines to disambiguate. \
                            Must not be empty."
                    },
                    "new_str": {
                        "type": "string",
                        "description": "Replacement text. May be empty to delete old_str."
                    }
                },
                "required": ["path", "old_str", "new_str"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: edit_file requires a string \"path\"".to_string())?;
        let old_str = input
            .get("old_str")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: edit_file requires a string \"old_str\"".to_string())?;
        let new_str = input
            .get("new_str")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: edit_file requires a string \"new_str\"".to_string())?;

        check_old_str(old_str)?;
        with_path(path, ctx, |abs| apply_edit(abs, path, old_str, new_str))
    }
}

fn apply_edit(
    abs: &std::path::Path,
    path: &str,
    old_str: &str,
    new_str: &str,
) -> Result<String, String> {
    let content = match std::fs::read_to_string(abs) {
        Ok(c) => c,
        Err(err) => return Err(file_error("read", path, FileError::from_io(&err))),
    };

    let (replaced, pass) = replace(&content, old_str, new_str, path)?;

    match std::fs::write(abs, &replaced) {
        Ok(()) => Ok(success_message(path, pass)),
        Err(err) => Err(file_error("write", path, FileError::from_io(&err))),
    }
}

fn success_message(path: &str, pass: Pass) -> String {
    match pass {
        Pass::Exact => format!("edited {path}"),
        Pass::Fuzzy => format!(
            "edited {path} (matched with whitespace normalized; new_str re-indented to the file)"
        ),
    }
}

fn check_old_str(old_str: &str) -> Result<(), String> {
    if old_str.is_empty() {
        Err("old_str must not be empty; to create a new file use write_file instead".to_string())
    } else {
        Ok(())
    }
}

fn replace(
    content: &str,
    old_str: &str,
    new_str: &str,
    path: &str,
) -> Result<(String, Pass), String> {
    match count_exact(content, old_str) {
        1 => exact_replace(content, old_str, new_str, path),
        0 => fuzzy_replace(content, old_str, new_str, path),
        n => Err(ambiguous(n, path)),
    }
}

fn count_exact(content: &str, old_str: &str) -> usize {
    // Number of non-overlapping matches (String.split length - 1).
    content.matches(old_str).count()
}

fn exact_replace(
    content: &str,
    old_str: &str,
    new_str: &str,
    path: &str,
) -> Result<(String, Pass), String> {
    // char offset of the match start (String.length(before)).
    let byte_pos = content.find(old_str).unwrap();
    let start = content[..byte_pos].chars().count();
    let len = old_str.chars().count();

    if splits_identifier(content, start, len) {
        Err(split_identifier(content, start, len, path))
    } else {
        Ok((content.replacen(old_str, new_str, 1), Pass::Exact))
    }
}

// A match splits an identifier when a boundary falls INSIDE a word: the char
// just before the span and the span's first char are both word chars, or the
// span's last char and the char just after are both word chars.
fn splits_identifier(content: &str, start: usize, len: usize) -> bool {
    let graphemes: Vec<char> = content.chars().collect();
    let span: Vec<char> = graphemes.iter().skip(start).take(len).copied().collect();

    let before = if start > 0 {
        graphemes.get(start - 1).copied()
    } else {
        None
    };
    let after = graphemes.get(start + len).copied();

    let start_break = word_char(before) && word_char(span.first().copied());
    let end_break = word_char(span.last().copied()) && word_char(after);

    (start > 0 && start_break) || end_break
}

fn word_char(c: Option<char>) -> bool {
    matches!(c, Some(ch) if ch.is_ascii_alphanumeric() || ch == '_')
}

fn split_identifier(content: &str, start: usize, len: usize, path: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let span_lines = matched_line_range(&lines, start, len);
    format!(
        "old_str \"{}\" would splice into the middle of a word in {path}. old_str must cover whole words - copy complete lines. The line(s) it matched inside:\n\n{span_lines}\n\nCopy old_str exactly from these lines or from read_file output.",
        quoted_span(content, start, len)
    )
}

fn quoted_span(content: &str, start: usize, len: usize) -> String {
    content.chars().skip(start).take(len).collect()
}

// The file line(s) the matched span [start, start+len) overlaps, joined.
fn matched_line_range(lines: &[&str], start: usize, len: usize) -> String {
    let finish = start + len;
    let mut selected = Vec::new();
    let mut pos = 0usize;
    for line in lines {
        let line_start = pos;
        let line_end = pos + line.chars().count();
        if line_start <= finish && line_end >= start {
            selected.push(*line);
        }
        pos = line_end + 1;
    }
    selected.join("\n")
}

fn ambiguous(n: usize, path: &str) -> String {
    format!(
        "old_str matches {n} locations in {path}; include more surrounding lines to make it unique"
    )
}

const PLAIN_NOT_FOUND_SUFFIX: &str =
    ", even with whitespace normalized; copy it exactly from read_file output";

fn fuzzy_replace(
    content: &str,
    old_str: &str,
    new_str: &str,
    path: &str,
) -> Result<(String, Pass), String> {
    let file_lines: Vec<&str> = content.split('\n').collect();
    let old_lines = split_lines(old_str);

    let matches = window_matches(&file_lines, &old_lines);
    match matches.as_slice() {
        [start] => Ok((
            apply_fuzzy(&file_lines, &old_lines, new_str, *start),
            Pass::Fuzzy,
        )),
        [] => Err(not_found(&file_lines, &old_lines, path)),
        many => Err(ambiguous(many.len(), path)),
    }
}

fn not_found(file_lines: &[&str], old_lines: &[String], path: &str) -> String {
    let plain = format!("old_str not found in {path}{PLAIN_NOT_FOUND_SUFFIX}");
    let first = old_lines.first().map(|s| s.as_str()).unwrap_or("");

    match closest_region(file_lines, first) {
        None => plain,
        Some(region) => format!(
            "old_str not found in {path}, even with whitespace normalized. Closest region in the file:\n\n{region}\n\nCopy old_str exactly from these lines or from read_file output."
        ),
    }
}

fn closest_region(file_lines: &[&str], needle: &str) -> Option<String> {
    if file_lines.is_empty() {
        return None;
    }
    // max_by score, ties go to the earliest index (Elixir max_by keeps first).
    let mut best_idx = 0usize;
    let mut best_score = jaro_distance(needle, file_lines[0].trim());
    for (idx, line) in file_lines.iter().enumerate().skip(1) {
        let score = jaro_distance(needle, line.trim());
        if score > best_score {
            best_score = score;
            best_idx = idx;
        }
    }

    if best_score >= CLOSEST_THRESHOLD {
        let first = best_idx.saturating_sub(CONTEXT_LINES);
        let last = (best_idx + CONTEXT_LINES).min(file_lines.len() - 1);
        Some(file_lines[first..=last].join("\n"))
    } else {
        None
    }
}

// A trailing newline means "up to the end of that line", not "plus an empty
// line" — drop it before line-windowing.
fn split_lines(str: &str) -> Vec<String> {
    let trimmed = str.strip_suffix('\n').unwrap_or(str);
    trimmed.split('\n').map(|s| s.to_string()).collect()
}

fn window_matches(file_lines: &[&str], old_lines: &[String]) -> Vec<usize> {
    let target: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();
    let n = old_lines.len();
    if n == 0 || file_lines.len() < n {
        return Vec::new();
    }

    let trimmed: Vec<&str> = file_lines.iter().map(|l| l.trim()).collect();
    let mut out = Vec::new();
    for start in 0..=(trimmed.len() - n) {
        if trimmed[start..start + n] == target[..] {
            out.push(start);
        }
    }
    out
}

fn apply_fuzzy(file_lines: &[&str], old_lines: &[String], new_str: &str, start: usize) -> String {
    let n = old_lines.len();
    let matched: Vec<&str> = file_lines[start..start + n].to_vec();

    let new_lines: Vec<String> = if new_str.is_empty() {
        Vec::new()
    } else {
        reindent(&split_lines(new_str), old_lines, &matched)
    };

    let mut result: Vec<String> = file_lines[..start].iter().map(|s| s.to_string()).collect();
    result.extend(new_lines);
    result.extend(file_lines[start + n..].iter().map(|s| s.to_string()));
    result.join("\n")
}

// Each new_str line takes the indentation delta of its same-index
// old_str/file line pair; lines beyond old_str's shape reuse the last pair.
fn reindent(new_lines: &[String], old_lines: &[String], matched: &[&str]) -> Vec<String> {
    let pairs: Vec<(String, String)> = old_lines
        .iter()
        .zip(matched.iter())
        .map(|(o, m)| (indent_of(o), indent_of(m)))
        .collect();
    let last = pairs.len().saturating_sub(1);

    new_lines
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let (old_indent, file_indent) = &pairs[idx.min(last)];
            reindent_line(line, old_indent, file_indent)
        })
        .collect()
}

fn indent_of(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn reindent_line(line: &str, old_indent: &str, file_indent: &str) -> String {
    let body = line.trim_start_matches([' ', '\t']);
    if body.is_empty() {
        String::new()
    } else {
        format!(
            "{}{body}",
            shift_indent(&indent_of(line), old_indent, file_indent)
        )
    }
}

fn shift_indent(new_indent: &str, old_indent: &str, file_indent: &str) -> String {
    if let Some(rest) = new_indent.strip_prefix(old_indent) {
        format!("{file_indent}{rest}")
    } else if old_indent.starts_with(new_indent) {
        let outdent = old_indent.chars().count() - new_indent.chars().count();
        let keep = file_indent.chars().count().saturating_sub(outdent);
        file_indent.chars().take(keep).collect()
    } else {
        // Indent characters disagree (tabs vs spaces): no computable delta, so
        // the line takes the matched file line's indentation as-is.
        file_indent.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        EditFile.run(&input, ctx).await
    }

    fn read(root: &std::path::Path, name: &str) -> String {
        std::fs::read_to_string(root.join(name)).unwrap()
    }

    #[test]
    fn spec_requires_path_old_str_new_str() {
        let spec = EditFile.spec();
        assert_eq!(spec.name, "edit_file");
        assert_eq!(
            spec.input_schema["required"],
            json!(["path", "old_str", "new_str"])
        );
    }

    // ---- exact pass ----

    #[tokio::test]
    async fn replaces_a_uniquely_matched_old_str() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "foo bar baz").unwrap();

        let msg = run(
            json!({"path": "a.txt", "old_str": "bar", "new_str": "qux"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(msg.contains("edited a.txt"));
        assert_eq!(read(tmp.path(), "a.txt"), "foo qux baz");
    }

    #[tokio::test]
    async fn matches_old_str_exactly_including_whitespace() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n  :ok\nend\n").unwrap();

        assert!(
            run(
                json!({"path": "code.ex", "old_str": "  :ok\n", "new_str": "  :error\n"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "code.ex"), "def run do\n  :error\nend\n");
    }

    #[tokio::test]
    async fn more_than_one_exact_match_is_an_ambiguity_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "foo bar foo").unwrap();

        let err = run(
            json!({"path": "a.txt", "old_str": "foo", "new_str": "baz"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("2 locations"));
        assert!(err.contains("more surrounding lines"));
        assert_eq!(read(tmp.path(), "a.txt"), "foo bar foo");
    }

    #[tokio::test]
    async fn a_unique_exact_match_wins_even_when_fuzzy_would_be_ambiguous() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "  x = 1\n    x = 1\n").unwrap();

        let msg = run(
            json!({"path": "a.txt", "old_str": "    x = 1", "new_str": "    x = 2"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(!msg.contains("whitespace"));
        assert_eq!(read(tmp.path(), "a.txt"), "  x = 1\n    x = 2\n");
    }

    #[tokio::test]
    async fn new_str_may_be_empty_deletes_old_str() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "keep DROP keep").unwrap();

        assert!(
            run(
                json!({"path": "a.txt", "old_str": "DROP ", "new_str": ""}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.txt"), "keep keep");
    }

    // ---- whitespace-normalized fallback ----

    #[tokio::test]
    async fn matches_despite_wrong_indentation_and_reindents() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n    :ok\nend\n").unwrap();

        let msg = run(
            json!({
                "path": "code.ex",
                "old_str": "def run do\n  :ok\nend",
                "new_str": "def run do\n  :error\nend"
            }),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(msg.contains("whitespace normalized"));
        assert_eq!(read(tmp.path(), "code.ex"), "def run do\n    :error\nend\n");
    }

    #[tokio::test]
    async fn matches_despite_trailing_whitespace_in_the_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x = 1   \ny = 2\n").unwrap();

        assert!(
            run(
                json!({"path": "a.txt", "old_str": "x = 1\ny = 2", "new_str": "x = 3\ny = 2"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.txt"), "x = 3\ny = 2\n");
    }

    #[tokio::test]
    async fn lines_beyond_old_str_shape_keep_relative_indent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n    :ok\nend\n").unwrap();

        assert!(
            run(
                json!({
                    "path": "code.ex",
                    "old_str": "      :ok",
                    "new_str": "      if go?() do\n        :yes\n      end"
                }),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(
            read(tmp.path(), "code.ex"),
            "def run do\n    if go?() do\n      :yes\n    end\nend\n"
        );
    }

    #[tokio::test]
    async fn blank_lines_in_new_str_stay_blank() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n    :ok\nend\n").unwrap();

        assert!(run(
            json!({"path": "code.ex", "old_str": "      :ok", "new_str": "      :a\n\n      :b"}),
            &ctx(tmp.path())
        )
        .await
        .is_ok());
        assert_eq!(
            read(tmp.path(), "code.ex"),
            "def run do\n    :a\n\n    :b\nend\n"
        );
    }

    #[tokio::test]
    async fn reindents_across_tabs_vs_spaces_style_mismatch() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n\t:ok\nend\n").unwrap();

        assert!(
            run(
                json!({"path": "code.ex", "old_str": "  :ok", "new_str": "  :done"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "code.ex"), "def run do\n\t:done\nend\n");
    }

    #[tokio::test]
    async fn when_old_and_new_disagree_on_indent_chars_file_wins() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def run do\n\t:ok\nend\n").unwrap();

        assert!(
            run(
                json!({"path": "code.ex", "old_str": "  :ok", "new_str": "\t:done"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "code.ex"), "def run do\n\t:done\nend\n");
    }

    #[tokio::test]
    async fn empty_new_str_deletes_the_matched_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a\n  DROP\nb\n").unwrap();

        assert!(
            run(
                json!({"path": "a.txt", "old_str": "    DROP", "new_str": ""}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.txt"), "a\nb\n");
    }

    #[tokio::test]
    async fn more_than_one_fuzzy_match_is_an_ambiguity_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "  x = 1\n    x = 1\n").unwrap();

        let err = run(
            json!({"path": "a.txt", "old_str": "      x = 1", "new_str": "      x = 2"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("2 locations"));
        assert_eq!(read(tmp.path(), "a.txt"), "  x = 1\n    x = 1\n");
    }

    // ---- rejects matches that split an identifier ----

    #[tokio::test]
    async fn a_truncated_old_str_that_would_splice_mid_identifier_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("discount.ex"),
            "defmodule Discount do\n  def amount_off(base, code) do\n    base - lookup(code)\n  end\nend\n",
        )
        .unwrap();

        let err = run(
            json!({
                "path": "discount.ex",
                "old_str": "amount_off(bas",
                "new_str": "amount_off(item_subtot"
            }),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("whole words"));
        assert!(err.contains("  def amount_off(base, code) do"));
        assert!(read(tmp.path(), "discount.ex").contains("def amount_off(base, code) do"));
    }

    #[tokio::test]
    async fn a_match_whose_end_boundary_falls_inside_a_word_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ex"), "x = subtotal_amount + 1\n").unwrap();

        let err = run(
            json!({"path": "a.ex", "old_str": "subtotal", "new_str": "total"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("whole words"));
        assert_eq!(read(tmp.path(), "a.ex"), "x = subtotal_amount + 1\n");
    }

    #[tokio::test]
    async fn replacing_a_standalone_word_flanked_by_punctuation_works() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ex"), "fee(subtotal)\n").unwrap();

        assert!(
            run(
                json!({"path": "a.ex", "old_str": "subtotal", "new_str": "grand_total"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.ex"), "fee(grand_total)\n");
    }

    #[tokio::test]
    async fn a_whole_line_edit_is_unaffected_by_the_boundary_guard() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ex"), "  def amount_off(base, code) do\n").unwrap();

        assert!(
            run(
                json!({
                    "path": "a.ex",
                    "old_str": "  def amount_off(base, code) do",
                    "new_str": "  def amount_off(base) do"
                }),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.ex"), "  def amount_off(base) do\n");
    }

    #[tokio::test]
    async fn a_multi_line_edit_is_unaffected_by_the_boundary_guard() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ex"), "def run do\n  :ok\nend\n").unwrap();

        assert!(
            run(
                json!({
                    "path": "a.ex",
                    "old_str": "def run do\n  :ok\nend",
                    "new_str": "def run do\n  :error\nend"
                }),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.ex"), "def run do\n  :error\nend\n");
    }

    #[tokio::test]
    async fn a_match_at_the_start_of_the_file_is_unaffected() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.ex"), "hello world\n").unwrap();

        assert!(
            run(
                json!({"path": "a.ex", "old_str": "hello", "new_str": "goodbye"}),
                &ctx(tmp.path())
            )
            .await
            .is_ok()
        );
        assert_eq!(read(tmp.path(), "a.ex"), "goodbye world\n");
    }

    // ---- misc errors ----

    #[tokio::test]
    async fn empty_old_str_is_an_error_pointing_to_write_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "content").unwrap();

        let err = run(
            json!({"path": "a.txt", "old_str": "", "new_str": "x"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("write_file"));
        assert_eq!(read(tmp.path(), "a.txt"), "content");
    }

    #[tokio::test]
    async fn old_str_not_found_even_fuzzily_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "actual content").unwrap();

        let err = run(
            json!({"path": "a.txt", "old_str": "imaginary", "new_str": "x"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("old_str not found"));
        assert!(err.contains("whitespace normalized"));
        assert_eq!(read(tmp.path(), "a.txt"), "actual content");
    }

    #[tokio::test]
    async fn a_near_miss_old_str_yields_error_with_the_real_line_verbatim() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("cart.ex"),
            "defmodule Cart do\n  def total(items, tax_rate) do\n    sum(items) * tax_rate\n  end\nend\n",
        )
        .unwrap();

        let err = run(
            json!({
                "path": "cart.ex",
                "old_str": "  def total(items) do",
                "new_str": "  def total(items, rate) do"
            }),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("old_str not found"));
        assert!(err.contains("Closest region in the file"));
        assert!(err.contains("  def total(items, tax_rate) do"));
        assert!(err.contains("Copy old_str"));
        assert!(read(tmp.path(), "cart.ex").contains("def total(items, tax_rate)"));
    }

    #[tokio::test]
    async fn a_totally_unrelated_old_str_yields_plain_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "actual content\nmore lines\n").unwrap();

        let err = run(
            json!({"path": "a.txt", "old_str": "zzqx wholly unrelated", "new_str": "x"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("old_str not found"));
        assert!(!err.contains("Closest region in the file"));
    }

    #[tokio::test]
    async fn the_quoted_region_respects_the_line_cap() {
        let tmp = TempDir::new().unwrap();
        let lines: Vec<String> = (1..=60)
            .map(|n| format!("line number {n} filler text here"))
            .collect();
        let body = format!("{}\n", lines.join("\n"));
        std::fs::write(tmp.path().join("big.txt"), body).unwrap();

        let err = run(
            json!({
                "path": "big.txt",
                "old_str": "line number 30 filler text elsewhere",
                "new_str": "changed"
            }),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Closest region in the file"));
        assert!(err.contains("line number 30 filler text here"));

        let quoted = err
            .split("Closest region in the file:")
            .last()
            .unwrap()
            .split("Copy old_str")
            .next()
            .unwrap();
        let quoted_lines: Vec<&str> = quoted.trim().split('\n').collect();
        assert!(quoted_lines.len() <= 6);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"path": "nope.txt", "old_str": "a", "new_str": "b"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(
                json!({"path": "../../etc/passwd", "old_str": "root", "new_str": "x"}),
                &ctx(tmp.path())
            )
            .await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_arguments_are_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(
            crate::tools::execute("edit_file", &json!({"path": "a.txt"}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute(
                "edit_file",
                &json!({"path": "a.txt", "old_str": 1, "new_str": 2}),
                &c
            )
            .await
            .is_error
        );
    }
}
