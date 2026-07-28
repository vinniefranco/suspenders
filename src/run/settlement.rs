//! Run Settlement (CONTEXT.md): how an ended Run enters the Conversation, as
//! one pure fold.
//!
//! `crate::agent` accumulates the facts here while the Run task runs - the
//! latest checkpoint, the reported stop reason, whether the user cancelled -
//! and calls [`settle`] exactly once when the task ends. The fold returns the
//! complete resolution: the settled Conversation, the settlement event to
//! broadcast, the Session Log entry, and the Rollover decision over the queued
//! Steering. The Agent only interprets - it updates its state, logs,
//! broadcasts, and maybe starts the next Run (the same pure-core/process-shell
//! split ADR-0011 gave the Run loop).
//!
//! Every Run settles exactly one way: completed, failed, or cancelled; a crash
//! settles as a failure, and so does a `Shutdown` nobody asked for.
//! Cancellation needs both the flag and reason `Shutdown`, so a crash that
//! races a cancel still settles as a failure.
//!
//! A Run that did not complete settles on its latest checkpoint (the pre-Run
//! Conversation when none arrived): the killed Run's Tool Calls already
//! mutated the disk, so dropping them from the Conversation would leave the
//! model amnesiac about its own edits. The settled Conversation closes with an
//! assistant marker so roles keep alternating - strict chat templates on small
//! local models choke on two user messages in a row.
//!
//! Rollover (CONTEXT.md): Steering the Run ended before delivering
//! auto-submits as the next Run's prompt (queued texts joined) when the Run
//! settled completed or failed; Cancellation discards it - cancel means stop
//! everything (the text stays in the UI's input history).

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::session::log::{Settled, SettledEntry, StopReason};
use crate::voice;

/// A Run failure/exit reason, an arbitrary term at baud's boundary: it rides
/// the settlement event verbatim, and is debug-formatted (baud's `inspect/1`)
/// into the Session Log entry. Only the shapes the Run produces are modelled;
/// [`Reason::inspect`] reproduces baud's inspect rendering for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A bare atom reason (`:timeout`, `:boom`, `:killed`, `:shutdown`, ...).
    Atom(String),
    /// A tagged tuple reason (`{:badarg, []}` and friends), carrying its inspect
    /// rendering verbatim.
    Tuple(String),
}

impl Reason {
    /// A bare-atom reason, e.g. `Reason::atom("timeout")` for `:timeout`.
    pub fn atom(name: impl Into<String>) -> Self {
        Reason::Atom(name.into())
    }

    /// A tagged-tuple reason carrying its already-rendered inspect form, e.g.
    /// `Reason::tuple("{:badarg, []}")`.
    pub fn tuple(rendered: impl Into<String>) -> Self {
        Reason::Tuple(rendered.into())
    }

    /// Baud's `inspect/1` rendering of the reason, as it enters the Session Log.
    pub fn inspect(&self) -> String {
        match self {
            Reason::Atom(name) => format!(":{name}"),
            Reason::Tuple(rendered) => rendered.clone(),
        }
    }
}

/// How the Run task ended, as seen from the Agent's mailbox (baud's
/// `outcome`): the async reply - [`Outcome::Ok`], [`Outcome::Failed`] (the Loop
/// already closed the Conversation with the failure marker), or
/// [`Outcome::Error`] (no Conversation came back, e.g. budget exhaustion) - or
/// [`Outcome::Down`] when the task died without replying.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(Conversation, StopReason),
    Failed(Reason, Conversation),
    Error(Reason),
    Down(Reason),
}

/// The settlement event to broadcast (baud's `event`).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    RunFinished {
        stop_reason: StopReason,
        token_estimate: u64,
        context_budget: u64,
    },
    RunError(Reason),
    RunCancelled,
}

/// The Rollover decision over the queued Steering (CONTEXT.md): `Submit` the
/// joined queue as the next Run's prompt, or `None` (empty queue, or a
/// cancellation discarding it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rollover {
    Submit(String),
    None,
}

/// The complete resolution of one ended Run: the settled Conversation, the
/// settlement event, the `{:settled, ...}` Session Log entry, and the Rollover
/// decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub conversation: Conversation,
    pub event: Event,
    pub log_entry: SettledEntry,
    pub rollover: Rollover,
}

/// The accumulating settlement facts (baud's `Settlement.t`): the latest
/// checkpoint and whether the user cancelled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settlement {
    checkpoint: Option<Conversation>,
    cancelled: bool,
}

