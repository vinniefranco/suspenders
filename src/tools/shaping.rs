//! Applies the Result Cap (CONTEXT.md): the size ceiling one Tool Result may
//! occupy in the Conversation, derived from the Context Budget once per
//! Session.
//!
//! `Tools::run` shapes every Tool Result through here; individual tools carry
//! no size logic. Callers pass the raw Tool Call input; Shaping owns the
//! read_file resume-marker rule (reading `offset` from the input so a cut's
//! resume point is file-absolute). run_command keeps its start AND end (the exit
//! code and last errors live at the end); read_file cuts at a line boundary and
//! its marker names the exact 0-based `offset` that continues the read (qwen's
//! read_file param, ADR-0060); every other tool keeps the start.
//!
//! This Result-Cap fold is a suspenders concern with no qwen equivalent (qwen
//! truncates by a per-line char limit inside the tool). The marker only needs to
//! name the real qwen param the model would pass to continue: `offset` (0-based),
//! plus `limit` to page a bounded window.

use crate::content::ResultBlock;
use crate::voice;
use serde_json::Value;

/// Even a tiny Context Budget gets a usable file read (~1.1k tokens).
const FLOOR_CHARS: usize = 4_000;

/// run_command's head:tail split. The tail carries the signal.
const HEAD_QUARTER: usize = 4;

/// Derives the Result Cap in chars from the Context Budget and the reply
/// reserve: a sixteenth of the Conversation window, at 3.5 chars per token,
/// floored at 4000 chars (`window * 7 / 32`).
pub fn cap_for(context_budget: u64, max_tokens_reserve: u64) -> usize {
    let window_tokens = context_budget.saturating_sub(max_tokens_reserve);
    // window_tokens * 3.5 chars/token, a sixteenth of it: window * 7 / 32.
    ((window_tokens * 7 / 32) as usize).max(FLOOR_CHARS)
}

/// Shapes one Tool Result's block list to the Result Cap (ADR-0059). The cap
/// applies to the TEXT blocks only - the Text blocks are concatenated, cut as
/// before, and returned as one Text block; media blocks (image, PDF document)
/// pass through uncapped, keeping their position relative to the text. `input`
/// is the raw Tool Call input: for read_file Shaping reads its `start_line` so a
/// cut's resume marker is file-absolute; every other tool's input is ignored.
///
/// The common case is a single Text block in, a single (possibly cut) Text block
/// out - byte-identical to the old `&str` shaping.
pub fn shape(
    tool_name: &str,
    input: &Value,
    blocks: Vec<ResultBlock>,
    cap: usize,
) -> Vec<ResultBlock> {
    let (text, media, text_first) = split_text_and_media(blocks);
    let shaped_text = shape_text(tool_name, input, &text, cap);

    // A text-only result (the common case) is one Text block. When media rides,
    // keep the text ahead of the media unless the media led the original list.
    if media.is_empty() {
        return vec![ResultBlock::text(shaped_text)];
    }
    let text_block = ResultBlock::text(shaped_text);
    let mut out = Vec::with_capacity(media.len() + 1);
    if text_first {
        out.push(text_block);
        out.extend(media);
    } else {
        out.extend(media);
        out.push(text_block);
    }
    out
}

/// Splits a block list into (concatenated text, media blocks in order, whether
/// text led the list). Text blocks fold into one string; media blocks (image,
/// document) keep their order. `text_first` is true unless a media block opens
/// the list, so a leading image keeps its place ahead of trailing text.
fn split_text_and_media(blocks: Vec<ResultBlock>) -> (String, Vec<ResultBlock>, bool) {
    let text_first = !matches!(blocks.first(), Some(ResultBlock::Image { .. }))
        && !matches!(blocks.first(), Some(ResultBlock::Document { .. }));
    let mut text = String::new();
    let mut media = Vec::new();
    for block in blocks {
        match block {
            ResultBlock::Text { text: t } => text.push_str(&t),
            media_block => media.push(media_block),
        }
    }
    (text, media, text_first)
}

/// Cuts the concatenated text to the cap (the original `&str` shaping). Text
/// within the cap passes through untouched.
fn shape_text(tool_name: &str, input: &Value, content: &str, cap: usize) -> String {
    let total = content.chars().count();
    if total <= cap {
        content.to_string()
    } else {
        cut(tool_name, content, cap, total, read_offset(tool_name, input))
    }
}

