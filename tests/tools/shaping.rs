
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
