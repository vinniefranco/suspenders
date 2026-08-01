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
//! [`RunDeps`]. Tool execution (the Extension pipeline) runs in-loop over an
//! `extensions` list and a `ToolCtx` the caller supplies - the Rust Session
//! carries extension *names*, not `Registered` values, so these ride as
//! explicit `run` arguments (the shell builds them from the Session).

use crate::compaction::Compaction;
use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::extensions::Registered;
use crate::llm::response::{Response, StopReason};
use crate::llm::{LlmRequest, StreamEvent};
use crate::plan::Plan;
use crate::run::deps::{AfterPass, Emitter, RunDeps};
use crate::run::next_speaker::{self, NextSpeaker};
use crate::run::{batch, finish};
use crate::session::Session;
use crate::session::log;
use crate::tool::ToolCtx;
use crate::voice;

/// The Run loop's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The Run completed; carries the final Conversation and terminal stop
    /// reason.
    Ok(Conversation, OutcomeStop),
    /// The response errored; carries the LLM error reason and the Conversation
    /// (with the partial text and the failed marker).
    Failed(String, Conversation),
    /// The Context Budget was exhausted and Compaction could not recover it: no
    /// request was ever sent.
    Error,
}

/// The terminal stop reason of an `Ok` outcome. Spans the enumerable reasons
/// ([`log::StopReason`]: `end_turn`, `max_tokens`, `turn_limit`, ...) and the
/// arbitrary atom an after-Pass `Stop` hook may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeStop {
    Reason(log::StopReason),
    Custom(String),
}

impl OutcomeStop {
    // Used only by the test module's assertions, not by non-test builds.
    #[allow(dead_code)]
    fn end_turn() -> Self {
        OutcomeStop::Reason(log::StopReason::EndTurn)
    }
}

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
    pub(super) extensions: &'a [Registered],
    pub(super) tool_ctx: &'a ToolCtx,
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

/// The Extension pipeline and Tool execution context for one Run: always built
/// from Session data by the caller, passed together because they are always
/// produced together and belong together.
pub struct RunEnv<'a> {
    pub extensions: &'a [Registered],
    pub tool_ctx: &'a ToolCtx,
}

/// Runs the loop until the model stops asking for tools, the turn bound is hit,
/// or the response errors.
///
/// `env` bundles the Extension pipeline and Tool execution context (both
/// Session-derived; the Rust Session carries extension names only).
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
        extensions: env.extensions,
        tool_ctx: env.tool_ctx,
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

    conversation = maybe_compact_proactive(&mut state, conversation).await;
    conversation = drain_notifications_at_run_start(&mut state, conversation).await;
    run_loop(&mut state, conversation).await
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
        match state.deps.compact(conversation.clone()).await {
            Ok(compacted) => compacted,
            Err(_) => conversation,
        }
    } else {
        conversation
    }
}

