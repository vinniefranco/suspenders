//! Run Loop - the inner tool-call loop of a Run.
//! (Module name is `loop_` because `loop` is a keyword - ADR-0022.)
//!
//! One Pass (CONTEXT.md) = one model response plus the Tool Calls it carries.
//! Per Pass the loop emits a well-formed message grammar - `MessageStart`,
//! `MessageUpdate` (delta + accumulated snapshot), `MessageEnd` - on every path,
//! including errored responses, then acts on the stop reason.
//!
//! This module keeps the loop skeleton: the Pass cycle (request, stream,
//! dispatch), proactive Compaction at Run start, the tool-answering tail
//! (Steering, checkpoint, after-Pass hook), and a plain turn bound. Executing a
//! Pass's Tool Call batch lives in [`super::batch`]; how a Run ends when the
//! model stops calling tools lives in [`super::finish`].
//!
//! The Loop owns zero I/O and zero process concerns: every effect goes through
//! [`RunDeps`]. Tool execution runs in-loop over a `ToolCtx` the caller
//! supplies; each tool shapes its own output and attaches its own display
//! Artifacts (ADR-0007), so there is no wrapper pipeline.

use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::response::Response;
use crate::llm::{LlmRequest, StreamEvent};
use crate::plan::Plan;
use crate::run::deps::{Emitter, RunDeps};
use crate::run::settlement::{Outcome, Reason};
use crate::run::{dispatch, finish};
use crate::session::Session;
use crate::tool::ToolCtx;
use crate::voice;

/// Options for [`run`]: the restored Plan content and the durable original
/// task copy from the Compaction state.
#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    pub plan: Option<String>,
    pub original_task: Option<String>,
}

// The loop state that spans Passes: the effect bundle, the owned emission
// handle (obtained once from `deps.emitter()`, ADR-0025), the Plan state, the
// turn counter and its bound, the malformed re-draw budget/tally, and the
// loop-detector state. Fields are `pub(super)` so `batch` and `finish`
// work on the state directly.
pub(super) struct LoopState<'a, D: RunDeps> {
    pub(super) deps: &'a mut D,
    pub(super) emitter: Emitter,
    pub(super) tool_ctx: &'a ToolCtx,
    // The Run's hook firing handle (Phase 3a, ADR-0066), threaded from RunEnv so
    // `batch` can fire the four tool events. `None` for a Run that fires no hooks.
    pub(super) hooks: Option<&'a crate::run::hooks::Hooks<'a>>,
    // A hook's `continue:false` halt requested during a tool batch (Phase 3a,
    // ADR-0066): the minimal Stop thread. `batch` records the reason here when a
    // Pre/PostToolUse hook returns `continue:false`; `dispatch::continue_tools`
    // reads it AFTER the batch answers and closes the Run through the same
    // `close_custom` path an after-Pass `Stop` takes.
    pub(super) hook_stop: Option<String>,
    // The conditional-skill activation seam (ADR-0058), threaded from RunEnv so
    // `batch` can activate a conditional skill by the touched file path at the
    // tool-success seam. `None` for a Run that activates no skills (child/test).
    pub(super) skill_activation: Option<SkillActivation>,
    // The Stop-hook continuation COUNTER (Phase 3b, ADR-0066; A2, qwen's
    // `stopHookState.iterationCount`): how many times a Stop hook has forced a
    // continuation this Run. The finish path (`dispatch::finish_or_stop_hook`) fires
    // Stop while `stop_hook_count < stop_hook_cap`, increments on each forced
    // continuation, and at the cap emits qwen's
    // `formatStopHookBlockingCapWarning` then STOPS - so a Stop hook that always
    // blocks forces at most `cap` extra Passes (qwen default 8), never an infinite
    // loop, and a legitimate stop (no forcing hook) is never wrongly blocked.
    pub(super) stop_hook_count: u64,
    // The resolved Stop-hook continuation cap (A2, qwen's
    // `resolveStopHookBlockingCap`): env `SUSPENDERS_STOP_HOOK_BLOCK_CAP` (clamped
    // to 1..=100) else the default 8. Resolved once at Run start.
    pub(super) stop_hook_cap: u64,
    pub(super) plan: Plan,
    // The current Pass number, 1-based (replaces the Ledger's `pass()`).
    pub(super) turn: u64,
    // The turn bound, resolved from `session.run_limit` at Run start: the loop
    // closes on the run-limit marker once `turn` exceeds it.
    pub(super) max_turns: u64,
    // The malformed-tool-call re-draw Setpoint (ADR-0030), resolved once from
    // the Session: how many in-band re-draws a retryable generation error may
    // trigger this Run (0 disables it).
    pub(super) malformed_retry_budget: u64,
    // How many re-draws have been spent this Run (replaces the Ledger's
    // `retries_used()`/`note_retry()`).
    pub(super) retries_used: u64,
    // Loop-detector state (qwen-style loop break): the passive circuit breaker.
    // `last_tool_signature` is the byte image of the previous Pass's Tool Call
    // batch; `identical_count` is how many Passes in a row have carried that
    // same batch (1 on a fresh signature). When the count reaches
    // `identical_cap` the Run terminates - injecting NO steering text, only an
    // Event and the close marker. `identical_cap` is resolved from
    // `session.loop_stall_limit` at Run start.
    pub(super) last_tool_signature: Option<Vec<u8>>,
    pub(super) identical_count: u64,
    pub(super) identical_cap: u64,
    // Skips the next-speaker check (ADR-0043), resolved from
    // `session.skip_next_speaker` at Run start: when `true` a no-tool-call Pass
    // finishes the Run as it did before the check existed.
    pub(super) skip_next_speaker: bool,
}

