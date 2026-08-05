//! Run - one agent iteration (prompt to settlement).
//!
//! [`run`] declares the Run's [`deps::RunDeps`] port (what a Run needs from its
//! host) and drives [`loop_::run`] over it. The host supplies both the concrete
//! [`deps::RunDeps`] adapter and the [`Capture`] (the Model + Llm snapshot the
//! Run took at spawn) it builds its tooling from - so the Run never depends on
//! any one host. The Agent's adapter is [`crate::agent::deps::AgentDeps`].

mod batch;
pub mod child;
pub mod deps;
mod dispatch;
mod finish;
pub mod hooks;
pub mod loop_;
pub mod next_speaker;
pub mod settlement;
pub mod side_query;
pub mod subagent;

// Shared test fixtures for the split Loop (today only `loop_`'s tests; any
// test module `batch`/`finish` grow shares this set instead of drifting
// copies). Private suffices: descendants reach a private ancestor module.
#[cfg(test)]
#[path = "../tests/run/fixtures.rs"]
mod fixtures;

use std::sync::Arc;

use crate::content::{ContentBlock, Message, Role};
use crate::conversation::{Conversation, ConversationOpts};
use crate::llm::Llm;
use crate::llm::ToolCallStyle;
use crate::llm::model::Model;
use crate::run::child::{ChildDeps, ChildSink};
use crate::run::deps::RunDeps;
use crate::run::loop_::{RunEnv, RunOpts};
use crate::run::settlement::Outcome;
use crate::session::Session;
use crate::stop_reason::StopReason;
use crate::tool::Tool;
use crate::tool::caps::{
    Capabilities, DecliningQuestioner, DenyingApprover, SubagentPlanMode, SubagentResult,
    UnavailableBackgroundShellSpawner, UnavailableSubagentSpawner,
};