impl Settlement {
    pub fn new() -> Self {
        Settlement::default()
    }

    /// Partial-Run snapshot, sent after every Tool Result; the latest one wins.
    pub fn note_checkpoint(mut self, conversation: Conversation) -> Self {
        self.checkpoint = Some(conversation);
        self
    }

    /// The user cancelled; a following `Down(Shutdown)` settles as cancelled.
    pub fn note_cancelled(mut self) -> Self {
        self.cancelled = true;
        self
    }

    /// Folds the outcome, the accumulated facts, and the queued Steering into
    /// the Run's complete [`Resolution`]. `base` is the Conversation from
    /// before the Run, the fallback when no checkpoint arrived.
    pub fn settle(&self, outcome: Outcome, base: &Conversation, steering: &[String]) -> Resolution {
        let (conversation, event) = self.resolve(outcome, base);
        let log_entry = log_entry(&event);
        let rollover = rollover(&event, steering);
        Resolution {
            conversation,
            event,
            log_entry,
            rollover,
        }
    }

    fn resolve(&self, outcome: Outcome, base: &Conversation) -> (Conversation, Event) {
        match outcome {
            Outcome::Ok(conversation, stop_reason) => {
                let token_estimate = conversation.token_estimate();
                let context_budget = conversation.context_budget;
                (
                    conversation,
                    Event::RunFinished {
                        stop_reason,
                        token_estimate,
                        context_budget,
                    },
                )
            }
            // The Loop already closed this Conversation with the failure marker
            // and kept the errored response's partial text (the LLM error
            // algebra).
            Outcome::Failed(reason, conversation) => (conversation, Event::RunError(reason)),
            Outcome::Error(reason) => (failed(self.latest(base)), Event::RunError(reason)),
            // Cancellation needs both the flag and reason Shutdown.
            Outcome::Down(reason) if self.cancelled && reason == Reason::atom("shutdown") => {
                let mut conversation = self.latest(base);
                conversation
                    .add_assistant_blocks(vec![ContentBlock::text(voice::run_cancelled_marker())]);
                (conversation, Event::RunCancelled)
            }
            Outcome::Down(reason) => (failed(self.latest(base)), Event::RunError(reason)),
        }
    }

    // The checkpoint holds the partial Run (per Tool Result); before the first
    // one, the pre-Run snapshot is all there is.
    fn latest(&self, base: &Conversation) -> Conversation {
        self.checkpoint.clone().unwrap_or_else(|| base.clone())
    }
}

// Close a failed Run with an assistant marker so roles keep alternating.
fn failed(mut conversation: Conversation) -> Conversation {
    conversation.add_assistant_blocks(vec![ContentBlock::text(voice::run_failed_marker())]);
    conversation
}

fn log_entry(event: &Event) -> SettledEntry {
    match event {
        Event::RunFinished { stop_reason, .. } => {
            SettledEntry::new(Settled::Completed, *stop_reason, None)
        }
        Event::RunError(reason) => {
            SettledEntry::new(Settled::Failed, StopReason::Error, Some(reason.inspect()))
        }
        Event::RunCancelled => SettledEntry::new(Settled::Cancelled, StopReason::Unknown, None),
    }
}

// Rollover (CONTEXT.md): Cancellation discards the queue; any other settlement
// auto-submits it, joined, as the next Run's prompt.
fn rollover(event: &Event, steering: &[String]) -> Rollover {
    match event {
        Event::RunCancelled => Rollover::None,
        _ if steering.is_empty() => Rollover::None,
        _ => Rollover::Submit(steering.join("\n")),
    }
}

#[cfg(test)]
mod tests {
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
            ContentBlock::text(voice::run_failed_marker()),
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
            voice::run_failed_marker(),
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
            voice::run_failed_marker(),
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
            voice::run_failed_marker(),
        );
    }

    #[test]
    fn a_crash_settles_as_a_failure() {
        let settlement = Settlement::new().note_checkpoint(checkpoint(base()));

        let resolution =
            settlement.settle(Outcome::Down(Reason::tuple("{:badarg, []}")), &base(), &[]);

        assert_eq!(
            resolution.event,
            Event::RunError(Reason::tuple("{:badarg, []}"))
        );
        assert_closed_with(
            &resolution.conversation,
            &checkpoint(base()),
            voice::run_failed_marker(),
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
            voice::run_failed_marker(),
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
            voice::run_failed_marker(),
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
            voice::run_cancelled_marker(),
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
            voice::run_cancelled_marker(),
        );
    }
}