async fn run_loop<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
) -> Outcome {
    loop {
        // The turn bound (replaces the Endgame Run-Limit close): once the turn
        // counter passes the bound, close the Run on the run-limit marker,
        // stop calling the model, and keep roles alternating.
        if state.turn > state.max_turns {
            return finish::close(
                state,
                conversation,
                voice::run_limit_marker(),
                log::StopReason::RunLimit,
            );
        }

        let (request, next_conv) = match build_request(state, conversation).await {
            Ok(pair) => pair,
            Err(()) => return Outcome::Error,
        };
        conversation = next_conv;

        state.emitter.emit(Event::message_start(state.turn as u32));

        let response = complete_and_emit(state, request).await;

        state.emitter.emit(Event::message_end(
            response.content.clone(),
            response.stop_reason.clone(),
        ));

        conversation.note_usage(response.usage.clone());
        emit_context_pressure(state, &conversation);

        match dispatch(state, conversation, response).await {
            Flow::Done(outcome) => return outcome,
            Flow::Continue(next) => conversation = next,
            // A malformed-tool-call re-draw (ADR-0030): re-issue the request
            // from the SAME, unmutated Conversation without advancing the
            // Pass - the failed draw produced nothing to keep and nothing for
            // the model to correct, so the retry is silent to the model.
            Flow::Retry(same) => conversation = same,
        }
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
        Ok(req) => {
            // The reveal-aware wire list off the Run's Tool Registry: deferred
            // tools the model has surfaced via `tool_search` this Run join the
            // list here, on the very next request. (agent.rs's overhead estimate
            // uses the BASE `tools::specs()` instead - reveals add token cost on
            // demand that the one-time estimate does not pre-count, matching
            // qwen. Don't "fix" that to read this list.)
            let request =
                LlmRequest::new(req.system, req.messages, state.tool_ctx.registry().specs());
            Ok((request, conversation))
        }
        Err(_) => {
            // Compaction recovery: try summarizing before giving up.
            match state.deps.compact(conversation.clone()).await {
                Ok(compacted) => Box::pin(build_request(state, compacted)).await,
                Err(_) => Err(()),
            }
        }
    }
}

// The result of a stop-reason dispatch: either the loop continues with an
// updated Conversation, or the Run is done.
pub(super) enum Flow {
    Continue(Conversation),
    Done(Outcome),
    /// A malformed-tool-call generation is re-drawn in-band (ADR-0030): the
    /// SAME, unmutated Conversation is re-requested without advancing the Pass -
    /// no batch to answer (no tool_use blocks were produced), so nothing
    /// enters the Conversation.
    Retry(Conversation),
}

// Dispatch keys on Tool Call PRESENCE, not the stop reason (qwen-code parity,
// the parity spec's core inversion): a response carrying ANY tool_use block
// continues the loop regardless of whether the stop reason was tool_use,
// end_turn, max_tokens, or unknown. Only two reasons override that: `Error`
// takes the error path (nothing executes), and `MaxTokens` with tool_use runs
// the truncated-batch re-issue (ADR-0009: the cut-off arguments may be
// incomplete, so nothing runs). Otherwise tool_use blocks execute. A response
// with NO tool_use blocks consults the next-speaker check before finishing.
async fn dispatch<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    if response.stop_reason == StopReason::Error {
        return error_flow(state, conversation, response);
    }

    let has_tool_use = response.content.iter().any(ContentBlock::is_tool_use);
    if has_tool_use {
        // A max_tokens stop mid-batch re-issues every call; every other stop
        // with tool_use executes them (the presence-gated continuation).
        if response.stop_reason == StopReason::MaxTokens {
            truncated_batch(state, conversation, response).await
        } else {
            continue_tools(state, conversation, response).await
        }
    } else if response.stop_reason == StopReason::MaxTokens {
        // A max_tokens stop with no tool_use is a truncation, not a completed
        // reply: it finishes with the truncation marker as before, NOT through
        // the next-speaker check (a cut-off reply must not auto-continue - it is
        // re-draw territory, ADR-0009).
        let content = response.content.clone();
        finish::finish(state, conversation, content, StopReason::MaxTokens)
    } else {
        // No Tool Calls and a normal completion: the point where the Run used to
        // always end. Consult the next-speaker check first - it may decide the
        // model should continue.
        no_tool_call(state, conversation, response).await
    }
}

// A Pass with no Tool Calls that completed normally (end_turn, a phantom
// tool_use stop with zero blocks, stop_sequence, or unknown). Unless the check
// is skipped, ask the next-speaker check who speaks next: `Model` injects a
// "Please continue." user message and keeps looping (bounded by `max_turns`
// through the loop's top-of-loop guard); `User` finishes the Run as before.
// `skip_next_speaker` short-circuits straight to finishing (the pre-check
// behavior).
async fn no_tool_call<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    let content = response.content.clone();

    if state.skip_next_speaker {
        return finish::finish(state, conversation, content, response.stop_reason);
    }

    match next_speaker::check_next_speaker(state.deps, &response).await {
        NextSpeaker::User => finish::finish(state, conversation, content, response.stop_reason),
        NextSpeaker::Model => continue_after_no_tool(state, conversation, response),
    }
}