/// The Tool execution context for one Run: the [`ToolCtx`] the caller builds
/// from Session data, plus the Run's optional hook firing handle (Phase 3a,
/// ADR-0066). A struct (not a bare `&ToolCtx`) so a Run-scoped input can join it
/// without churning every call site - the [`crate::run::hooks::Hooks`] handle is
/// exactly the input this comment anticipated.
pub struct RunEnv<'a> {
    pub tool_ctx: &'a ToolCtx,
    /// The Run's hook firing handle (Phase 3a, ADR-0066), or `None` for a Run that
    /// fires no hooks (a child/subagent Run, or a test that does not wire them).
    /// The tool-dispatch seam ([`crate::run::batch`]) fires the four tool events
    /// through it; `None` is the fire-nothing path.
    pub hooks: Option<&'a crate::run::hooks::Hooks<'a>>,
    /// The conditional-skill activation seam (ADR-0058): the shared skill manager
    /// and the Project Root a touched file path is resolved against. `None` for a
    /// Run that activates no skills (a child/subagent Run, or a test). The
    /// tool-success seam ([`crate::run::batch`]) calls
    /// [`crate::skills::SkillManager::activate_by_path`] through it.
    pub skill_activation: Option<SkillActivation>,
}

/// The conditional-skill activation input for one Run (ADR-0058): the shared
/// [`crate::skills::SkillManager`] a Run activates a conditional skill on, plus
/// the Project Root a touched file path is resolved relative to. Bundled because
/// the two are always used together, at the one seam ([`crate::run::batch`]), and
/// carried by `Arc`/owned so the [`LoopState`] holds no extra borrow. The manager
/// is shared with the `skill` tool, so an activation this Run makes shows up in
/// the tool's next catalog build.
#[derive(Clone)]
pub struct SkillActivation {
    /// The shared skill manager; its interior activation registry is the mutated
    /// state.
    pub skills: std::sync::Arc<crate::skills::SkillManager>,
    /// The Project Root a touched path is resolved against (conditional skills
    /// are project-scoped).
    pub project_root: std::path::PathBuf,
}