/// The Model + Llm a Run captured at spawn (ADR-0033): the Model snapshot every
/// request travels to, and the Llm boundary the Run's tooling dispatches over.
/// The host builds this once and hands it to [`run`] alongside the
/// [`deps::RunDeps`] adapter, so the Run reads its tooling inputs from a plain
/// value rather than reaching into a particular host's deps.
pub struct Capture {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
    /// The tool-initiated Approval seam (F1, ADR-0055), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentApprover`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the registry it builds itself. `Arc<dyn Approver>` is Send+Sync,
    /// so the [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub approver: Arc<dyn crate::tool::caps::Approver>,
    /// The tool-initiated Question seam (P2a, ADR-0057), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentQuestioner`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the Approver. `Arc<dyn Questioner>` is Send+Sync, so the
    /// [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub questioner: Arc<dyn crate::tool::caps::Questioner>,
    /// The Plan-Mode effect seam (ADR-0067), built by the host that owns the
    /// Approval mode (the Agent builds a tx-backed `AgentPlanMode`). The Run
    /// assembles it into the Tool [`crate::tool::caps::Capabilities`] alongside
    /// the Questioner. `Arc<dyn PlanMode>` is Send+Sync, so the [`Capture`] stays
    /// `Send` for the `tokio::spawn` at the Agent. A child Run (spawned by
    /// `run_child`) carries the degraded [`crate::tool::caps::SubagentPlanMode`]
    /// here instead - the subagent block.
    pub plan_mode: Arc<dyn crate::tool::caps::PlanMode>,
    /// The Session-stable tool set the Agent built once (F8, ADR-0056):
    /// built-ins plus any discovered MCP tools. The Run builds its per-Run
    /// [`crate::tool_registry::ToolRegistry`] over this shared `Arc` (fresh
    /// revealed set per Run), so MCP tools ride every Run without re-boxing.
    pub tools: Arc<[Box<dyn crate::tool::Tool>]>,
    /// The Subagent effect seam (P4/F4, ADR-0061), built by the host that owns
    /// the subagent machinery (the Agent builds a
    /// [`crate::run::subagent::DirectSubagentSpawner`] over its Llm + registry +
    /// providers). The Run assembles it into the Tool
    /// [`crate::tool::caps::Capabilities`] alongside the Approver/Questioner.
    /// `Arc<dyn SubagentSpawner>` is Send+Sync, so the [`Capture`] stays `Send`
    /// for the `tokio::spawn` at the Agent. A child Run (spawned by `run_child`)
    /// carries an [`crate::tool::caps::UnavailableSubagentSpawner`] here instead
    /// - the recursion guard.
    pub subagents: Arc<dyn crate::tool::caps::SubagentSpawner>,
    /// The Background-Shell effect seam (Phase 9, ADR-0063), built by the host
    /// that owns the process lifecycle (the Agent builds a tx-backed
    /// `AgentBackgroundShellSpawner`). The Run assembles it into the Tool
    /// [`crate::tool::caps::Capabilities`] alongside the subagents spawner. A child
    /// Run (spawned by `run_child`) carries an
    /// [`crate::tool::caps::UnavailableBackgroundShellSpawner`] here instead - the
    /// recursion guard (a subagent cannot background a shell).
    pub bg_shells: Arc<dyn crate::tool::caps::BackgroundShellSpawner>,
    /// The hook subsystem (Phase 3a, ADR-0066): the standing hook manager the
    /// Agent resolved once at launch, threaded so `run` can build the Run's
    /// [`crate::run::hooks::Hooks`] firing handle over it. `Arc<HookManager>` is
    /// Send+Sync, so the [`Capture`] stays `Send` for the `tokio::spawn` at the
    /// Agent. A child Run carries the empty manager (a subagent fires no hooks in
    /// this phase - the tool-dispatch seam is the parent's).
    pub hooks: Arc<crate::hooks::HookManager>,
    /// The disk-skill manager (ADR-0058): the Agent discovered it once at
    /// launch, threaded so the Run can activate a conditional skill by touched
    /// path at the tool-success seam (`crate::run::batch`). Shared by `Arc` with
    /// the `skill` tool, so an activation this Run makes is visible to the tool's
    /// next catalog build. `Arc<SkillManager>` is Send+Sync, so the [`Capture`]
    /// stays `Send` for the `tokio::spawn` at the Agent. A child Run carries a
    /// fresh empty manager (a subagent activates nothing - the seam is the
    /// parent's).
    pub skills: Arc<crate::skills::SkillManager>,
    /// The Session identifier the hook payloads carry (H1, ADR-0066): the unique
    /// per-session token from the Session Log's JSONL file stem (ADR-0010). Empty
    /// for a Run whose Agent opened no log (a test, or a log-open failure).
    pub session_id: String,
    /// The Session Log's JSONL path the hook payloads carry (H1, ADR-0066): the
    /// running transcript a hook can tail (`transcript_path`). Empty when no log.
    pub transcript_path: String,
    /// The live Approval-mode mirror (ADR-0067): the shared
    /// [`crate::approvals::AtomicApprovalMode`] the Agent writes on every mode
    /// change and the batch gate reads in `classify`. Threaded through the
    /// Capture (like the Approver) because the Agent owns the authoritative
    /// mode; the Run only reads it. A child Run builds a fresh `Default` mirror
    /// (subagents do not cycle the mode), so it is NOT on the child path.
    pub approval_mode: Arc<crate::approvals::AtomicApprovalMode>,
    /// The one-shot manual-plan-exit notice carrier (ADR-0067): the shared
    /// [`crate::approvals::PendingManualPlanExit`] the Agent writes when the user
    /// leaves Plan OUTSIDE the approved `exit_plan_mode` flow (a `Shift+Tab`
    /// cycle), and the loop takes-and-clears in `shape_request` to inject the
    /// manual-exit reminder once. Threaded through the Capture like the mode
    /// mirror because the Agent owns the mode change; the Run only reads it. A
    /// child Run builds a fresh empty carrier (subagents do not cycle the mode),
    /// so it is NOT on the child path.
    pub plan_exit_notice: Arc<crate::approvals::PendingManualPlanExit>,
}

