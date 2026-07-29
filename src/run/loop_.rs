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
    run_loop(&mut state, conversation).await
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
// Conversation) or `Err(())` for context-budget exhaustion. The FULL Tool
// registry rides every request - there is no per-Pass narrowing.
async fn build_request<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Result<(LlmRequest, Conversation), ()> {
    match conversation.for_request() {
        Ok(req) => {
            let request = LlmRequest::new(req.system, req.messages, crate::tools::specs());
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

    // The batch enters stamped with the captured Model's Provenance
    // (ADR-0037): the request-shaping transform reads it to decide verbatim
    // replay vs cross-Provider normalization.
    conversation.add_assistant_response(response.content.clone(), state.deps.provenance());
    conversation.add_tool_results(results, steering.clone());

    for text in &steering {
        state.emitter.emit(Event::steering_delivered(text.clone()));
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
mod tests {
    use super::*;
    use crate::content::Usage;
    use crate::content::{ContentBlock, Role};
    use crate::event::Stage;
    use crate::extensions::Registered;
    use crate::llm::model::{Api, Model};
    use crate::llm::response::Response;
    use crate::llm::{Delta, malformed_input_marker};
    use crate::middleware::{Middleware, Token};
    use crate::run::deps::CompactError;
    use crate::run::fixtures::{
        FakeDeps, conversation, count_voiced, deps_for, empty, events, find_tool_result, just,
        last_message, next_speaker_verdict, ok, root, run_with, session, session_next_speaker,
        session_with, session_with_limit, text_end, text_result, tool_ctx, tool_use_result, write,
    };
    use crate::session::{Session, SessionOpts};
    use crate::test_support::Entry;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    // The harness fixtures (session builders, Response builders, `run_with`,
    // event inspectors) live in `crate::run::fixtures`, one set for the split
    // Loop's tests (these integration tests cover `batch` and `finish` too).

    // ---- tool loop --------------------------------------------------------

    #[tokio::test]
    async fn runs_the_tool_emits_events_checkpoints_and_feeds_result_back() {
        let root = root();
        write(&root, "marker.txt", "");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("tu_1", "list_files", json!({"path": "."}))),
                just(text_end("Here are the files.")),
            ],
        );

        let (outcome, deps) = run_with(&session, "list the files", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());

        let evs = events(&deps);
        // tool_call for tu_1
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::ToolCall { id, name, input }
            if id == "tu_1" && name == "list_files" && input == &json!({"path": "."})))
        );
        // tool_result for tu_1, not error, listing contains marker.txt
        let listing = evs.iter().find_map(|e| match e {
            Event::ToolResult {
                id,
                is_error,
                content,
                ..
            } if id == "tu_1" => {
                assert!(!is_error);
                Some(content.clone())
            }
            _ => None,
        });
        assert!(listing.unwrap().contains("marker.txt"));

        // The checkpoint after the result holds the answered pair.
        let checkpoints = deps.checkpoints.lock().unwrap();
        let cp = checkpoints.first().expect("a checkpoint");
        let tail = &cp.messages[cp.messages.len() - 2..];
        assert!(matches!(&tail[0].role, Role::Assistant));
        assert!(matches!(&tail[0].content[0], ContentBlock::ToolUse { id, .. } if id == "tu_1"));
        assert!(matches!(&tail[1].role, Role::User));
        assert!(
            matches!(&tail[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tu_1")
        );

        // The second request carried tools and the tool_result went back.
        let requests = deps.requests.lock().unwrap();
        let second = &requests[1];
        assert!(!second.tools.is_empty());
        let last = second.messages.last().unwrap();
        assert!(matches!(&last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "tu_1" && !is_error)
        );

        // The conversation ends on the model's reply.
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "Here are the files.")
        );
    }

    #[tokio::test]
    async fn a_multi_tool_pass_checkpoints_once_with_the_whole_answered_batch() {
        let root = root();
        write(&root, "a.txt", "");
        write(&root, "b.txt", "");
        let session = session(root.path());
        // One Pass emitting two Tool Calls: the batch is checkpointed once, after
        // both are answered (per-batch, not per-tool - ADR-0010's per-event
        // tool_result log entries carry crash recency; this checkpoint is only
        // the settlement fallback).
        let two_tool_pass = Response {
            content: vec![
                ContentBlock::tool_use("tu_1", "list_files", json!({"path": "."})),
                ContentBlock::tool_use("tu_2", "list_files", json!({"path": "."})),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![Entry::just(two_tool_pass), just(text_end("Done."))],
        );

        let (outcome, deps) = run_with(&session, "list twice", deps).await;
        ok(&outcome);

        let checkpoints = deps.checkpoints.lock().unwrap();
        // Exactly one checkpoint for the two-tool batch (plus the finish
        // checkpoint on end-of-Run) - never one per tool.
        assert_eq!(checkpoints.len(), 2, "one per batch, not one per tool");

        // The batch checkpoint carries both answered Tool Calls paired with
        // their results, and no unanswered tool_use block.
        let cp = &checkpoints[0];
        let tail = &cp.messages[cp.messages.len() - 2..];
        let assistant = &tail[0];
        assert!(matches!(assistant.role, Role::Assistant));
        let tool_use_ids: Vec<&str> = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_use_ids, vec!["tu_1", "tu_2"]);

        let user = &tail[1];
        assert!(matches!(user.role, Role::User));
        let result_ids: Vec<&str> = user
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(result_ids, vec!["tu_1", "tu_2"]);
    }

    // ---- Provenance stamping (ADR-0037) ------------------------------------

    #[tokio::test]
    async fn assistant_messages_enter_stamped_with_the_captured_models_provenance() {
        let root = root();
        write(&root, "marker.txt", "");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );

        let (outcome, _deps) = run_with(&session, "go", deps).await;
        let (conv, _) = ok(&outcome);

        // Both the tool-answering Pass and the finish reply are stamped with
        // the Run's captured Model; user messages carry no Provenance.
        let expected = Some(session.model.provenance());
        let assistants: Vec<_> = conv
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect();
        assert_eq!(assistants.len(), 2);
        for m in &assistants {
            assert_eq!(m.provenance, expected);
        }
        assert!(
            conv.messages
                .iter()
                .filter(|m| m.role == Role::User)
                .all(|m| m.provenance.is_none())
        );
    }

    #[tokio::test]
    async fn a_voice_authored_close_marker_carries_no_provenance() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "t1",
                "list_files",
                json!({"path": "."}),
            ))],
        )
        .with_after_pass(|_r, _c| AfterPass::Stop("budget_hook".to_string()));

        let (outcome, _deps) = run_with(&session, "look", deps).await;
        let (conv, _) = ok(&outcome);
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn stopped - reply to continue]")
        );
        assert_eq!(lm.provenance, None, "the Voice's marker is not the model's");
    }

    #[tokio::test]
    async fn emits_message_grammar_per_pass_including_errored_responses() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![Entry::response(
                vec![
                    Delta::Thinking("hm".into()),
                    Delta::Text("hi ".into()),
                    Delta::Text("there".into()),
                ],
                text_end("hi there"),
            )],
        );

        let (outcome, deps) = run_with(&session, "hello", deps).await;
        ok(&outcome);
        let evs = events(&deps);

        assert!(matches!(evs[0], Event::MessageStart { pass: 1 }));
        // First update: thinking delta + snapshot with thinking block.
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::MessageUpdate { delta, content }
            if *delta == Delta::Thinking("hm".into())
            && matches!(content.first(), Some(ContentBlock::Thinking { text }) if text == "hm")))
        );
        // A text delta "hi ".
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::MessageUpdate { delta, .. }
            if *delta == Delta::Text("hi ".into())))
        );
        // "there" update: snapshot has accumulated "hi there".
        assert!(evs.iter().any(|e| matches!(e, Event::MessageUpdate { delta, content }
            if *delta == Delta::Text("there".into())
            && content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "hi there")))));
        // message_end with text content and end_turn.
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::MessageEnd { content, stop_reason }
            if stop_reason == &StopReason::EndTurn
            && matches!(content.first(), Some(ContentBlock::Text { .. }))))
        );
    }

    #[tokio::test]
    async fn streaming_updates_are_emitted_live_during_complete_not_after() {
        let root = root();
        let session = session(root.path());

        // A shared events log created UP FRONT, so the Dynamic entry can drop a
        // sentinel into it from INSIDE `complete` - after every delta has gone
        // through the streaming sink, immediately before `complete` returns.
        // If the loop buffered deltas and emitted after the call (the defect
        // ADR-0025 removes), every MessageUpdate would land AFTER the sentinel.
        let events_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let sentinel = "__complete_returning__";
        let sentinel_log = Arc::clone(&events_log);
        let entry = Entry::dynamic(
            vec![Delta::Text("hi ".into()), Delta::Text("there".into())],
            move |_req, _model| {
                sentinel_log
                    .lock()
                    .unwrap()
                    .push(Event::steering_delivered(sentinel));
                text_end("hi there")
            },
        );

        let mut deps = deps_for(&session, vec![entry]);
        // Point the fake's recorder at the pre-shared log; the Emitter it hands
        // out clones this same Arc, so updates and sentinel share one ordering.
        deps.events = events_log;

        let (outcome, deps) = run_with(&session, "hello", deps).await;
        ok(&outcome);
        let evs = events(&deps);

        let sentinel_at = evs
            .iter()
            .position(|e| matches!(e, Event::SteeringDelivered { text } if text == sentinel))
            .expect("the sentinel was recorded inside complete");
        let update_positions: Vec<usize> = evs
            .iter()
            .enumerate()
            .filter_map(|(i, e)| matches!(e, Event::MessageUpdate { .. }).then_some(i))
            .collect();
        assert_eq!(update_positions.len(), 2);
        assert!(
            update_positions.iter().all(|&i| i < sentinel_at),
            "every MessageUpdate must precede the sentinel - updates are emitted \
             DURING complete, not after it returns (updates at {update_positions:?}, \
             sentinel at {sentinel_at})"
        );
    }

    // ---- context pressure -------------------------------------------------

    #[tokio::test]
    async fn emits_live_numbers_after_every_pass_once_usage_noted() {
        let root = root();
        write(&root, "marker.txt", "");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "list the files", deps).await;
        ok(&outcome);

        let evs = events(&deps);
        let pressures: Vec<(u64, u64, u64)> = evs
            .iter()
            .filter_map(|e| match e {
                Event::ContextPressure {
                    token_estimate,
                    context_budget,
                    max_tokens_reserve,
                } => Some((*token_estimate, *context_budget, *max_tokens_reserve)),
                _ => None,
            })
            .collect();
        assert_eq!(pressures.len(), 2);
        assert_eq!(pressures[0].1, session.context_budget_for(&session.model));
        assert_eq!(pressures[0].2, session.model.max_tokens);
        // Pressure grows Pass to Pass.
        assert!(pressures[1].0 >= pressures[0].0);
    }

    #[tokio::test]
    async fn context_pressure_never_enters_the_conversation() {
        let root = root();
        write(&root, "marker.txt", "");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, _deps) = run_with(&session, "list the files", deps).await;
        let (conv, _) = ok(&outcome);
        assert!(!conv.messages.iter().any(|m| m.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text.contains("context_pressure"))
        )));
    }

    // ---- steering ---------------------------------------------------------

    #[tokio::test]
    async fn drained_steering_rides_tool_results_message_and_is_announced() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        )
        .with_steering(vec![vec!["also check the README".to_string()], vec![]]);

        let (outcome, deps) = run_with(&session, "look around", deps).await;
        ok(&outcome);

        let evs = events(&deps);
        assert!(evs.iter().any(
            |e| matches!(e, Event::SteeringDelivered { text } if text == "also check the README")
        ));

        let requests = deps.requests.lock().unwrap();
        let last = requests[1].messages.last().unwrap();
        assert!(matches!(&last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")
        );
        assert!(
            matches!(&last.content[1], ContentBlock::Text { text } if text == "also check the README")
        );
    }

    // ---- after-Pass hook --------------------------------------------------

    #[tokio::test]
    async fn after_pass_stop_closes_the_run_with_the_stopped_marker() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "t1",
                "list_files",
                json!({"path": "."}),
            ))],
        )
        .with_after_pass(|_r, _c| AfterPass::Stop("budget_hook".to_string()));

        let (outcome, _deps) = run_with(&session, "look", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Custom("budget_hook".to_string()));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn stopped - reply to continue]")
        );
    }

    #[tokio::test]
    async fn after_pass_inject_appends_a_user_message_and_loops() {
        let root = root();
        let session = session(root.path());
        let injected = Arc::new(Mutex::new(vec![
            AfterPass::Continue,
            AfterPass::Inject("remember the budget".to_string()),
        ]));
        let inj = Arc::clone(&injected);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        )
        .with_after_pass(move |_r, _c| inj.lock().unwrap().pop().unwrap());

        let (outcome, deps) = run_with(&session, "look", deps).await;
        ok(&outcome);
        let requests = deps.requests.lock().unwrap();
        let last = requests[1].messages.last().unwrap();
        assert!(matches!(&last.role, Role::User));
        assert!(matches!(&last.content[0], ContentBlock::ToolResult { .. }));
        assert!(
            matches!(&last.content[1], ContentBlock::Text { text } if text == "remember the budget")
        );
    }

    // ---- error algebra ----------------------------------------------------

    #[tokio::test]
    async fn errored_response_settles_failed_keeping_partial_text() {
        let root = root();
        let session = session(root.path());
        let errored = Response {
            content: vec![
                ContentBlock::text("partial thought"),
                ContentBlock::tool_use("t1", "grep", json!({"pattern": "x"})),
            ],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error: Some("request_failed: closed".to_string()),
        };
        let deps = deps_for(&session, vec![just(errored)]);

        let (outcome, deps) = run_with(&session, "go", deps).await;
        let conv = match &outcome {
            Outcome::Failed(reason, conv) => {
                assert_eq!(reason, "request_failed: closed");
                conv
            }
            other => panic!("expected Failed, got {other:?}"),
        };
        // Grammar stays well-formed on the error path.
        let evs = events(&deps);
        assert!(evs.iter().any(|e| matches!(e, Event::MessageEnd { stop_reason, .. } if stop_reason == &StopReason::Error)));
        // Partial text survives; tool_use dropped; failed marker closes.
        let lm = last_message(conv);
        assert_eq!(lm.content.len(), 2);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "partial thought"));
        assert!(matches!(&lm.content[1], ContentBlock::Text { text } if text == "[turn failed]"));
    }

    // ---- malformed-tool-call re-draw (ADR-0030) ---------------------------

    // The server's constrained-decoding miss, as `llm/stream.rs` wraps it.
    fn malformed_error() -> Response {
        Response {
            content: vec![],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error: Some("api_stream_error: Failed to generate a valid tool call".to_string()),
        }
    }

    #[tokio::test]
    async fn a_retryable_error_re_draws_in_band_and_the_run_completes() {
        let root = root();
        let session = session(root.path());
        // A retryable draw fails, then the re-draw succeeds - the Run
        // continues and completes rather than failing.
        let deps = deps_for(
            &session,
            vec![just(malformed_error()), just(text_end("the good answer"))],
        );

        let (outcome, deps) = run_with(&session, "go", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());

        // The Conversation ends on the re-drawn reply; the failed draw left
        // nothing behind (no [run failed] marker).
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "the good answer"));
        assert!(!conv.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "[turn failed]"))
        }));

        let evs = events(&deps);
        // A retry event was produced (visible + durable), naming attempt 1/3.
        assert!(evs.iter().any(|e| matches!(
            e,
            Event::Retry { attempt: 1, budget: 3, error }
            if error.contains("Failed to generate a valid tool call")
        )));

        // The re-draw did NOT advance the Pass: both the failed draw and the
        // successful re-draw carry MessageStart { pass: 1 } - no extra Pass.
        let starts: Vec<u32> = evs
            .iter()
            .filter_map(|e| match e {
                Event::MessageStart { pass } => Some(*pass),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![1, 1]);
        // Two model calls: the failed draw and its re-draw.
        assert_eq!(deps.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_exhausted_budget_falls_to_finish_fail_as_before() {
        let root = root();
        // Budget 1: the first draw fails and re-draws once; a second failure
        // has no budget left, so it fails loud exactly as today.
        let session = session_with(
            root.path(),
            SessionOpts {
                malformed_retry_budget: Some(1),
                ..Default::default()
            },
        );
        let deps = deps_for(
            &session,
            vec![just(malformed_error()), just(malformed_error())],
        );

        let (outcome, deps) = run_with(&session, "go", deps).await;
        match &outcome {
            Outcome::Failed(reason, _) => {
                assert!(reason.contains("Failed to generate a valid tool call"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let evs = events(&deps);
        // Exactly one re-draw was spent before the budget ran out.
        assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 1);
        assert_eq!(deps.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_zero_budget_disables_the_re_draw_entirely() {
        let root = root();
        let session = session_with(
            root.path(),
            SessionOpts {
                malformed_retry_budget: Some(0),
                ..Default::default()
            },
        );
        let deps = deps_for(&session, vec![just(malformed_error())]);

        let (outcome, deps) = run_with(&session, "go", deps).await;
        assert!(matches!(&outcome, Outcome::Failed(_, _)));
        let evs = events(&deps);
        assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 0);
        assert_eq!(deps.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_non_retryable_error_fails_immediately_without_re_drawing() {
        let root = root();
        let session = session(root.path());
        // Context-exceeded is fail-loud by default: no re-draw, even with
        // budget to spare.
        let context_exceeded = Response {
            content: vec![],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error: Some("api_stream_error: Context size has been exceeded".to_string()),
        };
        let deps = deps_for(&session, vec![just(context_exceeded)]);

        let (outcome, deps) = run_with(&session, "go", deps).await;
        match &outcome {
            Outcome::Failed(reason, _) => {
                assert!(reason.contains("Context size has been exceeded"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let evs = events(&deps);
        assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 0);
        // Failed on the first draw: no re-request.
        assert_eq!(deps.requests.lock().unwrap().len(), 1);
    }

    // ---- loop guards ------------------------------------------------------

    #[tokio::test]
    async fn tool_use_stop_with_zero_blocks_ends_as_end_turn() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![just(Response {
                content: vec![ContentBlock::text("hmm")],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                error: None,
            })],
        );
        let (outcome, _deps) = run_with(&session, "hi", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "hmm"));
    }

    #[tokio::test]
    async fn truncated_batch_answers_every_call_executes_nothing() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(Response {
                    content: vec![
                        ContentBlock::text("partial answer"),
                        ContentBlock::tool_use(
                            "t1",
                            "write_file",
                            json!({"path": "a.txt", "content": "trunca"}),
                        ),
                    ],
                    stop_reason: StopReason::MaxTokens,
                    usage: Usage::default(),
                    error: None,
                }),
                just(text_end("re-issued and done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "go", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let tr = find_tool_result(&evs, "t1").unwrap();
        assert!(
            matches!(tr, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("re-issue"))
        );
        // Nothing touched disk.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        // The batch went back intact.
        let requests = deps.requests.lock().unwrap();
        let msgs = &requests[1].messages;
        let tail = &msgs[msgs.len() - 2..];
        assert!(matches!(&tail[0].role, Role::Assistant));
        assert!(matches!(&tail[0].content[0], ContentBlock::Text { .. }));
        assert!(matches!(&tail[0].content[1], ContentBlock::ToolUse { id, .. } if id == "t1"));
        assert!(
            matches!(&tail[1].content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && *is_error)
        );
    }

    #[tokio::test]
    async fn reissued_call_after_truncation_executes_not_duplicate() {
        let root = root();
        let session = session(root.path());
        let input = json!({"path": "a.txt", "content": "hello"});
        let deps = deps_for(
            &session,
            vec![
                just(Response {
                    content: vec![ContentBlock::tool_use("t1", "write_file", input.clone())],
                    stop_reason: StopReason::MaxTokens,
                    usage: Usage::default(),
                    error: None,
                }),
                just(tool_use_result("t2", "write_file", input.clone())),
                just(text_end("done")),
                just(text_end("declining to verify")),
            ],
        );
        let (outcome, deps) = run_with(&session, "write it", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "t1").unwrap(), Event::ToolResult { is_error, .. } if *is_error)
        );
        assert!(
            matches!(find_tool_result(&evs, "t2").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn max_tokens_with_no_tool_use_closes_with_text() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![just(text_result("partial answer", StopReason::MaxTokens))],
        );
        let (outcome, _deps) = run_with(&session, "go", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::MaxTokens));
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "partial answer"));
    }

    #[tokio::test]
    async fn max_tokens_with_no_content_closes_with_truncation_marker() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(&session, vec![just(empty(StopReason::MaxTokens))]);
        let (outcome, _deps) = run_with(&session, "go", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::MaxTokens));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "[response truncated by max_tokens]")
        );
    }

    #[tokio::test]
    async fn run_limit_stops_the_loop_after_n_passes() {
        let root = root();
        let session = session_with_limit(root.path(), 2);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "lib"}))),
            ],
        );
        let (outcome, _deps) = run_with(&session, "explore", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::RunLimit));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn limit reached - reply to continue]")
        );
        let penult = &conv.messages[conv.messages.len() - 2];
        assert!(matches!(&penult.role, Role::User));
        assert!(
            matches!(&penult.content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2")
        );
    }

    // The turn counter bounds the Run: after `max_turns` tool-answering Passes
    // the loop closes on the run-limit marker at the top of the next iteration,
    // without ever building another request. (Group F wires the loop-detector;
    // this is the plain bound.)
    #[tokio::test]
    async fn turn_counter_bounds_the_run_at_max_turns() {
        let root = root();
        let session = session_with_limit(root.path(), 3);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(tool_use_result("t3", "list_files", json!({"path": "."}))),
                // A fourth model reply must never be requested.
                just(text_end("should never be reached")),
            ],
        );
        let (outcome, deps) = run_with(&session, "list forever", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::RunLimit));
        // Exactly three requests: one per answered Pass, none for the bound.
        assert_eq!(deps.requests.lock().unwrap().len(), 3);
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::run_limit_marker())
        );
    }

    // ---- loop-detector (the passive circuit breaker) ----------------------

    // The model stuck on the IDENTICAL Tool Call batch trips the loop-detector:
    // after `loop_stall_limit` consecutive identical batches the Run terminates
    // on the loop-stall marker with the `turn_limit_stuck` reason - and, the
    // whole point of the passive design, NO steering text was injected into the
    // Conversation. Only the close marker rides.
    #[tokio::test]
    async fn a_stuck_identical_batch_trips_the_loop_detector_without_injecting_text() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.loop_stall_limit = Some(3);
        // A run limit generous enough that the detector, not the bound, fires.
        opts.run_limit = Some(50);
        let session = session_with(root.path(), opts);

        // The same call, Pass after Pass: three identical batches trip the cap.
        let same = || just(tool_use_result("t1", "list_files", json!({"path": "."})));
        let deps = deps_for(
            &session,
            vec![
                same(),
                same(),
                same(),
                // A fourth reply must never be requested - the detector closes
                // at the third identical batch.
                just(text_end("should never be reached")),
            ],
        );
        let (outcome, deps) = run_with(&session, "loop forever", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::RunLimitStuck));

        // Exactly three model calls: the detector closed on the third identical
        // batch before a fourth request could be built.
        assert_eq!(deps.requests.lock().unwrap().len(), 3);

        // The Run closes on the loop-stall marker.
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::loop_stall_marker())
        );

        // The passive invariant: NO loop-detector steering text entered the
        // Conversation. Every unstamped (Voice-authored, no Provenance)
        // assistant text block must be exactly the close marker - the detector
        // appends that one marker and nothing else, in contrast to the deleted
        // duplicate/failure nudges which fed corrective text back to the model.
        let voice_texts: Vec<&str> = conv
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant && m.provenance.is_none())
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            voice_texts,
            vec![voice::loop_stall_marker()],
            "the detector appends only the close marker, no steering text"
        );

        // The operator DID get an event: the detector is silent to the model,
        // never to the operator.
        let evs = events(&deps);
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::LoopStall { count } if *count == 3))
        );
    }

    // Different Tool Calls each Pass reset the detector: a model making genuine
    // progress never trips it, even past the stall limit in raw Pass count.
    #[tokio::test]
    async fn distinct_batches_each_pass_never_trip_the_detector() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.loop_stall_limit = Some(2);
        opts.run_limit = Some(50);
        let session = session_with(root.path(), opts);

        // Four DIFFERENT calls in a row, then a clean finish: each batch differs
        // from the last, so the identical-count resets to 1 every Pass and the
        // cap of 2 is never reached.
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "src"}))),
                just(tool_use_result("t3", "list_files", json!({"path": "lib"}))),
                just(tool_use_result("t4", "list_files", json!({"path": "docs"}))),
                just(text_end("done exploring")),
            ],
        );
        let (outcome, deps) = run_with(&session, "explore around", deps).await;
        let (conv, stop) = ok(&outcome);
        // A clean end_turn - the detector never fired.
        assert_eq!(*stop, OutcomeStop::end_turn());
        let evs = events(&deps);
        assert!(!evs.iter().any(|e| matches!(e, Event::LoopStall { .. })));
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "done exploring"));
    }

    // Every request offers the FULL Tool registry - there is no per-Pass
    // narrowing (the Endgame narrowing was torn out with the Governors).
    #[tokio::test]
    async fn every_request_offers_the_full_tool_registry() {
        let root = root();
        let session = session_with_limit(root.path(), 3);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "list twice", deps).await;
        ok(&outcome);
        let full: Vec<String> = crate::tools::specs().into_iter().map(|s| s.name).collect();
        let requests = deps.requests.lock().unwrap();
        // Three requests, each carrying the identical full registry - no
        // narrowing on any Pass, near the limit or not.
        assert_eq!(requests.len(), 3);
        for req in requests.iter() {
            let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
            assert_eq!(names, full);
        }
    }

    // ---- next-speaker check (ADR-0043) ------------------------------------

    // A thinking-only reply (empty final content) auto-continues WITHOUT a
    // side-query: the short-circuit injects "Please continue." and loops, and
    // the next Pass finishes the Run. This is the #1 pain the check fixes.
    #[tokio::test]
    async fn a_thinking_only_reply_auto_continues_via_the_short_circuit() {
        let root = root();
        let session = session_next_speaker(root.path(), 50);
        // First reply: only a thinking block (dropped from final content -> the
        // Pass looks empty). Second reply (after "Please continue."): a real
        // answer, then its next-speaker verdict ends the Run.
        let thinking_only = Response {
            content: vec![ContentBlock::Thinking {
                text: "let me reason".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![
                just(thinking_only),
                just(text_end("here is the answer")),
                just(next_speaker_verdict("user")),
            ],
        );
        let (outcome, deps) = run_with(&session, "think then answer", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());

        // A "Please continue." user message was injected between the two Passes,
        // and announced as a delivered-steering event.
        assert!(conv.messages.iter().any(|m| m.role == Role::User
            && matches!(&m.content[0], ContentBlock::Text { text } if text == voice::please_continue())));
        let evs = events(&deps);
        assert!(evs.iter().any(
            |e| matches!(e, Event::SteeringDelivered { text } if text == voice::please_continue())
        ));

        // The Run ends on the real answer.
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "here is the answer")
        );
    }

    // A textful reply whose side-query returns {"next_speaker":"model"}
    // auto-continues: the reply enters the Conversation (stamped), then
    // "Please continue." nudges the model on.
    #[tokio::test]
    async fn a_model_verdict_continues_and_appends_the_reply_then_the_nudge() {
        let root = root();
        let session = session_next_speaker(root.path(), 50);
        let deps = deps_for(
            &session,
            vec![
                just(text_end("Next, I will read the config.")),
                just(next_speaker_verdict("model")),
                just(text_end("Done reading it.")),
                just(next_speaker_verdict("user")),
            ],
        );
        let (outcome, _deps) = run_with(&session, "go", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());

        // The announced-but-not-executed reply enters stamped with the Model's
        // Provenance; the nudge follows as an unstamped user message.
        let announce = conv
            .messages
            .iter()
            .position(|m| m.role == Role::Assistant
                && matches!(&m.content[0], ContentBlock::Text { text } if text == "Next, I will read the config."))
            .expect("the first reply is in the Conversation");
        assert_eq!(
            conv.messages[announce].provenance,
            Some(session.model.provenance())
        );
        let nudge = &conv.messages[announce + 1];
        assert_eq!(nudge.role, Role::User);
        assert!(
            matches!(&nudge.content[0], ContentBlock::Text { text } if text == voice::please_continue())
        );
        assert_eq!(
            nudge.provenance, None,
            "the nudge is Voice-authored, not the model's"
        );

        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "Done reading it.")
        );
    }

    // A {"next_speaker":"user"} verdict ends the Run exactly as before: the
    // reply is the closing message, no nudge injected.
    #[tokio::test]
    async fn a_user_verdict_finishes_the_run_with_no_nudge() {
        let root = root();
        let session = session_next_speaker(root.path(), 50);
        let deps = deps_for(
            &session,
            vec![
                just(text_end("All set. Let me know if you need anything.")),
                just(next_speaker_verdict("user")),
            ],
        );
        let (outcome, deps) = run_with(&session, "finish up", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());

        // No "Please continue." was injected.
        assert!(!conv.messages.iter().any(|m| m.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text == voice::please_continue())
        )));
        let evs = events(&deps);
        assert!(!evs.iter().any(
            |e| matches!(e, Event::SteeringDelivered { text } if text == voice::please_continue())
        ));

        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "All set. Let me know if you need anything.")
        );
    }

    // The auto-continuation is BOUNDED by max_turns: a model that keeps
    // producing empty replies (short-circuit -> always continue) cannot loop
    // forever - the run-limit guard closes the Run.
    #[tokio::test]
    async fn the_continuation_is_bounded_by_max_turns() {
        let root = root();
        // run_limit 3: at most three no-tool Passes, then the bound closes it.
        let session = session_next_speaker(root.path(), 3);
        let always_empty = || {
            just(Response {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                error: None,
            })
        };
        let deps = deps_for(
            &session,
            vec![
                always_empty(),
                always_empty(),
                always_empty(),
                // A fourth reply must never be requested - the bound closes
                // after the third empty Pass.
                just(text_end("should never be reached")),
            ],
        );
        let (outcome, deps) = run_with(&session, "loop on empties", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::RunLimit));

        // Exactly three model calls (no side-query fires on the empty
        // short-circuit): the fourth was never requested.
        assert_eq!(deps.requests.lock().unwrap().len(), 3);
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::run_limit_marker())
        );
    }

    // `skip_next_speaker` restores the pre-check behavior: a no-tool reply
    // finishes the Run immediately, with no side-query.
    #[tokio::test]
    async fn skip_next_speaker_finishes_without_the_check() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.skip_next_speaker = Some(true);
        let session = session_with(root.path(), opts);
        let deps = deps_for(&session, vec![just(text_end("done"))]);
        let (outcome, deps) = run_with(&session, "go", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());
        // No side-query: exactly one model call.
        assert_eq!(deps.requests.lock().unwrap().len(), 1);
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "done"));
    }

    // A tool-call reply continues on tool PRESENCE even when the stop reason is
    // NOT tool_use (qwen-code parity, the core inversion): an EndTurn stop that
    // still carries a tool_use block executes it rather than ending the Run.
    #[tokio::test]
    async fn tool_use_with_a_non_tool_use_stop_reason_still_continues() {
        let root = root();
        write(&root, "marker.txt", "");
        let session = session(root.path());
        // stop_reason EndTurn, but a tool_use block is present -> must execute.
        let end_turn_with_tool = Response {
            content: vec![ContentBlock::tool_use(
                "t1",
                "list_files",
                json!({"path": "."}),
            )],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![just(end_turn_with_tool), just(text_end("done"))],
        );
        let (outcome, deps) = run_with(&session, "list", deps).await;
        ok(&outcome);
        // The tool ran despite the EndTurn stop reason.
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "t1"), Some(Event::ToolResult { is_error, .. }) if !is_error)
        );
    }

    // ---- approval gate (ADR-0005) -----------------------------------------

    #[tokio::test]
    async fn a_denied_run_command_answers_the_denial_and_never_runs() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(text_end("moved on")),
            ],
        )
        .with_approvals(vec![false]);
        let (outcome, deps) = run_with(&session, "run it", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        // The Approval gate asked, then the denial became the is_error result.
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::ApprovalRequest { .. }))
        );
        assert!(
            find_tool_result(&evs, "r1")
                .map(|e| matches!(e, Event::ToolResult { is_error, content, .. }
                    if *is_error && content == voice::command_denied()))
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn context_budget_exhaustion_fails_before_any_request() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.context_budget = Some(60);
        opts.compaction_slack = Some(0.0);
        opts.model = Some(Model::new("local", "m", Api::AnthropicMessages, 64_000, 50));
        let session = session_with(root.path(), opts);
        // No script entries: any complete call would surface a different error.
        let deps = deps_for(&session, vec![]);
        let prompt = "pad ".repeat(50);
        let (outcome, _deps) = run_with(&session, &prompt, deps).await;
        assert_eq!(outcome, Outcome::Error);
    }

    // ---- malformed tool input ---------------------------------------------

    #[tokio::test]
    async fn malformed_input_becomes_error_result_never_executes() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(Response {
                    content: vec![ContentBlock::tool_use(
                        "t1",
                        "write_file",
                        malformed_input_marker("{\"path\": \"oops"),
                    )],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                    error: None,
                }),
                just(text_end("ok")),
            ],
        );
        let (outcome, deps) = run_with(&session, "write something", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let tr = evs
            .iter()
            .find(|e| matches!(e, Event::ToolResult { name, .. } if name == "write_file"))
            .unwrap();
        assert!(
            matches!(tr, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("not valid JSON"))
        );
        // The error tool_result went back to the model.
        let requests = deps.requests.lock().unwrap();
        let last = requests[1].messages.last().unwrap();
        assert!(
            matches!(&last.content[0], ContentBlock::ToolResult { is_error, content, .. } if *is_error && content.contains("not valid JSON"))
        );
        // Nothing executed.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    // ---- extension lifecycle (ADR-0007) -----------------------------------

    struct HaltEdits;
    impl Middleware for HaltEdits {
        fn pre_run(&self, token: Token, _opts: &Value) -> Token {
            if token.tool == "edit_file" {
                token.halt("[edits are frozen by HaltEdits]")
            } else {
                token
            }
        }
    }

    struct Artifactor;
    impl Middleware for Artifactor {
        fn post_run(&self, token: Token, _opts: &Value) -> Token {
            let tool = token.tool.clone();
            token.put_artifact("mark", Value::String(tool))
        }
    }

    struct PreBoomer;
    impl Middleware for PreBoomer {
        fn pre_run(&self, _token: Token, _opts: &Value) -> Token {
            panic!("pre boom")
        }
    }

    async fn run_with_extensions(
        session: &Session,
        prompt: &str,
        mut deps: FakeDeps,
        extensions: Vec<Registered>,
    ) -> (Outcome, FakeDeps) {
        let conv = conversation(session, prompt);
        let ctx = tool_ctx(session);
        let outcome = run(
            conv,
            session,
            RunEnv {
                extensions: &extensions,
                tool_ctx: &ctx,
            },
            &mut deps,
            RunOpts::default(),
        )
        .await;
        (outcome, deps)
    }

    #[tokio::test]
    async fn halting_extension_denies_the_call_with_its_own_wording() {
        let root = root();
        let session = session(root.path());
        let input = json!({"path": "f.txt", "old_str": "a", "new_str": "b"});
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "edit_file", input)),
                just(text_end("ok")),
            ],
        );
        let extensions =
            vec![Registered::new("HaltEdits", json!([])).with_middleware(Box::new(HaltEdits))];
        let (outcome, deps) =
            run_with_extensions(&session, "edit something", deps, extensions).await;
        ok(&outcome);
        let evs = events(&deps);
        let tr = evs
            .iter()
            .find(|e| matches!(e, Event::ToolResult { .. }))
            .unwrap();
        match tr {
            Event::ToolResult {
                is_error,
                content,
                artifacts,
                ..
            } => {
                assert!(is_error);
                assert_eq!(content, "[edits are frozen by HaltEdits]");
                assert!(artifacts.is_empty());
            }
            _ => unreachable!(),
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn artifacts_ride_the_tool_result_event() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("ok")),
            ],
        );
        let extensions =
            vec![Registered::new("Artifactor", json!([])).with_middleware(Box::new(Artifactor))];
        let (outcome, deps) = run_with_extensions(&session, "look around", deps, extensions).await;
        ok(&outcome);
        let evs = events(&deps);
        let tr = evs
            .iter()
            .find(|e| matches!(e, Event::ToolResult { .. }))
            .unwrap();
        match tr {
            Event::ToolResult {
                is_error,
                artifacts,
                ..
            } => {
                assert!(!is_error);
                assert_eq!(
                    artifacts.get("mark"),
                    Some(&Value::String("list_files".to_string()))
                );
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn crashing_extension_is_fail_open() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("ok")),
            ],
        );
        let extensions =
            vec![Registered::new("PreBoomer", json!([])).with_middleware(Box::new(PreBoomer))];
        let (outcome, deps) = run_with_extensions(&session, "look around", deps, extensions).await;
        ok(&outcome);
        let evs = events(&deps);
        let pe = evs
            .iter()
            .find(|e| matches!(e, Event::ExtensionError { .. }))
            .unwrap();
        assert!(
            matches!(pe, Event::ExtensionError { extension, stage, message }
            if extension == "PreBoomer" && *stage == Stage::PreRun && message.contains("pre boom"))
        );
        let tr = evs
            .iter()
            .find(|e| matches!(e, Event::ToolResult { .. }))
            .unwrap();
        assert!(matches!(tr, Event::ToolResult { is_error, .. } if !is_error));
    }

    // ---- Plan storage -----------------------------------------------------

    #[tokio::test]
    async fn successful_plan_call_stores_plan_via_set_plan() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "p1",
                    "todo_write",
                    json!({"todos": [
                        { "content": "read", "status": "in_progress" },
                        { "content": "edit", "status": "pending" },
                    ]}),
                )),
                just(text_end("planned, done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do X", deps).await;
        ok(&outcome);
        let plans = deps.plans.lock().unwrap();
        assert_eq!(plans.as_slice(), &["[~] read\n[ ] edit".to_string()]);
    }

    #[tokio::test]
    async fn failed_plan_call_does_not_store_a_plan() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("p1", "todo_write", json!({}))),
                just(text_end("recovered")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do X", deps).await;
        ok(&outcome);
        assert!(deps.plans.lock().unwrap().is_empty());
    }

    // ---- proactive Compaction (ADR-0012) ----------------------------------

    #[tokio::test]
    async fn proactive_compacts_before_first_pass() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.run_limit = Some(50);
        let session = session_with(root.path(), opts);

        let compacted = Arc::new(Mutex::new(false));
        let c = Arc::clone(&compacted);
        let mut deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        )
        .with_compact(move |conv: Conversation| {
            *c.lock().unwrap() = true;
            let mut out = conv.clone();
            out.messages = vec![conv.messages[0].clone()];
            Ok(out)
        });

        // Geometry: budget 4000, reserve 100, slack 0.3. Target = 2700; the big
        // assistant blob puts the estimate over target but under the cliff.
        let mut conv = Conversation::new(
            "sys",
            crate::conversation::ConversationOpts::new(4000, 100).compaction_slack(0.3),
        );
        conv.add_user_text("original task");
        conv.add_assistant_blocks(vec![ContentBlock::text("x".repeat(12_000))]);

        let extensions: Vec<Registered> = Vec::new();
        let ctx = tool_ctx(&session);
        let outcome = run(
            conv,
            &session,
            RunEnv {
                extensions: &extensions,
                tool_ctx: &ctx,
            },
            &mut deps,
            RunOpts::default(),
        )
        .await;
        let (_conv, _) = ok(&outcome);
        assert!(*compacted.lock().unwrap());
        // Compaction ran before the first model call: the first recorded request
        // reflects the compacted (single-message) conversation.
        let requests = deps.requests.lock().unwrap();
        assert_eq!(requests[0].messages.len(), 1);
    }

    #[tokio::test]
    async fn leaves_conversation_alone_under_the_target() {
        let root = root();
        let session = session(root.path());
        let compacted = Arc::new(Mutex::new(false));
        let c = Arc::clone(&compacted);
        let deps = deps_for(&session, vec![just(text_end("done"))]).with_compact(
            move |_conv: Conversation| {
                *c.lock().unwrap() = true;
                Err(CompactError("should_not_run".to_string()))
            },
        );
        let (outcome, _deps) = run_with(&session, "small task", deps).await;
        ok(&outcome);
        assert!(!*compacted.lock().unwrap());
    }
}