/// Runs the loop until the model stops asking for tools, the turn bound is hit,
/// or the response errors.
///
/// `env` carries the Tool execution context (Session-derived).
pub async fn run<D: RunDeps>(
    mut conversation: Conversation,
    session: &Session,
    env: RunEnv<'_>,
    deps: &mut D,
    opts: RunOpts,
) -> Outcome {
    let plan = Plan::new(opts.plan, opts.original_task).capture_task(&conversation);

    // The emission handle, detached from the deps once so the streaming sink
    // can emit while `complete` borrows them (ADR-0025).
    let emitter = deps.emitter();

    let mut state = LoopState {
        deps,
        emitter,
        tool_ctx: env.tool_ctx,
        hooks: env.hooks,
        hook_stop: None,
        skill_activation: env.skill_activation,
        stop_hook_count: 0,
        stop_hook_cap: crate::run::hooks::resolve_stop_hook_cap(),
        plan,
        turn: 1,
        max_turns: session.run_limit,
        malformed_retry_budget: session.malformed_retry_budget,
        retries_used: 0,
        // Loop-detector state: the identical-batch cap comes from the Session
        // (`loop_stall_limit`); the count starts fresh with no prior signature.
        last_tool_signature: None,
        identical_count: 0,
        identical_cap: session.loop_stall_limit,
        skip_next_speaker: session.skip_next_speaker,
    };

    // The UserPromptSubmit seam (Phase 3b, ADR-0066): fire before the model sees
    // the prompt. A blocking hook vetoes the Run (the prompt never reaches the
    // model); an injecting hook prepends its additionalContext as a leading user
    // turn. Returns `Err(outcome)` when the Run is vetoed here.
    conversation = match fire_user_prompt_submit(&mut state, conversation).await {
        Ok(conv) => conv,
        Err(outcome) => return outcome,
    };

    conversation = maybe_compact_proactive(&mut state, conversation).await;
    conversation = drain_notifications_at_run_start(&mut state, conversation).await;
    run_loop(&mut state, conversation).await
}

// The UserPromptSubmit fire (Phase 3b, ADR-0066): fired once at Run start, before
// the first request, so a hook can veto or enrich the submitted prompt. The
// submitted prompt is the Conversation's LAST user text (a fresh Run appended it;
// a next-Pass Run has no fresh prompt, so the last user text is still the most
// recent thing the user said). A Reject closes the Run through the custom-stop
// path (the prompt is never sent); a Proceed with additionalContext merges it
// onto the trailing user message so the model reads the hook's note with the
// prompt. Every deciding fire is surfaced visibly (ADR-0018); a firing error is
// fail-open (no hooks wired, or a runner failure, leaves the prompt untouched).
async fn fire_user_prompt_submit<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
) -> Result<Conversation, Outcome> {
    let Some(hooks) = state.hooks else {
        return Ok(conversation);
    };
    let Some(prompt) = last_user_text(&conversation) else {
        return Ok(conversation);
    };

    match hooks.user_prompt_submit(&prompt).await {
        crate::run::hooks::UserPromptDecision::Reject { reason, context } => {
            emit_hook_decision(
                state,
                "UserPromptSubmit",
                &format!("rejected the prompt: {reason}"),
            );
            // The vetoed prompt never reaches the model: close the Run on the
            // hook's reason through the custom-stop path a Stop hook takes, so the
            // Run ends with the hook's explanation. Any additionalContext the
            // blocking hook still carried rides the closing marker text.
            let marker = match context {
                Some(ctx) => format!("{}\n{ctx}", voice::Marker::RunStopped.text()),
                None => voice::Marker::RunStopped.text().to_string(),
            };
            Err(finish::close_custom(state, conversation, &marker, reason))
        }
        crate::run::hooks::UserPromptDecision::Proceed { context } => {
            if let Some(ctx) = context {
                emit_hook_decision(state, "UserPromptSubmit", "injected additional context");
                conversation.merge_user_text(ctx);
            }
            Ok(conversation)
        }
    }
}