// The next-speaker `Model` continuation (ADR-0043): append the model's reply as
// an assistant message stamped with its Provenance, then a "Please continue."
// user message (unstamped Voice string), announce it, and advance the turn.
// The continuation is BOUNDED by `max_turns`: the loop's top-of-loop
// `turn > max_turns` guard runs before the next request, so a model that keeps
// producing no-tool replies can loop at most `max_turns` times before the Run
// closes on the run-limit marker.
fn continue_after_no_tool<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
) -> Flow {
    // The reply enters stamped (it is the model's own content); a thinking-only
    // or empty reply contributes no speakable block, so nothing but the nudge
    // is appended in that case.
    let blocks: Vec<ContentBlock> = response
        .content
        .iter()
        .filter(|b| !b.is_tool_use() && !matches!(b, ContentBlock::Thinking { .. }))
        .cloned()
        .collect();
    if !blocks.is_empty() {
        conversation.add_assistant_response(blocks, state.deps.provenance());
    }

    conversation.add_user_text(voice::please_continue());
    state
        .emitter
        .emit(Event::steering_delivered(voice::please_continue()));
    state.deps.checkpoint(&conversation);

    state.turn += 1;
    Flow::Continue(conversation)
}

// ADR-0030: a StopReason::Error whose error string classifies as retryable -
// the malformed-tool-call class only - re-draws the generation in-band while
// the per-Run budget holds, instead of failing the whole Run. Increment the
// tally, emit the visible info event (the Agent folds it to a durable `retry`
// Session Log entry), and re-request the SAME, unmutated Conversation. On a
// non-retryable error, or once the budget is spent (default 3, 0 disables), the
// loud `finish::fail` runs.
fn error_flow<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    if response.is_retryable() && state.retries_used < state.malformed_retry_budget {
        state.retries_used += 1;
        let attempt = state.retries_used;
        let error = response.error.clone().unwrap_or_default();
        state
            .emitter
            .emit(Event::retry(error, attempt, state.malformed_retry_budget));
        Flow::Retry(conversation)
    } else {
        Flow::Done(finish::fail(state, conversation, response))
    }
}

// The model asked for tools and gets them, in the order it emitted them. The
// loop-detector runs first: a model stuck emitting the SAME batch terminates
// here, before the batch is executed again.
async fn continue_tools<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    // The loop-detector folds this Pass's Tool Call signature into the running
    // tally; a stall closes the Run here, injecting NO conversation text.
    if loop_break(state, &response) {
        state.emitter.emit(Event::loop_stall(state.identical_count));
        return Flow::Done(finish::close(
            state,
            conversation,
            voice::loop_stall_marker(),
            log::StopReason::RunLimitStuck,
        ));
    }
    let (results, conversation) =
        batch::execute_tools(state, conversation, &response.content).await;
    next_pass(state, conversation, response, results).await
}

// The loop-detector (qwen-style loop break): the passive circuit breaker. It
// folds this Pass's Tool Call batch signature into the running tally - a batch
// identical to the previous Pass's increments `identical_count`, a different
// one resets it to 1 and remembers the new signature. It returns `true` once
// the count reaches `identical_cap` (resolved from `session.loop_stall_limit`),
// meaning the model has emitted the SAME batch that many Passes in a row and is
// stuck. It NEVER mutates the Conversation and NEVER injects steering text; the
// caller terminates the Run on a `true`, appending only the close marker (the
// whole point of the passive design - contrast the deleted duplicate/failure
// nudges). With `identical_cap == 0` the detector is inert (a fresh count of 1
// never reaches 0).
fn loop_break<D: RunDeps>(state: &mut LoopState<'_, D>, response: &Response) -> bool {
    let signature = tool_signature(&response.content);

    if state.last_tool_signature.as_ref() == Some(&signature) {
        state.identical_count += 1;
    } else {
        state.last_tool_signature = Some(signature);
        state.identical_count = 1;
    }

    state.identical_cap > 0 && state.identical_count >= state.identical_cap
}

