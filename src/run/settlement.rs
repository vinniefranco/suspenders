//! Run Settlement (CONTEXT.md): how an ended Run enters the Conversation, as
//! one pure fold.
//!
//! This module owns the Run-outcome vocabulary end to end: [`Outcome`] is what
//! the Run task returns (the Loop constructs `Ok`/`Failed`/`Error` directly;
//! the Agent's watcher mints `Down` when the task died without replying), and
//! [`settle`](Settlement::settle) folds it - with the accumulated facts and
//! the queued Steering - into the complete [`Resolution`]: the settled
//! Conversation, the broadcast [`Event`], the Session Log entry, and the
//! Rollover decision. The Agent only interprets - it updates its state, logs,
//! broadcasts, and maybe starts the next Run (the same pure-core/process-shell
//! split ADR-0011 gave the Run loop). There is no translation layer between
//! the Loop's outcome and this fold: they are the same type, and the stop
//! reason (the canonical [`StopReason`], Hook atoms included) rides through
//! to the event and the log unmapped.
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
use crate::event::Event;
use crate::session::log::{Settled, SettledEntry};
use crate::stop_reason::StopReason;
use crate::voice;

/// A Run failure/exit reason, an arbitrary term at baud's boundary: it rides
/// the settlement event (via [`Reason::inspect`]) and the Session Log entry.
/// Only the shapes the Run produces are modelled; [`Reason::inspect`]
/// reproduces baud's inspect rendering for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A bare atom reason (`:timeout`, `:boom`, `:killed`, `:shutdown`, ...):
    /// [`Reason::inspect`] prefixes the colon.
    Atom(String),
    /// A pre-rendered reason carried verbatim - no colon prefix, no reshaping.
    /// Production wraps a raw LLM error string (the error algebra's
    /// already-rendered term) and the bare word `turn_panic`.
    Verbatim(String),
}

impl Reason {
    /// A bare-atom reason, e.g. `Reason::atom("timeout")` for `:timeout`.
    pub fn atom(name: impl Into<String>) -> Self {
        Reason::Atom(name.into())
    }

    /// A pre-rendered reason carried verbatim, e.g.
    /// `Reason::verbatim("connection refused")`.
    pub fn verbatim(rendered: impl Into<String>) -> Self {
        Reason::Verbatim(rendered.into())
    }

    /// Baud's `inspect/1` rendering of the reason, as it enters the Session Log
    /// and the settlement event.
    pub fn inspect(&self) -> String {
        match self {
            Reason::Atom(name) => format!(":{name}"),
            Reason::Verbatim(rendered) => rendered.clone(),
        }
    }
}

/// How the Run task ended (baud's `outcome`) - the ONE Run-outcome type: the
/// Loop returns [`Outcome::Ok`] (carrying the canonical [`StopReason`], a
/// Hook's custom atom included), [`Outcome::Failed`] (the Loop already closed
/// the Conversation with the failure marker), or [`Outcome::Error`] (no
/// Conversation came back, e.g. budget exhaustion); the Agent's watcher mints
/// [`Outcome::Down`] when the task died without replying.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(Conversation, StopReason),
    Failed(Reason, Conversation),
    Error(Reason),
    Down(Reason),
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
/// broadcast settlement event ([`Event::RunFinished`] / [`Event::RunError`] /
/// [`Event::RunCancelled`]), the `{:settled, ...}` Session Log entry, and the
/// Rollover decision.
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
    /// before the Run, the fallback when no checkpoint arrived. One match,
    /// each arm naming the one way its outcome settles ([`completed`] /
    /// [`errored`] / [`cancelled`], the module's three settlement shapes) - no
    /// event-to-entry re-derivation.
    pub fn settle(&self, outcome: Outcome, base: &Conversation, steering: &[String]) -> Resolution {
        match outcome {
            Outcome::Ok(conversation, stop_reason) => {
                completed(conversation, stop_reason, steering)
            }
            // The Loop already closed this Conversation with the failure marker
            // and kept the errored response's partial text (the LLM error
            // algebra).
            Outcome::Failed(reason, conversation) => errored(reason, conversation, steering),
            Outcome::Error(reason) => errored(reason, failed(self.latest(base)), steering),
            // Cancellation needs both the flag and reason Shutdown.
            Outcome::Down(reason) if self.cancelled && reason == Reason::atom("shutdown") => {
                cancelled(self.latest(base))
            }
            Outcome::Down(reason) => errored(reason, failed(self.latest(base)), steering),
        }
    }

    // The checkpoint holds the partial Run (per Tool Result); before the first
    // one, the pre-Run snapshot is all there is.
    fn latest(&self, base: &Conversation) -> Conversation {
        self.checkpoint.clone().unwrap_or_else(|| base.clone())
    }
}

// The completed settlement: the RunFinished event over the settled
// Conversation's estimates, the completed log entry carrying the stop reason,
// and the ordinary Rollover.
fn completed(
    conversation: Conversation,
    stop_reason: StopReason,
    steering: &[String],
) -> Resolution {
    Resolution {
        event: Event::RunFinished {
            stop_reason: stop_reason.clone(),
            token_estimate: conversation.token_estimate(),
            context_budget: conversation.context_budget,
        },
        log_entry: SettledEntry::new(Settled::Completed, stop_reason, None),
        rollover: rollover(steering),
        conversation,
    }
}

// A failed settlement over an already-closed Conversation: the RunError
// event, the failed log entry carrying the inspected reason, and the
// ordinary Rollover.
fn errored(reason: Reason, conversation: Conversation, steering: &[String]) -> Resolution {
    Resolution {
        event: Event::RunError {
            reason: reason.inspect(),
        },
        log_entry: SettledEntry::new(Settled::Failed, StopReason::Error, Some(reason.inspect())),
        rollover: rollover(steering),
        conversation,
    }
}

// The cancelled settlement: the Conversation closed with the cancellation
// marker (roles keep alternating), the RunCancelled event, the cancelled log
// entry, and a DISCARDED queue - cancel means stop everything (the text stays
// in the UI's input history).
fn cancelled(mut conversation: Conversation) -> Resolution {
    conversation.add_assistant_blocks(vec![ContentBlock::text(voice::Marker::RunCancelled.text())]);
    Resolution {
        event: Event::RunCancelled,
        log_entry: SettledEntry::new(Settled::Cancelled, StopReason::Unknown, None),
        rollover: Rollover::None,
        conversation,
    }
}

// Close a failed Run with an assistant marker so roles keep alternating.
fn failed(mut conversation: Conversation) -> Conversation {
    conversation.add_assistant_blocks(vec![ContentBlock::text(voice::Marker::RunFailed.text())]);
    conversation
}

// Rollover (CONTEXT.md): a completed or failed settlement auto-submits the
// queue, joined, as the next Run's prompt; the cancellation arm discards it
// inline (cancel means stop everything).
fn rollover(steering: &[String]) -> Rollover {
    if steering.is_empty() {
        Rollover::None
    } else {
        Rollover::Submit(steering.join("\n"))
    }
}

#[cfg(test)]
#[path = "../../tests/run/settlement.rs"]
mod tests;