// The Conversation's last user text (the submitted prompt at Run start): the
// text of the trailing user message, or `None` when the last message is not a
// user text block. Used only by the UserPromptSubmit fire.
fn last_user_text(conversation: &Conversation) -> Option<String> {
    conversation.messages.iter().rev().find_map(|m| {
        if m.role != crate::content::Role::User {
            return None;
        }
        match m.content.first() {
            Some(crate::content::ContentBlock::Text { text }) => Some(text.clone()),
            _ => None,
        }
    })
}

// Surfaces a deciding hook fire as a visible line (ADR-0018 fail-open-with-
// visibility, ADR-0066): a block / auto-approve / deny / stop / inject /
// force-continue is never a silent decision. Reuses the fail-open report seam
// skills/MCP use (a `fail_open_report` labelled `hook <event>`), so the
// operator reads what a hook did on the same channel a launch notice takes.
// Shared with `batch`, which surfaces the tool-event fires through it.
pub(super) fn emit_hook_decision<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    event: &str,
    what: &str,
) {
    state
        .emitter
        .emit(Event::fail_open_report(format!("hook {event}"), what));
}

// Run-start notification drain (P4b, ADR-0063): a background child can settle
// while the parent is idle (no Run in flight), so a `<task-notification>` sits
// on the Agent's queue between Runs. `next_pass` only drains at a tool-answering
// Pass, so a next Run that answers with pure text (no tool call) would never
// deliver the queued note. Draining here, once at Run start, merges any pending
// notifications into the FIRST request's user turn (the same trailing-text shape
// `next_pass` uses for the tool-results message), so the model reads the
// completion on its very NEXT Run regardless of whether that Run calls tools.
// Drained-and-cleared once: the Agent empties the queue on this drain, so the
// same note is never re-delivered by a later `next_pass`.
async fn drain_notifications_at_run_start<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
) -> Conversation {
    let notifications = state.deps.drain_notifications().await;
    for text in notifications {
        // Merge onto the trailing user message (the Run's prompt) without
        // breaking role alternation, then announce it, exactly as `next_pass`
        // does for a between-Passes drain.
        state
            .emitter
            .emit(Event::background_notification(text.clone()));
        conversation.merge_user_text(text);
    }
    conversation
}

// Proactive Compaction (ADR-0012): when the Conversation already exceeds the
// compaction target at Run start, compact before the first Pass. A failed
// Compaction falls through to the reactive path at the budget cliff.
async fn maybe_compact_proactive<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Conversation {
    if Compaction::proactive(&conversation) {
        match compact_with_hooks(state, conversation.clone()).await {
            Ok(compacted) => compacted,
            Err(_) => conversation,
        }
    } else {
        conversation
    }
}

// The Pre/PostCompact seam (Phase 3b, ADR-0066): fire PreCompact before the
// compaction service runs and PostCompact after it produces a summary. PreCompact
// may inject a custom instruction (surfaced visibly); PostCompact is observe-only
// (matching qwen). Both bracket the `compact` Dep - the SINGLE place the loop
// invokes compaction - so proactive and reactive compaction share one fire path.
// A firing error is fail-open (the compaction proceeds regardless).
async fn compact_with_hooks<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Result<Conversation, crate::run::deps::CompactError> {
    if let Some(hooks) = state.hooks
        && hooks.pre_compact().await.is_some()
    {
        emit_hook_decision(state, "PreCompact", "injected a compaction instruction");
    }
    let result = state.deps.compact(conversation).await;
    if let Some(hooks) = state.hooks
        && result.is_ok()
    {
        hooks.post_compact().await;
    }
    result
}