// Only read_file's cut resumes at an absolute line; every other tool's input
// carries nothing Shaping needs. read_file's window param is `offset` (0-based,
// qwen's read_file param); a missing / non-integer offset is treated as 0.
fn read_offset(tool_name: &str, input: &Value) -> Option<i64> {
    if tool_name == "read_file" {
        input.get("offset").and_then(|v| v.as_i64())
    } else {
        None
    }
}

fn cut(
    tool_name: &str,
    content: &str,
    cap: usize,
    total: usize,
    offset: Option<i64>,
) -> String {
    match tool_name {
        "run_shell_command" => {
            let head = cap / HEAD_QUARTER;
            let tail = cap - head;
            format!(
                "{}{}{}",
                char_slice(content, 0, head),
                voice::omitted_middle(total - cap, total),
                char_slice(content, total - tail, tail),
            )
        }
        "read_file" => cut_read_file(content, cap, total, offset),
        _ => head_cut(content, cap, total),
    }
}

// Cut at a line boundary and name the absolute 0-based resume offset. A first
// line wider than the whole cap falls back to the generic head cut. `offset` is
// the read_file call's 0-based offset (qwen param); the shaped content's first
// line is file-absolute line `offset + 1`.
fn cut_read_file(content: &str, cap: usize, total: usize, offset: Option<i64>) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let kept = whole_lines_within(&lines, cap);

    if kept == 0 {
        head_cut(content, cap, total)
    } else {
        // Convert the 0-based offset to a 1-based start line for the file-
        // absolute line numbers the marker displays.
        let start = resolve_offset(offset) + 1;
        let last_shown = start + kept - 1;
        let last_line = start + line_count(content, &lines) - 1;

        let body = lines[..kept].join("\n");
        // The marker names the 0-based offset that continues the read: line
        // `last_shown + 1` (1-based) is offset `last_shown` (0-based).
        format!("{}{}", body, voice::truncated_file(last_shown, last_line))
    }
}

fn head_cut(content: &str, cap: usize, total: usize) -> String {
    format!(
        "{}{}",
        char_slice(content, 0, cap),
        voice::truncated_output(total, cap)
    )
}

// How many whole lines (joined by newlines) fit within cap chars.
fn whole_lines_within(lines: &[&str], cap: usize) -> usize {
    // The -1 start pays back the first line's joining newline (as in baud).
    let mut chars: i64 = -1;
    let mut kept = 0usize;
    for line in lines {
        chars += line.chars().count() as i64 + 1;
        if chars <= cap as i64 {
            kept += 1;
        } else {
            break;
        }
    }
    kept
}

// A trailing newline splits into a final empty string that is not a line.
fn line_count(content: &str, lines: &[&str]) -> usize {
    if content.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    }
}

// The read_file call's 0-based `offset`, clamped to a non-negative usize. A
// missing / negative / non-integer offset is a full read from the top (0).
fn resolve_offset(offset: Option<i64>) -> usize {
    match offset {
        Some(o) if o >= 0 => o as usize,
        _ => 0,
    }
}

