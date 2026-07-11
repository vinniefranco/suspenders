//! Applies the Result Cap (CONTEXT.md): the size ceiling one Tool Result may
//! occupy in the Conversation, derived from the Context Budget once per
//! Session.
//!
//! `Tools::run` shapes every Tool Result through here; individual tools carry
//! no size logic. run_command keeps its start AND end (the exit code and last
//! errors live at the end); read_file cuts at a line boundary and its marker
//! names the exact `start_line` that continues the read; every other tool
//! keeps the start.

use crate::voice;

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

/// Shapes one Tool Result's content to the Result Cap. Content within the cap
/// passes through untouched. `start_line` is read from read_file's cut so the
/// resume point in the marker is absolute; pass `None` for other tools.
pub fn shape(tool_name: &str, content: &str, cap: usize, start_line: Option<i64>) -> String {
    let total = content.chars().count();
    if total <= cap {
        content.to_string()
    } else {
        cut(tool_name, content, cap, total, start_line)
    }
}

fn cut(
    tool_name: &str,
    content: &str,
    cap: usize,
    total: usize,
    start_line: Option<i64>,
) -> String {
    match tool_name {
        "run_command" => {
            let head = cap / HEAD_QUARTER;
            let tail = cap - head;
            format!(
                "{}{}{}",
                char_slice(content, 0, head),
                voice::omitted_middle(total - cap, total),
                char_slice(content, total - tail, tail),
            )
        }
        "read_file" => cut_read_file(content, cap, total, start_line),
        _ => head_cut(content, cap, total),
    }
}

// Cut at a line boundary and name the absolute resume line. A first line wider
// than the whole cap falls back to the generic head cut.
fn cut_read_file(content: &str, cap: usize, total: usize, start_line: Option<i64>) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let kept = whole_lines_within(&lines, cap);

    if kept == 0 {
        head_cut(content, cap, total)
    } else {
        let start = resolve_start_line(start_line);
        let last_shown = start + kept - 1;
        let last_line = start + line_count(content, &lines) - 1;

        let body = lines[..kept].join("\n");
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

fn resolve_start_line(start_line: Option<i64>) -> usize {
    match start_line {
        Some(s) if s >= 1 => s as usize,
        _ => 1,
    }
}

// Slice by chars (Elixir String.slice semantics), not bytes.
fn char_slice(s: &str, start: usize, len: usize) -> String {
    s.chars().skip(start).take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ---- shape/3 ----

    #[test]
    fn content_within_the_cap_passes_through_untouched() {
        assert_eq!(shape("read_file", "short", 100, None), "short");
        assert_eq!(shape("run_command", "short", 100, None), "short");
    }

    #[test]
    fn content_exactly_at_the_cap_passes_through_untouched() {
        let content = "a".repeat(100);
        assert_eq!(shape("read_file", &content, 100, None), content);
    }

    #[test]
    fn head_only_shaping_keeps_first_cap_chars_and_appends_marker() {
        let content = "a".repeat(250);
        assert_eq!(
            shape("grep", &content, 100, None),
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

        let shaped = shape("read_file", &content, 100, None);

        assert!(shaped.ends_with(
            "line-0010\n[truncated at line 10 of 30 - continue with read_file start_line 11]"
        ));
        assert!(!shaped.contains("line-0011"));
    }

    #[test]
    fn read_file_shaping_keeps_line_numbers_file_absolute_via_start_line() {
        let content = (1..=30)
            .map(|i| format!("line-{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        let shaped = shape("read_file", &content, 100, Some(21));

        // The content is the slice from line 21 on, so its 10th line is 30.
        assert!(
            shaped.contains("[truncated at line 30 of 50 - continue with read_file start_line 31]")
        );
    }

    #[test]
    fn read_file_shaping_ignores_trailing_newlines_empty_split() {
        let content = (1..=30)
            .map(|i| format!("line-{i:04}\n"))
            .collect::<Vec<_>>()
            .join("");

        let shaped = shape("read_file", &content, 100, None);

        assert!(
            shaped.contains("[truncated at line 10 of 30 - continue with read_file start_line 11]")
        );
    }

    #[test]
    fn read_file_first_line_wider_than_cap_falls_back_to_head_cut() {
        let content = "a".repeat(250);
        assert_eq!(
            shape("read_file", &content, 100, None),
            format!(
                "{}\n[truncated: output is 250 chars, showing the first 100]",
                "a".repeat(100)
            )
        );
    }

    #[test]
    fn run_command_shaping_keeps_head_and_tail_with_omission_marker() {
        let content = format!("HEAD{}TAIL", "x".repeat(500));
        let shaped = shape("run_command", &content, 100, None);

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