/// Runs the Run: builds the Tool registry and Tool ctx and drives
/// [`loop_::run`]. Returns the Loop outcome.
pub async fn run(
    conversation: Conversation,
    session: Session,
    capture: Capture,
    mut deps: impl RunDeps,
    opts: RunOpts,
) -> Outcome {
    // The Tool Registry, built once per Run (F3, F8). Reveals are Run-scoped: a
    // fresh registry per Run resets them, matching qwen's
    // clearRevealedDeferredTools on session reset. It shares the Agent's
    // Session-stable tool set (built-ins + discovered MCP tools, ADR-0056) via
    // `with_shared`, so MCP tools ride every Run. It rides the Tool ctx so
    // `tool_search` can reveal deferred tools into the next request's wire list.
    let registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::with_shared(
        Arc::clone(&capture.tools),
    ));

    // The file-read cache, built once per Run (F6, ADR-0060): fresh and empty at
    // Run start, like the registry, so a file read in a prior Run does not clear
    // this Run's read-before-edit enforcement. read_file records into it;
    // notebook_edit checks it before mutating a notebook.
    let read_cache = Arc::new(crate::tool::read_cache::FileReadCache::new());

    // The Side-Query effect seam (P2b, ADR-0055): the real impl is the captured
    // Llm boundary called OFF the main Conversation. Unlike the Approver, it does
    // NOT travel the Agent mpsc - a side-query touches no Agent/Conversation state
    // - so the Run builds it here from the Capture's own `llm`/`model` (no new
    // Capture field). `None` temperature defers a side-query to the server's own
    // default, matching qwen's side-query (which sets no temperature).
    let side_query: Arc<dyn crate::tool::caps::SideQuery> =
        Arc::new(crate::run::side_query::LlmSideQuery {
            llm: Arc::clone(&capture.llm),
            model: capture.model.clone(),
            temperature: None,
        });

    // The Tool Capability Context (F1, ADR-0055): the registry the Run built plus
    // the effect seams a Tool Call reaches its host through. P1b carries the
    // registry (concrete, Run-scoped state) and the Approver (the host's
    // tx-backed handle, threaded through the Capture); P2b adds the SideQuery
    // (built here at the Llm boundary). The other two capabilities land in their
    // consuming phases.
    let caps = crate::tool::caps::Capabilities {
        registry,
        read_cache,
        approver: Arc::clone(&capture.approver),
        side_query,
        // The Question seam (P2a, ADR-0057): the Agent's tx-backed handle,
        // threaded through the Capture like the Approver.
        questioner: Arc::clone(&capture.questioner),
        // The Plan-Mode seam (ADR-0067): the Agent's tx-backed handle, threaded
        // through the Capture like the Approver/Questioner. A child Run carries the
        // degraded SubagentPlanMode here instead - the subagent block.
        plan_mode: Arc::clone(&capture.plan_mode),
        // The Subagent seam (P4/F4, ADR-0061): the Agent's DirectSubagentSpawner,
        // threaded through the Capture like the Approver/Questioner. A child Run
        // carries an UnavailableSubagentSpawner here instead - the recursion
        // guard.
        subagents: Arc::clone(&capture.subagents),
        // The Background-Shell seam (Phase 9, ADR-0063): the Agent's tx-backed
        // handle, threaded through the Capture like the subagents spawner. A child
        // Run carries an UnavailableBackgroundShellSpawner here instead - the
        // recursion guard.
        bg_shells: Arc::clone(&capture.bg_shells),
        // The live Approval-mode mirror (ADR-0067): the Agent's shared atomic,
        // threaded through the Capture like the Approver. The batch gate reads it
        // in `classify` so plan mode enforces read-only against the LIVE mode,
        // even when the operator cycles mid-Run.
        approval_mode: Arc::clone(&capture.approval_mode),
        // The manual-exit notice carrier (ADR-0067): the Agent's shared carrier,
        // threaded through the Capture like the mode mirror. `shape_request` takes
        // it once on the next Pass after a Shift+Tab exit and injects the
        // manual-exit reminder into that request only.
        plan_exit_notice: Arc::clone(&capture.plan_exit_notice),
    };

    // The Tool ctx: the Session's Root and timeout, the Result Cap derived from
    // this Run's captured Model (ADR-0037), and the Run's Capabilities.
    let tool_ctx = session.tool_ctx(&capture.model, caps);

    // The Run's hook firing handle (Phase 3a, ADR-0066): built at THIS wiring
    // layer (above both the `hooks` leaf and `run_command`) over the standing
    // manager the Agent resolved, the captured Llm boundary + Model as the prompt
    // capability, and the Session's Project Root as the payload cwd. The
    // production shell/http capabilities are wired inside `Hooks::new`. It borrows
    // `capture`, which outlives the loop below. The reporter is an own emission
    // handle off the deps (ADR-0025), so a hook firing failure surfaces as a
    // visible fail-open report (ADR-0018).
    let hooks = crate::run::hooks::Hooks::new(
        capture.hooks.as_ref(),
        capture.llm.as_ref(),
        &capture.model,
        crate::run::hooks::HookIdentity {
            cwd: session.root.clone(),
            session_id: capture.session_id.clone(),
            transcript_path: capture.transcript_path.clone(),
        },
        Some(deps.emitter()),
    );

    // The conditional-skill activation seam (ADR-0058): the shared skill manager
    // the Agent discovered + the Session's Project Root, so `batch` can activate a
    // conditional skill by the file path a Tool Call touched. The manager is shared
    // with the `skill` tool, so an activation this Run makes shows up in the tool's
    // next catalog build.
    let skill_activation = loop_::SkillActivation {
        skills: Arc::clone(&capture.skills),
        project_root: std::path::PathBuf::from(&session.root),
    };

    loop_::run(
        conversation,
        &session,
        loop_::RunEnv {
            tool_ctx: &tool_ctx,
            hooks: Some(&hooks),
            skill_activation: Some(skill_activation),
        },
        &mut deps,
        opts,
    )
    .await
}

