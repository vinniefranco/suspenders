
use super::*;
use crate::content::ContentBlock;
use serde_json::json;

fn user(blocks: Vec<ContentBlock>) -> Message {
    Message::new(Role::User, blocks)
}

fn assistant(blocks: Vec<ContentBlock>) -> Message {
    Message::new(Role::Assistant, blocks)
}

#[test]
fn run_start_user_message_is_a_boundary() {
    assert!(is_run_start(&user(vec![ContentBlock::text(
        "do the thing"
    )])));
}

#[test]
fn standalone_rider_is_not_a_boundary() {
    // Text appended after a tool result: first block is a ToolResult, so a
    // rider never opens a Run even though it is a User message.
    let rider = user(vec![
        ContentBlock::tool_result("t1", "ok", false),
        ContentBlock::text("and also this"),
    ]);
    assert!(!is_run_start(&rider));
}

#[test]
fn tool_result_first_user_message_is_not_a_boundary() {
    let results = user(vec![ContentBlock::tool_result("t1", "ok", false)]);
    assert!(!is_run_start(&results));
}

#[test]
fn assistant_message_is_not_a_boundary() {
    assert!(!is_run_start(&assistant(vec![ContentBlock::text(
        "thinking out loud"
    )])));
    assert!(!is_run_start(&assistant(vec![ContentBlock::tool_use(
        "t1",
        "read_file",
        json!({}),
    )])));
}

#[test]
fn empty_content_is_not_a_boundary() {
    assert!(!is_run_start(&user(vec![])));
    assert!(!is_run_start(&assistant(vec![])));
}

#[test]
fn indices_pick_out_only_run_starts() {
    let messages = vec![
        user(vec![ContentBlock::text("first request")]), // 0: boundary
        assistant(vec![ContentBlock::tool_use("t1", "read_file", json!({}))]),
        user(vec![ContentBlock::tool_result("t1", "ok", false)]), // rider/results, not a boundary
        user(vec![ContentBlock::text("second request")]),         // 3: boundary
        assistant(vec![ContentBlock::text("done")]),
    ];
    let indices: Vec<usize> = run_start_indices(&messages).collect();
    assert_eq!(indices, vec![0, 3]);
}

#[test]
fn indices_over_empty_slice_is_empty() {
    let indices: Vec<usize> = run_start_indices(&[]).collect();
    assert!(indices.is_empty());
}
