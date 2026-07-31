use super::*;
use crate::content::{ContentBlock, Usage};
use crate::llm::response::{Response, StopReason};
use crate::run::Outcome;
use crate::run::fixtures::{deps_for, events, just, run_with, session, text_end};
use crate::test_support::Entry;
use serde_json::json;
use tempfile::TempDir;

fn make_tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

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
            make_tool_use("g1", "run_shell_command", json!({"command": "echo first"})),
            make_tool_use("g2", "run_shell_command", json!({"command": "echo second"})),
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
    assert_eq!(result_blocks_text(&answer.content), voice::command_denied());
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

#[test]
fn an_extension_halt_reads_as_a_failed_run() {
    let answer = Answer::halted("blocked by plugin".to_string(), Default::default());
    assert_eq!(result_blocks_text(&answer.content), "blocked by plugin");
    assert!(answer.is_error);
}