// The loop skeleton (integration, IOSP): it owns only the Pass cycle and the
// turn bound, threading the Conversation between Passes and mapping each Pass's
// `PassStep` back to the loop's control flow. Every decision inside a Pass -
// build the request, complete, dispatch the stop reason - lives in `run_pass`.
async fn run_loop<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
) -> Outcome {
    loop {
        // The Run Limit (CONTEXT.md): once the turn counter passes the bound,
        // close the Run on the run-limit marker, stop calling the model, and
        // keep roles alternating.
        if state.turn > state.max_turns {
            return finish::close(
                state,
                conversation,
                voice::Marker::RunLimit.text(),
                crate::stop_reason::StopReason::RunLimit,
            );
        }

        match run_pass(state, conversation).await {
            PassStep::Done(outcome) => return outcome,
            // Continue and Retry both thread a Conversation back into the loop;
            // the Pass has already advanced (or deliberately not advanced) the
            // turn, so the skeleton just carries the value forward.
            PassStep::Advance(next) => conversation = next,
        }
    }
}

// One Pass's step, from the loop skeleton's view: either the Run is done, or the
// loop carries a Conversation into the next iteration. It flattens `Flow`'s
// Continue/Retry (which differ only inside the Pass - whether the turn advanced)
// into one Advance the skeleton treats uniformly.
enum PassStep {
    Done(Outcome),
    Advance(Conversation),
}

// One Pass (operation, IOSP): build the request (Compaction may rewrite the
// Conversation, or exhaustion may end the Run), stream the response with its
// well-formed message grammar, note the usage, then dispatch on the stop reason.
// The only logic here is the request-build result and the `Flow` mapping; the
// steps are delegated calls.
async fn run_pass<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> PassStep {
    let (request, mut conversation) = match build_request(state, conversation).await {
        Ok(pair) => pair,
        // Even Compaction could not fit the request: the Loop names the one
        // reason it can produce this outcome for.
        Err(()) => return PassStep::Done(Outcome::Error(Reason::atom("context_budget_exhausted"))),
    };

    state.emitter.emit(Event::message_start(state.turn as u32));
    let response = complete_and_emit(state, request).await;
    state.emitter.emit(Event::message_end(
        response.content.clone(),
        response.stop_reason.clone(),
    ));

    conversation.note_usage(response.usage.clone());
    emit_context_pressure(state, &conversation);

    match dispatch::dispatch(state, conversation, response).await {
        dispatch::Flow::Done(outcome) => PassStep::Done(outcome),
        dispatch::Flow::Continue(next) => PassStep::Advance(next),
        // A malformed-tool-call re-draw (ADR-0030): re-issue from the SAME,
        // unmutated Conversation without advancing the Pass - the failed draw
        // produced nothing to keep and nothing for the model to correct, so the
        // retry is silent to the model.
        dispatch::Flow::Retry(same) => PassStep::Advance(same),
    }
}

// The invariant: streaming deltas are emitted AS THEY STREAM - every
// MessageUpdate goes out between MessageStart and MessageEnd, DURING the
// `complete` call, never buffered until it returns (ADR-0025). Destructuring
// `state` borrows the disjoint `deps` and `emitter` fields, so the sink emits
// live while the model call holds the deps.
async fn complete_and_emit<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    request: LlmRequest,
) -> Response {
    let LoopState { deps, emitter, .. } = state;
    let mut sink = |ev: &StreamEvent| {
        emitter.emit(Event::message_update(ev.delta.clone(), ev.content.clone()));
    };
    deps.complete(request, &mut sink).await
}

// Returns `Ok((request, conversation))` (Compaction may have rewritten the
// Conversation) or `Err(())` for context-budget exhaustion. The wire tool list
// is the Run Registry's reveal-aware projection (ADR-0054): every non-deferred
// tool plus any deferred tool the model has surfaced via `tool_search` this
// Run. There is no per-Pass narrowing beyond that reveal gate.
async fn build_request<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Result<(LlmRequest, Conversation), ()> {
    match conversation.for_request() {
        Ok(req) => Ok((shape_request(state, req), conversation)),
        // Over budget: reactively compact and retry (the last recovery before
        // the Run fails). The recovery owns the compaction + recursion so this
        // function stays a flat fit-check dispatch.
        Err(_) => compact_and_retry(state, conversation).await,
    }
}

