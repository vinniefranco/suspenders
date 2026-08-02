use super::*;
use crate::content::{ContentBlock, Message};
use crate::conversation::{Conversation, ConversationOpts};
use serde_json::json;

// The pre-Run Conversation: system prompt plus the submitted user prompt.
fn base() -> Conversation {
    let mut conv = Conversation::new("system", ConversationOpts::new(10_000, 0));
    conv.add_user_text("do the thing");
    conv
}

// A partial-Run checkpoint: one answered Tool Call on top of a conversation.
fn checkpoint(mut conversation: Conversation) -> Conversation {
    conversation.add_assistant_blocks(vec![ContentBlock::tool_use(
        "t1",
        "read_file",
        json!({ "path": "a.ex" }),
    )]);
    conversation.add_tool_results(
        vec![ContentBlock::tool_result("t1", "defmodule A", false)],
        vec![],
    );
    conversation
}

fn last_message(conversation: &Conversation) -> &Message {
    conversation.messages.last().unwrap()
}

fn assert_closed_with(conversation: &Conversation, on_top_of: &Conversation, marker: &str) {
    assert_eq!(
        *last_message(conversation),
        Message::assistant(vec![ContentBlock::text(marker)])
    );

    let mut expected = on_top_of.messages.clone();
    expected.push(last_message(conversation).clone());
    assert_eq!(conversation.messages, expected);
}

// ---- completed ----

#[test]
fn adopts_the_tasks_conversation_and_the_reported_stop_reason() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);

    let resolution = Settlement::new().settle(
        Outcome::Ok(final_conv.clone(), StopReason::EndTurn),
        &base(),
        &[],
    );

    assert_eq!(resolution.conversation, final_conv);
    assert_eq!(
        resolution.event,
        Event::RunFinished {
            stop_reason: StopReason::EndTurn,
            token_estimate: final_conv.token_estimate(),
            context_budget: final_conv.context_budget,
        }
    );
}

#[test]
fn the_loops_stop_reason_rides_through_verbatim() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);

    let resolution = Settlement::new().settle(
        Outcome::Ok(final_conv.clone(), StopReason::RunLimit),
        &base(),
        &[],
    );

    assert_eq!(resolution.conversation, final_conv);
    assert!(matches!(
        resolution.event,
        Event::RunFinished {
            stop_reason: StopReason::RunLimit,
            ..
        }
    ));
}

#[test]
fn ignores_the_checkpoint_the_tasks_conversation_is_already_complete() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);
    let settlement = Settlement::new().note_checkpoint(checkpoint(base()));

    let resolution = settlement.settle(
        Outcome::Ok(final_conv.clone(), StopReason::EndTurn),
        &base(),
        &[],
    );

    assert_eq!(resolution.conversation, final_conv);
    assert!(matches!(resolution.event, Event::RunFinished { .. }));
}

// ---- failed with a conversation (Failed(reason, conv)) ----

#[test]
fn adopts_the_loop_closed_conversation_as_is() {
    // The Loop closed this itself (error algebra): partial text + marker.
    let mut closed = base();
    closed.add_assistant_blocks(vec![
        ContentBlock::text("partial thought"),
        ContentBlock::text(voice::Marker::RunFailed.text()),
    ]);

    let settlement = Settlement::new().note_checkpoint(checkpoint(base()));

    let resolution = settlement.settle(
        Outcome::Failed(Reason::atom("boom"), closed.clone()),
        &base(),
        &[],
    );

    assert_eq!(resolution.conversation, closed);
    assert_eq!(resolution.event, Event::RunError(Reason::atom("boom")));
}

// ---- failed ----

#[test]
fn in_run_error_with_no_checkpoint_closes_the_pre_run_conversation() {
    let resolution =
        Settlement::new().settle(Outcome::Error(Reason::atom("timeout")), &base(), &[]);

    assert_eq!(resolution.event, Event::RunError(Reason::atom("timeout")));
    assert_closed_with(
        &resolution.conversation,
        &base(),
        voice::Marker::RunFailed.text(),
    );
}

#[test]
fn in_run_error_with_a_checkpoint_the_partial_run_survives() {
    let partial = checkpoint(base());
    let settlement = Settlement::new().note_checkpoint(partial.clone());

    let resolution = settlement.settle(
        Outcome::Error(Reason::atom("context_budget_exhausted")),
        &base(),
        &[],
    );

    assert_eq!(
        resolution.event,
        Event::RunError(Reason::atom("context_budget_exhausted"))
    );
    assert_closed_with(
        &resolution.conversation,
        &partial,
        voice::Marker::RunFailed.text(),
    );
}

#[test]
fn the_latest_checkpoint_wins() {
    let first = checkpoint(base());
    let second = checkpoint(first.clone());

    let settlement = Settlement::new()
        .note_checkpoint(first)
        .note_checkpoint(second.clone());

    let resolution = settlement.settle(Outcome::Error(Reason::atom("timeout")), &base(), &[]);

    assert_closed_with(
        &resolution.conversation,
        &second,
        voice::Marker::RunFailed.text(),
    );
}

