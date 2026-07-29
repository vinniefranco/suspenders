//! The event vocabulary between the Run, the Agent, and the Transcript - one
//! authoritative enumeration of every shape that flows as a run/agent event
//! (baud's `{:turn_event, ...}` / `{:baud_event, ...}` payloads).
//!
//! baud keeps the wire shape as bare tuples; what `Baud.Event` adds is a
//! single author for the shapes, so adding an event means adding a variant
//! (and a Transcript fold clause) rather than discovering the convention
//! across five files. This is the Rust port of that single author: a typed
//! [`Event`] enum with the same variants and the same constructor helpers.
//!
//! Settlement events ([`Event::RunFinished`], [`Event::RunError`],
//! [`Event::RunCancelled`]) are constructed by the Run's settlement as part
//! of its resolution; their shapes are enumerated here with everything else.
//!
//! An event the Transcript does not know is still silently ignored by its
//! catch-all (baud's deliberate tolerance: a new event must not break an old
//! subscriber) - that lives in the Transcript fold, not here.
//!
//! baud has no `event_test.exs`, so there are no ported tests in this module -
//! just the exhaustive enum and the constructor helpers `Baud.Event` provides.

use std::collections::HashMap;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::llm::Delta;
use crate::llm::response::StopReason;
use crate::view_model::SelectorRow;

/// The `extension_error` stage: which point in the extension's lifecycle crashed
/// (fail-open, ADR-0007). Mirrors baud's `:pre_run | :post_run` (and the
/// deferred `:present`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    PreRun,
    PostRun,
    /// The Presentment stage (deferred with the Transcript Item type).
    Present,
}

impl Stage {
    /// The atom-name baud uses on the wire (`:pre_run`, `:post_run`,
    /// `:present`).
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::PreRun => "pre_run",
            Stage::PostRun => "post_run",
            Stage::Present => "present",
        }
    }
}

/// Every event shape the Run and the Agent emit.
///
/// The `artifacts` on [`Event::ToolResult`] is display-side Presenter data
/// (CONTEXT.md: Artifact) - a `HashMap<String, Value>`, `{}` when no extension
/// attached any; it never enters the Conversation, is never shaped or evicted.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    // ---- Run lifecycle ----
    /// A Run began; carries the Run's reference. baud uses `reference()`;
    /// the Rust port carries an opaque string id.
    RunStarted(String),
    MessageStart {
        pass: u32,
    },
    MessageUpdate {
        delta: Delta,
        content: Vec<ContentBlock>,
    },
    MessageEnd {
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
    },

    // ---- Tool Calls ----
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: bool,
        artifacts: HashMap<String, Value>,
    },

    // ---- Steering ----
    SteeringQueued {
        text: String,
    },
    SteeringDelivered {
        text: String,
    },

    // ---- Approvals ----
    ApprovalRequest {
        approval_id: String,
        command: String,
    },
    ApprovalResolved {
        approval_id: String,
        approved: bool,
    },
    ApprovalAuto {
        command: String,
    },

    // ---- Extensions / Session Log / Context ----
    ExtensionError {
        extension: String,
        stage: Stage,
        message: String,
    },
    ContextPressure {
        token_estimate: u64,
        context_budget: u64,
        max_tokens_reserve: u64,
    },
    CompactionProgress {
        status: String,
    },
    /// The Session's cumulative dollar cost after a priced Response (ADR-0037:
    /// pricing rides the Catalog Model; surfacing is display-side only).
    /// Emitted by the metered boundary for every priced call - main Run,
    /// Scout, and Compaction alike - and never for an unpriced (local/custom)
    /// Model, so a local-only Session sees none of these. Never logged: cost
    /// enters neither the Conversation nor the Session Log.
    SessionCost {
        total: f64,
    },
    SessionLogError {
        message: String,
    },

    /// A malformed-tool-call generation was re-drawn in-band (ADR-0030): the
    /// classified error and the attempt number against the budget. Silent to
    /// the model's Conversation - nothing is appended - but never silent to
    /// the operator: the Transcript shows an info line and the Session Log
    /// records a `retry` entry.
    Retry {
        error: String,
        attempt: u64,
        budget: u64,
    },

    /// The loop-detector terminated a Run: the model emitted the identical
    /// Tool Call batch `count` times in a row (the configured stall limit).
    /// A passive circuit breaker - NO steering text enters the Conversation,
    /// only this operator-visible event and the Run's close marker. Display-side
    /// only; the durable fact rides the Run's `turn_limit_stuck` settlement.
    LoopStall {
        count: u64,
    },

    // ---- Slash Command selector (ADR-0032/0033) ----
    /// A committed selector-opening Slash Command's rows arrived: the adapter
    /// fetched them (e.g. `/model`'s model list) and hands them back so the
    /// pure core flips its `Loading` overlay to a `Ready` [`SelectorRow`] list.
    /// The core stays command-agnostic - it neither fetches nor interprets;
    /// these are opaque rows the generic selector filters and renders.
    /// `generation` echoes the activation counter the requesting
    /// `Effect::Command` carried, so the core can drop a fill meant for an
    /// earlier activation.
    SelectorReady {
        generation: u64,
        rows: Vec<SelectorRow>,
    },
    /// The adapter could not produce the rows (fetch failed, cache empty): the
    /// pure core flips its `Loading` overlay to `Failed(message)`. Carries the
    /// same `generation` echo as [`Event::SelectorReady`].
    SelectorFailed {
        generation: u64,
        message: String,
    },

    // ---- Settlement ----
    RunFinished {
        stop_reason: StopReason,
        token_estimate: u64,
        context_budget: u64,
    },
    RunCancelled,
    /// A Run failed; carries the reason (baud's `term()`).
    RunError {
        reason: String,
    },
}

