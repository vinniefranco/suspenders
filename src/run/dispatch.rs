//! Stop-reason dispatch and the loop-detector - the decision half of a Pass,
//! carved from the Run Loop ([`super::loop_`]) so the loop skeleton keeps only
//! the Pass cycle and this module owns "what happens after the model responds".
//!
//! [`dispatch`] keys on Tool Call PRESENCE (not the stop reason - qwen-code
//! parity's core inversion) and routes each Pass to one of: the error re-draw,
//! the truncated-batch re-issue, the tool-answering continuation (guarded by the
//! passive loop-detector), or the no-tool-call finish (via the next-speaker
//! check). The loop skeleton reads only the returned [`Flow`]; executing a
//! batch lives in [`super::batch`], and how a Run ends lives in [`super::finish`].

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::response::{Response, StopReason};
use crate::run::deps::{AfterPass, RunDeps};
use crate::run::loop_::LoopState;
use crate::run::next_speaker::{self, NextSpeaker};
use crate::run::settlement::Outcome;
use crate::run::{batch, finish};
use crate::voice;

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
pub(super) async fn dispatch<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    if response.stop_reason == StopReason::Error {
        return error_flow(state, conversation, response).await;
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
// behavior). Either way, the WOULD-FINISH branch first consults the Stop hook
// (Phase 3b, ADR-0066): a Stop hook that blocks forces one more continuation.
async fn no_tool_call<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    let content = response.content.clone();

    if state.skip_next_speaker {
        return finish_or_stop_hook(state, conversation, response, content).await;
    }

    match next_speaker::check_next_speaker(state.deps, &response).await {
        NextSpeaker::User => finish_or_stop_hook(state, conversation, response, content).await,
        NextSpeaker::Model => continue_after_no_tool(state, conversation, response),
    }
}

// The Stop seam (Phase 3b, ADR-0066): the model would END the Run here (no more
// tool calls, the next-speaker check said User or was skipped). Fire the Stop
// hooks first - a Stop hook that blocks (`continue:false`) INVERTS the stop: the
// Run must continue, and the hook's "Stop hook feedback:\n<reason>" is injected as
// a user turn to guide the next Pass. A2 (qwen's configurable cap): the Run tracks
// a continuation COUNTER against a resolved cap (default 8, env-overridable). Stop
// fires while `stop_hook_count < stop_hook_cap`, incrementing on each forced
// continuation; when the count reaches the cap the Run STOPS despite a still-
// blocking hook and qwen's cap warning is emitted. So a hook that always blocks
// forces at most `cap` extra Passes, never an infinite loop, and a legitimate stop
// (no forcing hook) is never wrongly blocked.
async fn finish_or_stop_hook<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
    content: Vec<ContentBlock>,
) -> Flow {
    // The cap already reached, or no hooks are wired: finish as normal.
    let Some(hooks) = state
        .hooks
        .filter(|_| state.stop_hook_count < state.stop_hook_cap)
    else {
        return finish::finish(state, conversation, content, response.stop_reason);
    };

    match hooks.stop().await {
        crate::run::hooks::StopDecision::Stop => {
            finish::finish(state, conversation, content, response.stop_reason)
        }
        crate::run::hooks::StopDecision::Continue { feedback } => {
            // The forced continuation: increment the counter (A2). If it has now
            // reached the cap, emit qwen's cap warning and END the Run rather than
            // continue - the hook is overridden so it cannot loop forever.
            state.stop_hook_count += 1;
            if state.stop_hook_count >= state.stop_hook_cap {
                let warning = crate::run::hooks::format_stop_hook_cap_warning(state.stop_hook_cap);
                state
                    .emitter
                    .emit(Event::fail_open_report("hook Stop".to_string(), warning));
                return finish::finish(state, conversation, content, response.stop_reason);
            }
            state.emitter.emit(Event::fail_open_report(
                "hook Stop".to_string(),
                "forced the Run to continue".to_string(),
            ));
            let blocks: Vec<ContentBlock> = content
                .iter()
                .filter(|b| !b.is_tool_use() && !matches!(b, ContentBlock::Thinking { .. }))
                .cloned()
                .collect();
            if !blocks.is_empty() {
                conversation.add_assistant_response(blocks, state.deps.provenance());
            }
            conversation.add_user_text(feedback.clone());
            state.emitter.emit(Event::steering_delivered(feedback));
            state.deps.checkpoint(&conversation);
            state.turn += 1;
            Flow::Continue(conversation)
        }
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
async fn error_flow<D: RunDeps>(
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
        // The StopFailure seam (Phase 3b, ADR-0066): the turn ended on an API error
        // (non-retryable, or the re-draw budget is spent). Fire the observational
        // StopFailure hooks with the error before the loud `fail` closes the Run.
        if let Some(hooks) = state.hooks {
            hooks
                .stop_failure(&response.error.clone().unwrap_or_default())
                .await;
        }
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
            voice::Marker::LoopStall.text(),
            crate::stop_reason::StopReason::RunLimitStuck,
        ));
    }
    let (results, conversation) =
        batch::execute_tools(state, conversation, &response.content).await;

    // A hook's `continue:false` (Phase 3a, ADR-0066): a Pre/PostToolUse hook that
    // ran during this batch requested the loop stop. Answer the batch first (the
    // results are appended so the Conversation never persists an unanswered
    // tool_use block), then close the Run on the hook's reason through the same
    // custom-stop path an after-Pass `Stop` takes. Taken (not peeked) so a later
    // Pass does not re-close.
    if let Some(reason) = state.hook_stop.take() {
        let (mut conversation, response, results) = (conversation, response, results);
        conversation.add_assistant_response(response.content.clone(), state.deps.provenance());
        conversation.add_tool_results(results, Vec::new());
        state.deps.checkpoint(&conversation);
        return Flow::Done(finish::close_custom(
            state,
            conversation,
            voice::Marker::RunStopped.text(),
            reason,
        ));
    }

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
// whole point of the passive design, ADR-0045). With `identical_cap == 0` the
// detector is inert (a fresh count of 1 never reaches 0).
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

            let content = voice::Marker::TruncatedCallReissue.text().to_string();
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
            voice::Marker::RunStopped.text(),
            reason,
        )),
        AfterPass::Inject(text) => {
            conversation.merge_user_text(text);
            state.turn += 1;
            Flow::Continue(conversation)
        }
    }
}