#[test]
fn a_crash_settles_as_a_failure() {
    let settlement = Settlement::new().note_checkpoint(checkpoint(base()));

    let resolution = settlement.settle(Outcome::Down(Reason::tuple("{:badarg, []}")), &base(), &[]);

    assert_eq!(
        resolution.event,
        Event::RunError(Reason::tuple("{:badarg, []}"))
    );
    assert_closed_with(
        &resolution.conversation,
        &checkpoint(base()),
        voice::Marker::RunFailed.text(),
    );
}

#[test]
fn a_shutdown_nobody_asked_for_settles_as_a_failure() {
    let resolution =
        Settlement::new().settle(Outcome::Down(Reason::atom("shutdown")), &base(), &[]);

    assert_eq!(resolution.event, Event::RunError(Reason::atom("shutdown")));
    assert_closed_with(
        &resolution.conversation,
        &base(),
        voice::Marker::RunFailed.text(),
    );
}

#[test]
fn a_crash_that_races_a_cancel_settles_as_a_failure_never_a_cancellation() {
    let settlement = Settlement::new().note_cancelled();

    let resolution = settlement.settle(Outcome::Down(Reason::atom("killed")), &base(), &[]);

    assert_eq!(resolution.event, Event::RunError(Reason::atom("killed")));
    assert_closed_with(
        &resolution.conversation,
        &base(),
        voice::Marker::RunFailed.text(),
    );
}

// ---- log entry ----

#[test]
fn completed_carries_the_stop_reason_no_failure_string() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);

    let resolution =
        Settlement::new().settle(Outcome::Ok(final_conv, StopReason::RunLimit), &base(), &[]);

    assert_eq!(
        resolution.log_entry,
        SettledEntry::new(Settled::Completed, StopReason::RunLimit, None)
    );
}

#[test]
fn failed_carries_the_reason_inspected_to_a_string() {
    let resolution =
        Settlement::new().settle(Outcome::Down(Reason::tuple("{:badarg, []}")), &base(), &[]);

    assert_eq!(
        resolution.log_entry,
        SettledEntry::new(
            Settled::Failed,
            StopReason::Error,
            Some("{:badarg, []}".to_string())
        )
    );
}

#[test]
fn cancelled_carries_neither() {
    let settlement = Settlement::new().note_cancelled();

    let resolution = settlement.settle(Outcome::Down(Reason::atom("shutdown")), &base(), &[]);

    assert_eq!(
        resolution.log_entry,
        SettledEntry::new(Settled::Cancelled, StopReason::Unknown, None)
    );
}

// ---- Rollover (CONTEXT.md) ----

#[test]
fn queued_steering_auto_submits_joined_after_a_completed_run() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);

    let resolution = Settlement::new().settle(
        Outcome::Ok(final_conv, StopReason::EndTurn),
        &base(),
        &["also this".to_string(), "and this".to_string()],
    );

    assert_eq!(
        resolution.rollover,
        Rollover::Submit("also this\nand this".to_string())
    );
}

#[test]
fn queued_steering_rolls_over_after_a_failed_run_too() {
    let resolution = Settlement::new().settle(
        Outcome::Error(Reason::atom("timeout")),
        &base(),
        &["keep going".to_string()],
    );

    assert_eq!(
        resolution.rollover,
        Rollover::Submit("keep going".to_string())
    );
}

#[test]
fn cancellation_discards_the_queue() {
    let settlement = Settlement::new().note_cancelled();

    let resolution = settlement.settle(
        Outcome::Down(Reason::atom("shutdown")),
        &base(),
        &["discarded".to_string()],
    );

    assert_eq!(resolution.rollover, Rollover::None);
}

#[test]
fn an_empty_queue_never_rolls_over() {
    let mut final_conv = base();
    final_conv.add_assistant_blocks(vec![ContentBlock::text("done")]);

    let resolution =
        Settlement::new().settle(Outcome::Ok(final_conv, StopReason::EndTurn), &base(), &[]);

    assert_eq!(resolution.rollover, Rollover::None);
}

// ---- cancelled ----

#[test]
fn cancel_plus_shutdown_closes_the_checkpoint_with_the_cancelled_marker() {
    let partial = checkpoint(base());

    let settlement = Settlement::new()
        .note_checkpoint(partial.clone())
        .note_cancelled();

    let resolution = settlement.settle(Outcome::Down(Reason::atom("shutdown")), &base(), &[]);

    assert_eq!(resolution.event, Event::RunCancelled);
    assert_closed_with(
        &resolution.conversation,
        &partial,
        voice::Marker::RunCancelled.text(),
    );
}

#[test]
fn with_no_checkpoint_the_pre_run_conversation_is_the_base() {
    let settlement = Settlement::new().note_cancelled();

    let resolution = settlement.settle(Outcome::Down(Reason::atom("shutdown")), &base(), &[]);

    assert_eq!(resolution.event, Event::RunCancelled);
    assert_closed_with(
        &resolution.conversation,
        &base(),
        voice::Marker::RunCancelled.text(),
    );
}