// Slice by chars (Elixir String.slice semantics), not bytes.
fn char_slice(s: &str, start: usize, len: usize) -> String {
    s.chars().skip(start).take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A single-Text-block shape, returning the shaped text - the common case,
    // so these text-cut tests read as before.
    fn st(tool: &str, input: &Value, content: &str, cap: usize) -> String {
        let out = shape(tool, input, vec![ResultBlock::text(content)], cap);
        match out.as_slice() {
            [ResultBlock::Text { text }] => text.clone(),
            other => panic!("expected a single Text block, got {other:?}"),
        }
    }

    // ---- cap_for/2 ----

    #[test]
    fn cap_for_is_a_sixteenth_of_the_window_in_chars() {
        // window = 64_000 - 8_000 = 56_000 tokens; 56_000 * 7 / 32 = 12_250.
        assert_eq!(cap_for(64_000, 8_000), 12_250);
    }

    #[test]
    fn cap_for_scales_with_the_context_budget() {
        assert!(cap_for(192_000, 8_000) > cap_for(64_000, 8_000));
    }

    #[test]
    fn cap_for_floors_at_4000() {
        // window = 20_000 - 4_000 = 16_000 -> 3_500 chars, below the floor.
        assert_eq!(cap_for(20_000, 4_000), 4_000);
        assert_eq!(cap_for(1, 1), 4_000);
    }

    // ---- shape/4 ----

    #[test]
    fn content_within_the_cap_passes_through_untouched() {
        assert_eq!(st("read_file", &json!({}), "short", 100), "short");
        assert_eq!(st("run_shell_command", &json!({}), "short", 100), "short");
    }

    #[test]
    fn content_exactly_at_the_cap_passes_through_untouched() {
        let content = "a".repeat(100);
        assert_eq!(st("read_file", &json!({}), &content, 100), content);
    }

    #[test]
    fn head_only_shaping_keeps_first_cap_chars_and_appends_marker() {
        let content = "a".repeat(250);
        assert_eq!(
            st("grep_search", &json!({}), &content, 100),
            format!(
                "{}\n[truncated: output is 250 chars, showing the first 100]",
                "a".repeat(100)
            )
        );
    }

    #[test]
    fn read_file_shaping_cuts_at_line_boundary_and_names_resume() {
        // 30 lines of 9 chars + newline: 10 whole lines fit a 100-char cap
        // (10 * 10 - 1 = 99 chars joined).
        let content = (1..=30)
            .map(|i| format!("line-{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        let shaped = st("read_file", &json!({}), &content, 100);

        // Offset absent -> a full read from the top: line 10 is the last shown,
        // and the resume offset is 10 (0-based), i.e. the next line (11, 1-based).
        assert!(shaped.ends_with(
            "line-0010\n[truncated at line 10 of 30 - continue with read_file offset 10 (0-based) and a limit]"
        ));
        assert!(!shaped.contains("line-0011"));
    }

    #[test]
    fn read_file_shaping_keeps_line_numbers_file_absolute_via_offset() {
        let content = (1..=30)
            .map(|i| format!("line-{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        // offset 20 (0-based) is file line 21; the content the tool returned is
        // the slice from line 21 on, so its 10th line is file line 30, and the
        // resume offset is 30 (0-based) = the next line (31, 1-based).
        let shaped = st("read_file", &json!({"offset": 20}), &content, 100);

        assert!(shaped.contains(
            "[truncated at line 30 of 50 - continue with read_file offset 30 (0-based) and a limit]"
        ));
    }

    #[test]
    fn read_file_shaping_treats_a_missing_or_non_numeric_offset_as_zero() {
        let content = (1..=30)
            .map(|i| format!("line-{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        let resume =
            "[truncated at line 10 of 30 - continue with read_file offset 10 (0-based) and a limit]";

        assert!(st("read_file", &json!({"file_path": "/a.txt"}), &content, 100).contains(resume));
        assert!(st("read_file", &json!({"offset": "20"}), &content, 100).contains(resume));
        // A negative offset is a full read from the top (0).
        assert!(st("read_file", &json!({"offset": -1}), &content, 100).contains(resume));
    }

    #[test]
    fn non_read_file_tools_ignore_an_offset_in_the_input() {
        let content = "a".repeat(250);
        assert_eq!(
            st("grep_search", &json!({"offset": 20}), &content, 100),
            format!(
                "{}\n[truncated: output is 250 chars, showing the first 100]",
                "a".repeat(100)
            )
        );
    }

    #[test]
    fn read_file_shaping_ignores_trailing_newlines_empty_split() {
        let content = (1..=30)
            .map(|i| format!("line-{i:04}\n"))
            .collect::<Vec<_>>()
            .join("");

        let shaped = st("read_file", &json!({}), &content, 100);

        assert!(shaped.contains(
            "[truncated at line 10 of 30 - continue with read_file offset 10 (0-based) and a limit]"
        ));
    }

    #[test]
    fn read_file_first_line_wider_than_cap_falls_back_to_head_cut() {
        let content = "a".repeat(250);
        assert_eq!(
            st("read_file", &json!({}), &content, 100),
            format!(
                "{}\n[truncated: output is 250 chars, showing the first 100]",
                "a".repeat(100)
            )
        );
    }

    #[test]
    fn run_command_shaping_keeps_head_and_tail_with_omission_marker() {
        let content = format!("HEAD{}TAIL", "x".repeat(500));
        let shaped = st("run_shell_command", &json!({}), &content, 100);

        // head = 25 chars, tail = 75 chars, 408 of 508 omitted.
        assert!(shaped.starts_with("HEAD"));
        assert!(shaped.ends_with("TAIL"));
        assert!(shaped.contains("\n[408 of 508 chars omitted from the middle of this output]\n"));

        assert_eq!(
            shaped.chars().count(),
            100 + "\n[408 of 508 chars omitted from the middle of this output]\n"
                .chars()
                .count()
        );
    }
}
