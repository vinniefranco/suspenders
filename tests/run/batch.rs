use super::*;
use crate::approvals::ApprovalMode;
use crate::content::{ContentBlock, Usage};
use crate::llm::response::{Response, StopReason};
use crate::run::Outcome;
use crate::run::fixtures::{
    deps_for, events, find_tool_result, just, run_with, run_with_mode, session, text_end,
};
use crate::test_support::Entry;
use serde_json::json;
use tempfile::TempDir;

// Batch-sequential invariant (ADR-0049 Risk 1): the whole newest-pending
// attach safety rests on gated calls running ONE AT A TIME. With two gated
// `run_command` calls in a single Pass, the second's approval/execution must
// NOT begin before the first fully resolves. We assert the event ordering:
// the log shows r1's ApprovalRequest → r1's ToolResult BEFORE r2's
// ApprovalRequest ever appears (no interleaving).
#[tokio::test]
async fn two_gated_calls_never_overlap_the_second_waits_for_the_first() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let two_gated_pass = Response {
        content: vec![
            ContentBlock::tool_use("g1", "run_shell_command", json!({"command": "echo first"})),
            ContentBlock::tool_use("g2", "run_shell_command", json!({"command": "echo second"})),
        ],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![Entry::just(two_gated_pass), just(text_end("done"))],
    )
    // Approve both, front-to-back.
    .with_approvals(vec![true, true]);
    let (outcome, deps) = run_with(&session, "run two", deps).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    // Index the boundary events by their distinguishing text/id.
    let pos = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).expect("event present");
    let req1 =
        pos(&|e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo first"));
    let res1 = pos(&|e| matches!(e, Event::ToolResult { id, .. } if id == "g1"));
    let req2 =
        pos(&|e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo second"));
    let res2 = pos(&|e| matches!(e, Event::ToolResult { id, .. } if id == "g2"));

    // r1 is requested, then resolved to a result, ALL before r2 is even
    // requested - the sequential gate proven, no concurrency.
    assert!(req1 < res1, "r1 requested before it resolves");
    assert!(res1 < req2, "r1 fully resolves before r2's approval begins");
    assert!(req2 < res2, "r2 requested before it resolves");
}

// The Answer constructors fuse the Voice's wording with the ran-fact so
// the pairing cannot drift (CONTEXT.md: Answer).

#[test]
fn a_denial_pairs_the_command_denied_voice_with_the_denied_fact() {
    // ADR-0005: the Approval gate; the command never ran.
    let answer = Answer::denied();
    assert_eq!(
        result_blocks_text(&answer.content),
        voice::Marker::CommandDenied.text()
    );
    assert!(answer.is_error);
}

#[test]
fn a_malformed_input_answer_reads_as_a_run() {
    let answer = Answer::malformed("{not json");
    assert_eq!(
        result_blocks_text(&answer.content),
        voice::malformed_input("{not json")
    );
    assert!(answer.is_error);
}

// ---- plan-mode enforcement at the loop gate (ADR-0067) ----

// In PLAN mode a mutating Tool Call (write_file) is BLOCKED at the gate with NO
// modal: the tool never runs, the result is an error carrying qwen's verbatim
// "not a read-only tool" reason, and no ApprovalRequest is ever emitted.
#[tokio::test]
async fn plan_mode_blocks_a_mutating_tool_call_with_no_modal() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let target = root.path().join("a.rs");
    let write_pass = Response {
        content: vec![ContentBlock::tool_use(
            "w1",
            "write_file",
            json!({"file_path": target.to_string_lossy(), "content": "fn main() {}"}),
        )],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![Entry::just(write_pass), just(text_end("done"))],
    );
    let (outcome, deps) = run_with_mode(&session, "write it", ApprovalMode::Plan, deps).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    // No modal opened for the blocked call.
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::ApprovalRequest { .. })),
        "a plan-mode block opens NO modal"
    );
    // The tool result is an error carrying qwen's verbatim block reason.
    let Some(Event::ToolResult {
        content, is_error, ..
    }) = find_tool_result(&evs, "w1")
    else {
        panic!("no tool result for w1");
    };
    assert!(*is_error, "the blocked call reads as an error");
    assert!(
        content.contains("Tool blocked by plan mode: \"write_file\" is not a read-only tool."),
        "the verbatim block reason reaches the model: {content}"
    );
    // No file was written (the tool never ran).
    assert!(
        !root.path().join("a.rs").exists(),
        "the blocked write never touched the filesystem"
    );
}

// In PLAN mode a read-only Tool Call (read_file) is ALLOWED: it runs normally,
// no block, no modal.
#[tokio::test]
async fn plan_mode_allows_a_read_only_tool_call() {
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("a.txt"), "hello from disk").unwrap();
    let session = session(root.path());
    let read_pass = Response {
        content: vec![ContentBlock::tool_use(
            "r1",
            "read_file",
            json!({"file_path": root.path().join("a.txt").to_string_lossy()}),
        )],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![Entry::just(read_pass), just(text_end("done"))],
    );
    let (outcome, deps) = run_with_mode(&session, "read it", ApprovalMode::Plan, deps).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Some(Event::ToolResult {
        content, is_error, ..
    }) = find_tool_result(&evs, "r1")
    else {
        panic!("no tool result for r1");
    };
    assert!(!*is_error, "the read ran fine: {content}");
    assert!(
        content.contains("hello from disk"),
        "the allowed read returned the file content: {content}"
    );
}

// In DEFAULT mode the same mutating write is NOT blocked (edits are ungated in
// suspenders): it runs, proving the block is plan-mode-specific.
#[tokio::test]
async fn default_mode_does_not_block_a_mutating_tool_call() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let target = root.path().join("a.rs");
    let write_pass = Response {
        content: vec![ContentBlock::tool_use(
            "w1",
            "write_file",
            json!({"file_path": target.to_string_lossy(), "content": "fn main() {}"}),
        )],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![Entry::just(write_pass), just(text_end("done"))],
    );
    let (outcome, deps) = run_with_mode(&session, "write it", ApprovalMode::Default, deps).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Some(Event::ToolResult { is_error, .. }) = find_tool_result(&evs, "w1") else {
        panic!("no tool result for w1");
    };
    assert!(!*is_error, "default mode runs the write");
    assert!(
        root.path().join("a.rs").exists(),
        "default mode wrote the file"
    );
}