impl Event {
    // ---- Run lifecycle ----

    pub fn run_started(reference: impl Into<String>) -> Self {
        Event::RunStarted(reference.into())
    }

    pub fn message_start(pass: u32) -> Self {
        Event::MessageStart { pass }
    }

    pub fn message_update(delta: Delta, content: Vec<ContentBlock>) -> Self {
        Event::MessageUpdate { delta, content }
    }

    pub fn message_end(content: Vec<ContentBlock>, stop_reason: StopReason) -> Self {
        Event::MessageEnd {
            content,
            stop_reason,
        }
    }

    // ---- Tool Calls ----

    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Event::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    pub fn tool_result(
        id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
        artifacts: HashMap<String, Value>,
    ) -> Self {
        Event::ToolResult {
            id: id.into(),
            name: name.into(),
            content: content.into(),
            is_error,
            artifacts,
        }
    }

    pub fn retry(error: impl Into<String>, attempt: u64, budget: u64) -> Self {
        Event::Retry {
            error: error.into(),
            attempt,
            budget,
        }
    }

    pub fn loop_stall(count: u64) -> Self {
        Event::LoopStall { count }
    }

    // ---- Steering ----

    pub fn steering_queued(text: impl Into<String>) -> Self {
        Event::SteeringQueued { text: text.into() }
    }

    pub fn steering_delivered(text: impl Into<String>) -> Self {
        Event::SteeringDelivered { text: text.into() }
    }

    // ---- Approvals ----

    pub fn approval_request(id: impl Into<String>, command: impl Into<String>) -> Self {
        Event::ApprovalRequest {
            approval_id: id.into(),
            command: command.into(),
        }
    }

    pub fn approval_resolved(id: impl Into<String>, approved: bool) -> Self {
        Event::ApprovalResolved {
            approval_id: id.into(),
            approved,
        }
    }

    pub fn approval_auto(command: impl Into<String>) -> Self {
        Event::ApprovalAuto {
            command: command.into(),
        }
    }

    // ---- Slash Command selector ----

    pub fn selector_ready(generation: u64, rows: Vec<SelectorRow>) -> Self {
        Event::SelectorReady { generation, rows }
    }

    pub fn selector_failed(generation: u64, message: impl Into<String>) -> Self {
        Event::SelectorFailed {
            generation,
            message: message.into(),
        }
    }

    // ---- The rest ----

    /// Constructs an `extension_error` from a pipeline [`crate::extensions::Failure`]'s
    /// parts: the extension name, the stage that crashed, and the message.
    pub fn extension_error(
        extension: impl Into<String>,
        stage: Stage,
        message: impl Into<String>,
    ) -> Self {
        Event::ExtensionError {
            extension: extension.into(),
            stage,
            message: message.into(),
        }
    }

    pub fn context_pressure(
        token_estimate: u64,
        context_budget: u64,
        max_tokens_reserve: u64,
    ) -> Self {
        Event::ContextPressure {
            token_estimate,
            context_budget,
            max_tokens_reserve,
        }
    }

    pub fn compaction_progress(status: impl Into<String>) -> Self {
        Event::CompactionProgress {
            status: status.into(),
        }
    }

    pub fn session_cost(total: f64) -> Self {
        Event::SessionCost { total }
    }

    // qual:test_helper
    pub fn session_log_error(message: impl Into<String>) -> Self {
        Event::SessionLogError {
            message: message.into(),
        }
    }
}
