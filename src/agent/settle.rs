//! The Run-outcome translation the Agent's `settle` folds through (ADR-0017).
//!
//! The actor loop hands `settle` a [`LoopOrDown`] - either the Loop's own
//! [`LoopOutcome`] (the awaited Run task yielded) or the watcher's [`Reason`]
//! (the task panicked or was aborted). These pure mappers turn that, and the
//! Settlement's resulting Event, into the vocabularies the rest of the Agent
//! speaks: the [`run::settlement`](crate::run::settlement) `Outcome`, the
//! Session-Log [`StopReason`], and the broadcast [`event::Event`](crate::event).
//! No state, no channel - just the shape translation, kept beside `settle`
//! rather than inside it so the actor file stays about the loop.

use crate::event::Event;
use crate::llm::response::StopReason as RespStopReason;
use crate::run::loop_::{Outcome as LoopOutcome, OutcomeStop};
use crate::run::settlement::{Event as SettleEvent, Outcome, Reason};
use crate::session::log::StopReason;

/// What the actor loop hands `settle`: the Loop's own outcome (the awaited Run
/// task yielded, baud's `{ref, outcome}`) or the watcher's Down (the task
/// panicked or was aborted, baud's `:DOWN`).
pub(super) enum LoopOrDown {
    Loop(LoopOutcome),
    Down(Reason),
}

// Maps the Loop's outcome (or the watcher's Down) into the settlement's outcome
// vocabulary.
pub(super) fn to_settlement_outcome(outcome: LoopOrDown) -> Outcome {
    match outcome {
        LoopOrDown::Loop(LoopOutcome::Ok(conv, stop)) => {
            Outcome::Ok(conv, outcome_stop_to_log(stop))
        }
        // The Loop already closed the Conversation with the failure marker and
        // kept the errored response's partial text (the LLM error algebra).
        LoopOrDown::Loop(LoopOutcome::Failed(reason, conv)) => {
            Outcome::Failed(Reason::tuple(reason), conv)
        }
        // Eviction + Compaction together could not fit the request.
        LoopOrDown::Loop(LoopOutcome::Error) => {
            Outcome::Error(Reason::atom("context_budget_exhausted"))
        }
        // A panic or an abort - settlement tells them apart via the cancel flag.
        LoopOrDown::Down(reason) => Outcome::Down(reason),
    }
}

fn outcome_stop_to_log(stop: OutcomeStop) -> StopReason {
    match stop {
        OutcomeStop::Reason(r) => r,
        // A custom after-Pass Stop atom - the wired AgentDeps never produces one
        // (its after_pass defaults to Continue). Degrade to Unknown.
        OutcomeStop::Custom(_) => StopReason::Unknown,
    }
}

// The settlement's Event → the broadcast event::Event (baud shares one shape;
// the Rust port has two typed enums that carry the same facts).
pub(super) fn settle_event_to_event(event: &SettleEvent) -> Event {
    match event {
        SettleEvent::RunFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } => Event::RunFinished {
            stop_reason: log_stop_to_resp(*stop_reason),
            token_estimate: *token_estimate,
            context_budget: *context_budget,
        },
        SettleEvent::RunError(reason) => Event::RunError {
            reason: reason.inspect(),
        },
        SettleEvent::RunCancelled => Event::RunCancelled,
    }
}

fn log_stop_to_resp(stop: StopReason) -> RespStopReason {
    match stop {
        StopReason::EndTurn => RespStopReason::EndTurn,
        StopReason::ToolUse => RespStopReason::ToolUse,
        StopReason::MaxTokens => RespStopReason::MaxTokens,
        StopReason::StopSequence => RespStopReason::StopSequence,
        StopReason::RunLimit
        | StopReason::RunLimitStuck
        | StopReason::Error
        | StopReason::Unknown => RespStopReason::Unknown,
    }
}
