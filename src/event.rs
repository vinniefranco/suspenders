//! The event vocabulary between the Turn, the Agent, and the Transcript — one
//! authoritative enumeration of every shape that flows as a turn/agent event
//! (baud's `{:turn_event, ...}` / `{:baud_event, ...}` payloads).
//!
//! baud keeps the wire shape as bare tuples; what `Baud.Event` adds is a
//! single author for the shapes, so adding an event means adding a variant
//! (and a Transcript fold clause) rather than discovering the convention
//! across five files. This is the Rust port of that single author: a typed
//! [`Event`] enum with the same variants and the same constructor helpers.
//!
//! Settlement events ([`Event::TurnFinished`], [`Event::TurnError`],
//! [`Event::TurnCancelled`]) are constructed by the Turn's settlement as part
//! of its resolution; their shapes are enumerated here with everything else.
//!
//! An event the Transcript does not know is still silently ignored by its
//! catch-all (baud's deliberate tolerance: a new event must not break an old
//! subscriber) — that lives in the Transcript fold, not here.
//!
//! baud has no `event_test.exs`, so there are no ported tests in this module —
//! just the exhaustive enum and the constructor helpers `Baud.Event` provides.

use std::collections::HashMap;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::conversation::WaveStats;
use crate::llm::response::StopReason;
use crate::llm::stream::Delta;
use crate::session::RecoveryShape;

/// The `plugin_error` stage: which point in the Plugin lifecycle crashed
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

/// Baud-voiced text that entered the Conversation (CONTEXT.md: Voice, and a
/// Nudge is always visible): the finish Nudges, the Explore Nudge, and the
/// Endgame's tail riders. In baud these share the `voiced/0` type and each
/// carries `%{text: ...}`; here they are distinct [`Event`] variants (so the
/// Transcript match stays exhaustive), and [`Event::voiced`] is the shared
/// constructor keyed by [`VoicedTag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoicedTag {
    VerifyNudge,
    VerifyFailedNudge,
    EmptyResponseNudge,
    ExploreNudge,
    WrapUpWarning,
    VerificationPass,
    FinalPass,
}

/// Every event shape the Turn and the Agent emit.
///
/// The `artifacts` on [`Event::ToolResult`] is display-side Plugin data
/// (CONTEXT.md: Artifact) — a `HashMap<String, Value>`, `{}` when no plugin
/// attached any; it never enters the Conversation, is never shaped or evicted.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    // ---- Turn lifecycle ----
    /// A Turn began; carries the Turn's reference. baud uses `reference()`;
    /// the Rust port carries an opaque string id.
    TurnStarted(String),
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

    // ---- Plugins / Session Log / Context ----
    PluginError {
        plugin: String,
        stage: Stage,
        message: String,
    },
    ContextPressure {
        token_estimate: u64,
        context_budget: u64,
        max_tokens_reserve: u64,
    },
    /// An Eviction wave fired while shaping a request (CONTEXT.md: Eviction,
    /// Dead Mass): the counts by kind and the Dead Mass share at wave time.
    /// Display-side only — a wave rewrites the request copy, never the
    /// Session Log (the log's schema is replay-sensitive; Resume re-applies
    /// waves request-time), so this event is how wave behavior gets vetted.
    EvictionWave {
        stats: WaveStats,
    },
    CompactionProgress {
        status: String,
    },
    SessionLogError {
        message: String,
    },

    // ---- Baud-voiced text entering the Conversation ----
    VerifyNudge {
        text: String,
    },
    VerifyFailedNudge {
        text: String,
    },
    EmptyResponseNudge {
        text: String,
    },
    ExploreNudge {
        text: String,
    },
    WrapUpWarning {
        text: String,
    },
    VerificationPass {
        text: String,
    },
    FinalPass {
        text: String,
    },
    /// An Anchor entered the Conversation (CONTEXT.md: Anchor). Placement is
    /// the anchor Governor's; the content is the Plan's — the model's voice,
    /// so it carries no [`VoicedTag`]. Routine rather than corrective, the
    /// Transcript ignores it; the Session Log persists it like every rider.
    Anchor {
        text: String,
    },

    /// A Recovery Turn opened (CONTEXT.md: Recovery Turn): carries the arm
    /// taken and the Voice-authored prompt that starts it — the prompt enters
    /// the Conversation, so the Transcript must show it.
    RecoveryTurn {
        shape: RecoveryShape,
        text: String,
    },

    /// A malformed-tool-call generation was re-drawn in-band (ADR-0030): the
    /// classified error and the attempt number against the budget. Silent to
    /// the model's Conversation — nothing is appended — but never silent to
    /// the operator: the Transcript shows an info line and the Session Log
    /// records a `retry` entry.
    Retry {
        error: String,
        attempt: u64,
        budget: u64,
    },

    // ---- Settlement ----
    TurnFinished {
        stop_reason: StopReason,
        token_estimate: u64,
        context_budget: u64,
    },
    TurnCancelled,
    /// A Turn failed; carries the reason (baud's `term()`).
    TurnError {
        reason: String,
    },
}

impl Event {
    // ---- Turn lifecycle ----

    pub fn turn_started(reference: impl Into<String>) -> Self {
        Event::TurnStarted(reference.into())
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

    // ---- Baud-voiced text entering the Conversation ----

    /// The shared constructor for the voiced Nudges and Endgame riders,
    /// mirroring baud's `voiced/2` keyed by tag.
    pub fn voiced(tag: VoicedTag, text: impl Into<String>) -> Self {
        let text = text.into();
        match tag {
            VoicedTag::VerifyNudge => Event::VerifyNudge { text },
            VoicedTag::VerifyFailedNudge => Event::VerifyFailedNudge { text },
            VoicedTag::EmptyResponseNudge => Event::EmptyResponseNudge { text },
            VoicedTag::ExploreNudge => Event::ExploreNudge { text },
            VoicedTag::WrapUpWarning => Event::WrapUpWarning { text },
            VoicedTag::VerificationPass => Event::VerificationPass { text },
            VoicedTag::FinalPass => Event::FinalPass { text },
        }
    }

    pub fn anchor(text: impl Into<String>) -> Self {
        Event::Anchor { text: text.into() }
    }

    pub fn recovery_turn(shape: RecoveryShape, text: impl Into<String>) -> Self {
        Event::RecoveryTurn {
            shape,
            text: text.into(),
        }
    }

    pub fn retry(error: impl Into<String>, attempt: u64, budget: u64) -> Self {
        Event::Retry {
            error: error.into(),
            attempt,
            budget,
        }
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

    // ---- The rest ----

    /// Constructs a `plugin_error` from a pipeline [`crate::plugins::Failure`]'s
    /// parts, mirroring baud's `plugin_error/1` which takes the failure map.
    pub fn plugin_error(
        plugin: impl Into<String>,
        stage: Stage,
        message: impl Into<String>,
    ) -> Self {
        Event::PluginError {
            plugin: plugin.into(),
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

    pub fn eviction_wave(stats: WaveStats) -> Self {
        Event::EvictionWave { stats }
    }

    pub fn compaction_progress(status: impl Into<String>) -> Self {
        Event::CompactionProgress {
            status: status.into(),
        }
    }

    pub fn session_log_error(message: impl Into<String>) -> Self {
        Event::SessionLogError {
            message: message.into(),
        }
    }
}
