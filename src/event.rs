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
use crate::conversation::WaveStats;
use crate::llm::Delta;
use crate::llm::response::StopReason;
use crate::run::governor::endgame::ReopenReason;
use crate::session::RecoveryShape;
use crate::view_model::{SelectorRow, Tone};

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

impl VoicedTag {
    /// The marker-plane [`Tone`] this rider carries (ADR-0040), stamped HERE at
    /// the firing-site authority so no downstream fold classifies by kind or
    /// text. A Governor's Nudge helps the model along ([`Tone::Aid`]); the
    /// Endgame's run-closing schedule limits it ([`Tone::Constrain`]).
    pub fn tone(self) -> Tone {
        match self {
            VoicedTag::VerifyNudge
            | VoicedTag::VerifyFailedNudge
            | VoicedTag::EmptyResponseNudge
            | VoicedTag::ExploreNudge => Tone::Aid,
            VoicedTag::WrapUpWarning | VoicedTag::VerificationPass | VoicedTag::FinalPass => {
                Tone::Constrain
            }
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
        /// The LIVE Dead Mass share (CONTEXT.md: Dead Mass) this pass, as a
        /// fraction of the Context Budget. Refreshed every pass alongside the
        /// token estimate, so the status bar shows dead mass as it currently
        /// stands, not the pre-reclaim snapshot a past wave found.
        dead_mass: f64,
    },
    /// An Eviction wave fired while shaping a request (CONTEXT.md: Eviction,
    /// Dead Mass): the counts by kind and the Dead Mass share at wave time.
    /// Display-side only - a wave rewrites the request copy, never the
    /// Session Log (the log's schema is replay-sensitive; Resume re-applies
    /// waves request-time), so this event is how wave behavior gets vetted.
    EvictionWave {
        stats: WaveStats,
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
    /// the anchor Governor's; the content is the Plan's - the model's voice,
    /// so it carries no [`VoicedTag`]. The `text` is the FULL anchor block the
    /// Session Log persists (the model read it verbatim); the Transcript shows
    /// only a concise `⚑ plan refreshed` marker (ADR-0040: an Aid tone), never
    /// the plan body.
    Anchor {
        text: String,
    },

    /// A Recovery Run opened (CONTEXT.md: Recovery Run): carries the arm
    /// taken, the reason it reopened (ADR-0043 - so an Open-Plan continuation
    /// greps distinctly from a broken-state recovery), and the Voice-authored
    /// prompt that starts it - the prompt enters the Conversation, so the
    /// Transcript must show it.
    RecoveryRun {
        shape: RecoveryShape,
        reason: ReopenReason,
        text: String,
    },

    /// The Endgame narrowed the offered Tools (CONTEXT.md: Endgame, Offer;
    /// ADR-0035): carries the surviving tool names (empty on the final Pass,
    /// `run_command` alone on the Verification Pass). Display-side only - the
    /// narrowing itself shapes the request, this event just lets the Transcript
    /// show a concise `⊘ tools narrowed` marker (ADR-0040: a Constrain tone).
    /// A Governor limiting the model, so it carries no [`VoicedTag`].
    ToolsNarrowed {
        tools: Vec<String>,
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

    /// The marker-plane [`Tone`] a voiced rider carries (ADR-0040), recovered
    /// from the variant back to its [`VoicedTag`] so the tone decision stays in
    /// ONE place ([`VoicedTag::tone`]) and the Screen fold never classifies.
    /// `None` for a non-rider event.
    pub fn voiced_tone(&self) -> Option<Tone> {
        let tag = match self {
            Event::VerifyNudge { .. } => VoicedTag::VerifyNudge,
            Event::VerifyFailedNudge { .. } => VoicedTag::VerifyFailedNudge,
            Event::EmptyResponseNudge { .. } => VoicedTag::EmptyResponseNudge,
            Event::ExploreNudge { .. } => VoicedTag::ExploreNudge,
            Event::WrapUpWarning { .. } => VoicedTag::WrapUpWarning,
            Event::VerificationPass { .. } => VoicedTag::VerificationPass,
            Event::FinalPass { .. } => VoicedTag::FinalPass,
            _ => return None,
        };
        Some(tag.tone())
    }

    pub fn anchor(text: impl Into<String>) -> Self {
        Event::Anchor { text: text.into() }
    }

    pub fn recovery_run(
        shape: RecoveryShape,
        reason: ReopenReason,
        text: impl Into<String>,
    ) -> Self {
        Event::RecoveryRun {
            shape,
            reason,
            text: text.into(),
        }
    }

    pub fn tools_narrowed(tools: Vec<String>) -> Self {
        Event::ToolsNarrowed { tools }
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
        dead_mass: f64,
    ) -> Self {
        Event::ContextPressure {
            token_estimate,
            context_budget,
            max_tokens_reserve,
            dead_mass,
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