/// The request that drives one child Run to settlement (P4/F4, ADR-0061). A
/// self-contained bundle: the child's Model, the shared [`Llm`] boundary, the
/// child's verbatim system prompt + first-user `prompt`, its tool subset, its
/// turn bound (`max_turns`), the Session's request settings, and the parent
/// [`Session`] the child derives its Root/timeout/budget knobs from. A foreground
/// subagent passes `sink: None` (its whole run is invisible until it settles);
/// the background path (DEFERRED) would pass a `sink`.
///
/// `session` is the parent's Session cloned - a child Run is the parent's Run
/// over a FRESH Conversation with a narrowed tool set and its own turn bound, so
/// it reuses the parent's Root, command timeout, budget knobs, and Provider set.
/// `run_child` overrides only the turn bound (`run_limit = max_turns`) and the
/// captured Model. `depth` rides for defence: at `depth >= 1` the child's own
/// subagents capability is already degraded (the recursion guard), so `depth` is
/// a belt-and-braces record, not the primary guard.
pub struct ChildRunRequest {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
    pub system_prompt: String,
    pub tools: Vec<Box<dyn Tool>>,
    pub prompt: String,
    pub max_turns: usize,
    pub temperature: Option<f64>,
    pub thinking_budget: Option<u64>,
    pub tool_call_style: ToolCallStyle,
    pub session: Session,
    pub sink: Option<ChildSink>,
    pub depth: usize,
}