// The wire request off a fitting Conversation snapshot: the reveal-aware tool
// list off the Run's Tool Registry (deferred tools the model surfaced via
// `tool_search` this Run join here, on the very next request). agent.rs's
// overhead estimate uses the BASE `tools::specs()` instead - reveals add token
// cost on demand the one-time estimate does not pre-count, matching qwen. Don't
// "fix" that to read this list.
fn shape_request<D: RunDeps>(
    state: &LoopState<'_, D>,
    req: crate::conversation::Request,
) -> LlmRequest {
    let system = shape_plan_mode_system(state, req.system);
    LlmRequest::new(system, req.messages, state.tool_ctx.registry().specs())
}

// The ephemeral plan-mode request-shaping Voice (ADR-0067, qwen's client.ts
// inject-every-Pass + geminiChat.ts one-shot manual-exit): appends the plan-mode
// reminders to the request's system text WITHOUT touching the Conversation or
// the Session Log. Reached once per Pass (a compaction retry recurses through
// `build_request` but only the terminal fitting call reaches `shape_request`),
// so the take-and-clear below injects the manual-exit notice into exactly one
// request.
//
//  - While the live mode is Plan: append `plan_mode_reminder` (qwen re-injects
//    `getPlanModeSystemReminder` into EVERY request in Plan - a small model
//    drifts without the standing read-only reminder, client.ts:2915).
//  - When a manual-exit notice is pending (the user left Plan via Shift+Tab):
//    TAKE-and-clear it and append `manual_plan_exit_reminder(mode)` ONCE (qwen's
//    `takePendingManualPlanExitNotice`, geminiChat.ts:2384). The take clears the
//    carrier, so the notice rides exactly one request. The two are mutually
//    exclusive - a pending notice means the mode already left Plan - so at most
//    one reminder is appended.
//
// The one impurity (the take-and-clear) IS the one-shot semantics, modelled as
// the carrier's own `take`; `shape_request` stays otherwise pure.
fn shape_plan_mode_system<D: RunDeps>(state: &LoopState<'_, D>, system: String) -> String {
    let caps = &state.tool_ctx.caps;
    if caps.approval_mode.load() == crate::approvals::ApprovalMode::Plan {
        return format!("{system}\n\n{}", crate::voice::plan_mode_reminder());
    }
    if let Some(mode) = caps.plan_exit_notice.take() {
        let reminder = crate::voice::manual_plan_exit_reminder(mode.wire_str());
        return format!("{system}\n\n{reminder}");
    }
    system
}

// Reactive Compaction recovery (ADR-0012): summarize the over-budget
// Conversation, then rebuild the request from the compacted snapshot; a failed
// Compaction is the exhaustion `Err(())` the Run fails on.
async fn compact_and_retry<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Result<(LlmRequest, Conversation), ()> {
    match compact_with_hooks(state, conversation.clone()).await {
        Ok(compacted) => Box::pin(build_request(state, compacted)).await,
        Err(_) => Err(()),
    }
}

// Live context-pressure indication, once the Pass's usage is noted.
fn emit_context_pressure<D: RunDeps>(state: &mut LoopState<'_, D>, conversation: &Conversation) {
    state.emitter.emit(Event::context_pressure(
        conversation.token_estimate(),
        conversation.context_budget,
        conversation.max_tokens_reserve,
    ));
}

#[cfg(test)]
// Test fixtures build SessionOpts by mutating a `default()` value one field at a
// time; the struct-literal form clippy wants would obscure which knob each test
// sets. Narrowly scoped to this test module.
#[allow(clippy::field_reassign_with_default)]
#[path = "../../tests/run/loop_.rs"]
mod tests;
