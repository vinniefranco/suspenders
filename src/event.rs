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

use crate::approvals::ApprovalMode;
use crate::content::ContentBlock;
use crate::llm::Delta;
use crate::llm::response::StopReason;
use crate::mcp::McpServerView;
use crate::tool::caps::Question;
use crate::view_model::SelectorRow;

/// A label on the generic fail-open `extension_error` report channel: which
/// point an extension subsystem crashed (fail-open, ADR-0007). These are just
/// labels on the report line, not a Middleware pipeline stage - the pipeline is
/// retired (Hooks are the lifecycle-interception layer, ADR-0066). MCP connect,
/// Hook discovery/firing, and skill discovery report through this channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    PreRun,
    PostRun,
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

/// One AT file-picker suggestion (qwen `useAtCompletion`'s `{label, value}`):
/// the repo-relative `label` shown in the popup (and highlighted against the
/// query) and the `value` INSERTED on accept - the same path with its spaces
/// backslash-escaped (qwen `escapePath`), so the detection's unescaped-space
/// scan round-trips. `matched` is the contiguous fuzzy-match window over
/// `label` for the inverted highlight (`None` for the empty-query rows). Plain
/// owned data crossing the async seam, mirroring [`SelectorRow`]; the composer
/// maps it into a render `Suggestion` (no matcher type crosses this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSuggestion {
    pub label: String,
    pub value: String,
    pub matched: Option<(usize, usize)>,
}