/// Drives one child Run to settlement and maps its [`Outcome`] to a
/// [`SubagentResult`] (P4/F4, ADR-0061). The re-entrant, self-contained Run
/// driver: it needs no Agent actor - it assembles a fresh child [`Conversation`]
/// (the system prompt plus the task as the first user message), a child
/// [`crate::tool_registry::ToolRegistry`] over the request's tool subset, a fresh
/// [`crate::tool::read_cache::FileReadCache`], child [`Capabilities`] (a denying
/// Approver, a declining Questioner, an [`crate::run::side_query::LlmSideQuery`]
/// over the child's own Llm/Model so web_fetch still works, and an
/// [`UnavailableSubagentSpawner`] - the RECURSION GUARD), a child
/// [`crate::tool::ToolCtx`], a [`ChildDeps`], and drives [`loop_::run`] with the
/// child turn bound. Every child effect that is not `complete` is a no-op or the
/// safe answer, so the child's run touches no Agent/Conversation state (ADR-0061).
pub async fn run_child(req: ChildRunRequest) -> SubagentResult {
    // Derive the child Session from the parent's: same Root, command timeout,
    // budget knobs, and Provider set, but the child's own turn bound and Model.
    let mut session = req.session;
    session.run_limit = req.max_turns as u64;
    session.model = req.model.clone();

    // The child Tool Registry over the request's narrowed tool subset (built-ins
    // minus the excluded set, per the def's selector). `with_shared` wants an
    // `Arc<[..]>`, so the boxed tool Vec converts into one.
    let tools: Arc<[Box<dyn Tool>]> = req.tools.into();
    let registry = Arc::new(crate::tool_registry::ToolRegistry::with_shared(Arc::clone(
        &tools,
    )));

    let read_cache = Arc::new(crate::tool::read_cache::FileReadCache::new());

    // The child SideQuery: the child's own Llm/Model boundary, so web_fetch's
    // prompt-guided extraction still works inside a subagent.
    let side_query: Arc<dyn crate::tool::caps::SideQuery> =
        Arc::new(crate::run::side_query::LlmSideQuery {
            llm: Arc::clone(&req.llm),
            model: req.model.clone(),
            temperature: None,
        });

    // The child Capabilities: a denying Approver + declining Questioner (a
    // foreground subagent has no interactive channel), the child SideQuery, and
    // the UnavailableSubagentSpawner - the recursion guard that makes a nested
    // `agent` call fail rather than spawn a grandchild.
    let caps = Capabilities {
        registry,
        read_cache,
        approver: Arc::new(DenyingApprover),
        side_query,
        questioner: Arc::new(DecliningQuestioner),
        // The subagent block (ADR-0061, ADR-0067): a child Run's plan-mode
        // capability is degraded, so `enter_plan_mode`/`exit_plan_mode` inside a
        // subagent return the VERBATIM qwen block result rather than mutating the
        // parent session's Approval mode. Plan mode is owned by the caller.
        plan_mode: Arc::new(SubagentPlanMode),
        subagents: Arc::new(UnavailableSubagentSpawner),
        // The recursion guard: a subagent's own background-shell capability is
        // degraded, so a subagent cannot background a shell (ADR-0063).
        bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        // A fresh `Default` mode mirror (ADR-0067): a subagent does not cycle the
        // Approval mode, and plan-mode entry inside a subagent is a no-op, so the
        // child never inherits the parent's live mode. `Default` means its gate
        // behaves normally (a child has degraded effect seams anyway).
        approval_mode: Arc::new(crate::approvals::AtomicApprovalMode::default()),
        // A fresh empty carrier (ADR-0067): a subagent does not cycle the mode, so
        // it never has a manual plan exit to notice - `shape_request` always takes
        // `None` here, injecting no manual-exit reminder in a child Run.
        plan_exit_notice: Arc::new(crate::approvals::PendingManualPlanExit::empty()),
    };

    let tool_ctx = session.tool_ctx(&req.model, caps);

    // The fresh child Conversation: the def's verbatim system prompt, seeded with
    // the task as the first user message. Budget figures derive from the child
    // Model like any Run (ADR-0037).
    let mut conversation = Conversation::new(
        req.system_prompt,
        ConversationOpts {
            compaction_slack: session.compaction_slack,
            compaction_keep: session.compaction_keep,
            ..ConversationOpts::new(
                session.context_budget_for(&req.model),
                session.reply_reserve_for(&req.model),
            )
        },
    );
    conversation.add_user_text(req.prompt);

    let mut deps = ChildDeps {
        llm: Arc::clone(&req.llm),
        model: req.model.clone(),
        temperature: req.temperature,
        thinking_budget: req.thinking_budget,
        tool_call_style: req.tool_call_style,
        sink: req.sink,
    };

    let outcome = loop_::run(
        conversation,
        &session,
        RunEnv {
            tool_ctx: &tool_ctx,
            // A child Run (subagent) fires no hooks in Phase 3a: the tool-dispatch
            // seam that fires them is the parent's, and a subagent has no standing
            // manager threaded. Phase 3b/4 revisits subagent-scoped firing.
            hooks: None,
            // A child Run activates no conditional skills either (ADR-0058): the
            // activation seam is the parent's, and a subagent has no shared skill
            // manager threaded.
            skill_activation: None,
        },
        &mut deps,
        RunOpts::default(),
    )
    .await;

    // `depth` is defence in depth alongside the degraded subagents capability;
    // it is recorded on the request and read here so the field is live.
    let _ = req.depth;

    outcome_to_result(outcome)
}