// The byte signature of a response's ToolUse blocks, in emission order: the
// name and canonical JSON input of each call. Two batches with the same calls
// in the same order share a signature.
fn tool_signature(blocks: &[ContentBlock]) -> Vec<u8> {
    let mut sig = Vec::new();
    for block in blocks.iter().filter(|b| b.is_tool_use()) {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            sig.extend_from_slice(name.as_bytes());
            sig.push(0);
            sig.extend_from_slice(input.to_string().as_bytes());
            sig.push(0);
        }
    }
    sig
}

// ADR-0009: a max_tokens stop cut the response mid-batch. The streamed
// arguments may be valid-but-incomplete JSON, so NOTHING executes; every call
// is answered with the re-issue error and the model retries in-band. The calls
// never enter the duplicate memory.
async fn truncated_batch<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    let mut results = Vec::new();
    for block in response.content.iter().filter(|b| b.is_tool_use()) {
        if let ContentBlock::ToolUse { id, name, input } = block {
            state.emitter.emit(Event::tool_call(
                id.clone(),
                name.clone(),
                batch::display_input(input),
            ));

            let content = voice::truncated_call_reissue().to_string();
            state.emitter.emit(Event::tool_result(
                id.clone(),
                name.clone(),
                content.clone(),
                true,
                Default::default(),
            ));

            results.push(ContentBlock::tool_result(id.clone(), content, true));
        }
    }

    next_pass(state, conversation, response, results).await
}

// Shared tail of every tool-answering Pass: drain Steering, append the batch
// (assistant blocks intact, results + Steering as ONE user message),
// checkpoint, then the after-Pass hook, then advance the turn.
async fn next_pass<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
    results: Vec<ContentBlock>,
) -> Flow {
    let steering = state.deps.drain_steering().await;

    // Background subagent notifications (P4b, ADR-0063): the PARALLEL channel to
    // Steering. A settled/cancelled background child queued its
    // `<task-notification>` on the Agent; drain them here and merge each into the
    // SAME tool-results user message Steering rides, so the model reads a
    // completion on its very next request. Not steering (no user voice) - just
    // trailing text blocks on the batch, and an operator-visible Event per note.
    let notifications = state.deps.drain_notifications().await;

    // The batch enters stamped with the captured Model's Provenance
    // (ADR-0037): the request-shaping transform reads it to decide verbatim
    // replay vs cross-Provider normalization.
    conversation.add_assistant_response(response.content.clone(), state.deps.provenance());
    // Steering and notifications ride the batch as trailing user-role text blocks
    // (the same shape), Steering first then the notifications.
    let mut trailing = steering.clone();
    trailing.extend(notifications.iter().cloned());
    conversation.add_tool_results(results, trailing);

    for text in &steering {
        state.emitter.emit(Event::steering_delivered(text.clone()));
    }
    for text in &notifications {
        state
            .emitter
            .emit(Event::background_notification(text.clone()));
    }

    state.deps.checkpoint(&conversation);

    match state.deps.after_pass(&response, &conversation).await {
        AfterPass::Continue => {
            state.turn += 1;
            Flow::Continue(conversation)
        }
        AfterPass::Stop(reason) => Flow::Done(finish::close_custom(
            state,
            conversation,
            voice::run_stopped_marker(),
            reason,
        )),
        AfterPass::Inject(text) => {
            conversation.merge_user_text(text);
            state.turn += 1;
            Flow::Continue(conversation)
        }
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