/// Every event shape the Run and the Agent emit.
///
/// The `artifacts` on [`Event::ToolResult`] is the display-side data a Tool
/// attaches to its result (CONTEXT.md: Artifact) - a `HashMap<String, Value>`,
/// `{}` when the Tool attached none; it never enters the Conversation, is never
/// shaped or evicted.
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
    /// The Approval mode changed (ADR-0050): the Agent's pure `Approvals` fold
    /// rotated its mode via the Shift+Tab cycle, and this mirrors the new mode
    /// to the Screen so the footer AutoAcceptIndicator can render it. The mode
    /// state lives authoritatively on the Agent; the Screen holds a display-only
    /// copy fed by this event.
    ApprovalModeChanged {
        mode: ApprovalMode,
    },

    // ---- Questions (ADR-0057, qwen ask_user_question) ----
    /// A tool put one or more questions to the user (`ask_user_question`): the
    /// Screen opens the question modal. Unlike an Approval there is NO auto path -
    /// every question opens a modal (ADR-0057). `question_id` is the per-call
    /// reference the Agent holds the reply oneshot under; `questions` are the
    /// shaped [`Question`]s the modal renders.
    QuestionRequest {
        question_id: String,
        questions: Vec<Question>,
    },
    /// A question round-trip settled (answered or declined): the Agent emits it
    /// after the reply is sent, mirroring [`Event::ApprovalResolved`]. The Screen
    /// has already cleared the modal on the answering keypress; this is the
    /// operator-visible settlement marker, carrying only the `question_id`.
    QuestionResolved {
        question_id: String,
    },

    // ---- Plan mode (ADR-0067, qwen exit_plan_mode) ----
    /// A tool raised the plan-confirmation dialog (`exit_plan_mode` in plan
    /// mode): the Screen opens the plan modal (Phase 3's `PendingPlan`). Parallel
    /// to [`Event::QuestionRequest`] - no auto path, the modal always opens.
    /// `plan_id` is the per-call reference the Agent holds the reply oneshot
    /// under; `plan` is the plan text the modal renders; `pre_plan_mode` is the
    /// mode Plan was entered from, so the modal can word qwen's "restore previous
    /// mode ({mode})" row. The round-trip resolves via
    /// [`crate::agent::Command::AnswerPlan`].
    PlanRequest {
        plan_id: String,
        plan: String,
        pre_plan_mode: ApprovalMode,
    },
    /// A plan round-trip settled (approved, cancelled, or stale): the Agent emits
    /// it after the reply is sent, mirroring [`Event::QuestionResolved`]. The
    /// Screen has already cleared the modal on the answering keypress; this is
    /// the operator-visible settlement marker, carrying only the `plan_id`.
    PlanResolved {
        plan_id: String,
    },

    // ---- MCP OAuth (ADR-0065 Phase D) ----
    /// A progress line from an in-flight `/mcp` Authenticate op (ADR-0065 Phase D,
    /// qwen's `OauthDisplayMessage` / `OauthAuthUrl`): the operator-visible status
    /// the AUTHENTICATE dialog step (Phase E) renders as the browser flow runs.
    /// `server` names the server being authenticated; `message` is one display
    /// line (the copy-the-URL hint, the auth URL itself, or an exchange note).
    /// `is_url` marks the line that IS the authorization URL, so the dialog can
    /// render it as a copyable/clickable link rather than wrapped prose. Display-
    /// side only - it never enters the Conversation or the Session Log.
    McpAuthProgress {
        server: String,
        message: String,
        is_url: bool,
    },

    // ---- MCP management dialog (ADR-0065 Phase E) ----
    /// The `/mcp` dialog's server views arrived: the adapter fetched them off the
    /// loop (`Agent::mcp_views()`) and hands them back so the Composer's open
    /// `McpDialog` overlay flips out of its Loading state and re-seeds the
    /// SERVER_LIST (qwen's `reloadServers`). Re-posted after every live action so
    /// the dialog reflects the change. `generation` echoes the activation counter
    /// the opening [`Effect::McpCommand`](crate::ui::screen::Effect::McpCommand)
    /// carried, so a stale fetch (from an earlier open) is dropped - exactly like
    /// [`Event::SelectorReady`]. Display-side only; never enters the Conversation.
    ///
    /// There is no `McpDialogFailed` twin (unlike the selector's ready/failed
    /// pair): `Agent::mcp_views()` fails OPEN to an empty list (a dead Agent
    /// answers `[]`), and an empty server list already renders the "No MCP servers
    /// configured." state - so the single ready event, carrying whatever the fetch
    /// found, is the whole fill vocabulary.
    McpDialogReady {
        generation: u64,
        servers: Vec<McpServerView>,
    },

    /// The footer's MCP health count (ADR-0065 Phase F, qwen `MCPHealthPill`):
    /// how many managed servers are `Disconnected && !is_disabled`. The footer
    /// renders synchronously, so the Screen holds this count and the pill reads
    /// it; the adapter recomputes it from the fetched [`McpServerView`]s at
    /// startup and whenever the `/mcp` dialog re-fetches, posting this to refresh
    /// the field. Display-side only; never enters the Conversation. The live 30s
    /// health loop qwen runs is DEFERRED - the count refreshes on fetch, not on a
    /// timer.
    McpHealth {
        offline: usize,
    },

    /// The result of an `/mcp` AUTHENTICATE OSC52 copy attempt (ADR-0065 Phase E,
    /// qwen `AuthenticateStep`'s `copyState`): `copied` is whether the escape
    /// reached a TTY. The adapter posts it after
    /// [`Effect::ClipboardOsc52`](crate::ui::screen::Effect::ClipboardOsc52)
    /// writes, so the open dialog can flip the copy-feedback hint. Display-side
    /// only; consumed by the Composer's open dialog and never surfaced elsewhere.
    McpCopyResult {
        copied: bool,
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

    // ---- AT file completion (Phase C2, qwen `useAtCompletion`) ----
    /// A `@path` file search finished: the adapter walked the (gitignore-aware)
    /// project tree, ranked it against `query`, capped it, and hands the rows
    /// back so the composer's AT overlay shows them. The core stays IO-free -
    /// it neither walks nor ranks; these are ready-to-render suggestions.
    /// `generation` echoes the [`Effect::FileSearch`](crate::ui::screen::Effect::FileSearch)
    /// activation counter, and `query` echoes the pattern searched: a delivery
    /// whose generation OR query no longer matches the live AT pattern is
    /// DROPPED (a stale keystroke's result must not overwrite a newer one).
    /// A failed/empty walk simply delivers an empty `suggestions` - there is no
    /// separate failure event, the popup just shows "no matches".
    ///
    /// [`Effect::FileSearch`]: crate::ui::screen::Effect::FileSearch
    FileSearchReady {
        generation: u64,
        query: String,
        suggestions: Vec<FileSuggestion>,
    },

    // ---- Background subagents (P4b, ADR-0063) ----
    /// A background subagent settled (or was cancelled) and its
    /// `<task-notification>` envelope was queued for the next Run to drain: this
    /// carries the same envelope text to the UI immediately, so the operator sees
    /// the completion now even though the model only reads it on its next Run.
    /// The text is already the assembled, XML-escaped envelope.
    BackgroundNotification {
        text: String,
    },
    /// A background subagent reached a terminal lifecycle state (P4b, ADR-0063):
    /// the operator-visible marker carrying the task id and the lifecycle word
    /// (`completed`/`failed`/`cancelled`). Display-side only - the durable fact
    /// rides the queued notification's user-role log entry.
    BackgroundTaskFinished {
        task_id: String,
        status: String,
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

    pub fn approval_mode_changed(mode: ApprovalMode) -> Self {
        Event::ApprovalModeChanged { mode }
    }

    // ---- Questions ----

    pub fn question_request(id: impl Into<String>, questions: Vec<Question>) -> Self {
        Event::QuestionRequest {
            question_id: id.into(),
            questions,
        }
    }

    pub fn question_resolved(id: impl Into<String>) -> Self {
        Event::QuestionResolved {
            question_id: id.into(),
        }
    }

    // ---- Plan mode ----

    pub fn plan_request(
        id: impl Into<String>,
        plan: impl Into<String>,
        pre_plan_mode: ApprovalMode,
    ) -> Self {
        Event::PlanRequest {
            plan_id: id.into(),
            plan: plan.into(),
            pre_plan_mode,
        }
    }

    pub fn plan_resolved(id: impl Into<String>) -> Self {
        Event::PlanResolved { plan_id: id.into() }
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

    // ---- AT file completion ----

    pub fn file_search_ready(
        generation: u64,
        query: impl Into<String>,
        suggestions: Vec<FileSuggestion>,
    ) -> Self {
        Event::FileSearchReady {
            generation,
            query: query.into(),
            suggestions,
        }
    }

    // ---- Background subagents (P4b, ADR-0063) ----

    pub fn background_notification(text: impl Into<String>) -> Self {
        Event::BackgroundNotification { text: text.into() }
    }

    pub fn background_task_finished(task_id: impl Into<String>, status: impl Into<String>) -> Self {
        Event::BackgroundTaskFinished {
            task_id: task_id.into(),
            status: status.into(),
        }
    }

    // ---- MCP OAuth (ADR-0065 Phase D) ----

    /// A display-message progress line from an `/mcp` Authenticate op (qwen's
    /// `OauthDisplayMessage`): `is_url` false.
    pub fn mcp_auth_message(server: impl Into<String>, message: impl Into<String>) -> Self {
        Event::McpAuthProgress {
            server: server.into(),
            message: message.into(),
            is_url: false,
        }
    }

    /// The authorization-URL progress line from an `/mcp` Authenticate op (qwen's
    /// `OauthAuthUrl`): `is_url` true, so the dialog renders it as a link.
    pub fn mcp_auth_url(server: impl Into<String>, url: impl Into<String>) -> Self {
        Event::McpAuthProgress {
            server: server.into(),
            message: url.into(),
            is_url: true,
        }
    }

    // ---- MCP management dialog (ADR-0065 Phase E) ----

    /// The `/mcp` dialog's fetched server views, tagged with the activation the
    /// fill echoes (mirrors [`Event::selector_ready`]). No failed twin -
    /// `mcp_views()` fails open to an empty list, which reads as the empty state.
    pub fn mcp_dialog_ready(generation: u64, servers: Vec<McpServerView>) -> Self {
        Event::McpDialogReady {
            generation,
            servers,
        }
    }

    /// The footer MCP-health count (ADR-0065 Phase F): the number of servers that
    /// are disconnected and not disabled ([`mcp_offline_count`]).
    pub fn mcp_health(offline: usize) -> Self {
        Event::McpHealth { offline }
    }

    /// The result of an `/mcp` OSC52 copy attempt (ADR-0065 Phase E): whether the
    /// escape reached a TTY.
    pub fn mcp_copy_result(copied: bool) -> Self {
        Event::McpCopyResult { copied }
    }

    // ---- The rest ----

    /// Constructs an `extension_error` from a tool-side subsystem failure's
    /// parts: the source name, the stage that crashed, and the message. Used by
    /// the Agent's MCP init / ops fail-open reporting (ADR-0056).
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

    #[cfg(test)]
    pub fn session_log_error(message: impl Into<String>) -> Self {
        Event::SessionLogError {
            message: message.into(),
        }
    }
}