/// Maps a child Run's [`Outcome`] to a [`SubagentResult`] (qwen's
/// `AgentTerminateMode`, ADR-0061). `EndTurn`/`MaxTokens` -> `"GOAL"` with the
/// last assistant text; `RunLimit` -> `"MAX_TURNS"`; every other stop (a stuck
/// loop, a custom Hook stop, a failed Run, an exhausted budget) -> `"ERROR"`.
/// The `result` is the child's last assistant text with any trailing Voice close
/// marker stripped, so the parent never reads Suspenders' internal marker as the
/// subagent's answer. TIMEOUT/CANCELLED are DEFERRED with the background path.
fn outcome_to_result(outcome: Outcome) -> SubagentResult {
    match outcome {
        Outcome::Ok(conversation, stop) => {
            let result = last_assistant_text(&conversation);
            let terminate_reason = match stop {
                StopReason::EndTurn
                | StopReason::MaxTokens
                | StopReason::ToolUse
                | StopReason::StopSequence => "GOAL",
                StopReason::RunLimit => "MAX_TURNS",
                // A stuck loop, a failed/unknown stop, or any custom Hook stop
                // all read as ERROR (qwen collapses these to a non-GOAL
                // terminate mode the parent treats as a failure).
                StopReason::RunLimitStuck
                | StopReason::Error
                | StopReason::Unknown
                | StopReason::Custom(_) => "ERROR",
            };
            SubagentResult {
                terminate_reason: terminate_reason.to_string(),
                result,
            }
        }
        // The LLM error algebra closed the Run with the partial text; surface it
        // as the ERROR result with that partial text (qwen returns finalText on a
        // non-GOAL terminate).
        Outcome::Failed(_reason, conversation) => SubagentResult {
            terminate_reason: "ERROR".to_string(),
            result: last_assistant_text(&conversation),
        },
        // No Conversation came back: an exhausted budget (`Error`); `Down` is
        // the watcher's word and never the awaited Loop's return, but a total
        // match reads it as the failure it is.
        Outcome::Error(_) | Outcome::Down(_) => SubagentResult {
            terminate_reason: "ERROR".to_string(),
            result: String::new(),
        },
    }
}

/// The child's answer: the text blocks of the LAST assistant message, joined and
/// trimmed, with a trailing pure-Voice close marker dropped. `finish`/`close`
/// append a marker-only assistant message when the Loop closes a Run itself (the
/// Run-Limit/stall/stopped markers) or a `[turn failed]` marker on the error
/// path; that marker is Suspenders' internal signal, not the subagent's answer,
/// so a last assistant message that is ONLY a close marker is skipped in favour
/// of the model's real prior text.
fn last_assistant_text(conversation: &Conversation) -> String {
    let assistant_texts = |message: &Message| -> Option<String> {
        if message.role != Role::Assistant {
            return None;
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    // Walk assistant messages newest-first, skipping any whose only text is a
    // pure Voice close marker (a self-authored `[...]` marker with no model text).
    conversation
        .messages
        .iter()
        .rev()
        .filter_map(assistant_texts)
        .find(|text| !crate::voice::Marker::is_run_close(text))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/run.rs"]
mod child_tests;
