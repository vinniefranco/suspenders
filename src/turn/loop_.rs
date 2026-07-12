//! Turn Loop — the inner tool-call loop of a Turn (baud's `Baud.Turn.Loop`).
//! (Module name is `loop_` because `loop` is a keyword — ADR-0022.)
//!
//! One Pass (CONTEXT.md) = one model response plus the Tool Calls it carries.
//! Per Pass the loop emits a well-formed message grammar — `MessageStart`,
//! `MessageUpdate` (delta + accumulated snapshot), `MessageEnd` — on every path,
//! including errored responses, then acts on the stop reason. See the Elixir
//! moduledoc (`baud/lib/baud/turn/loop.ex`) for the full narrative; this port
//! preserves its behaviour exactly, with the Turn Ledger and the Governors'
//! trigger state threaded as plain values instead of baud's functional
//! re-binding.
//!
//! This module keeps the loop skeleton: the Pass cycle (request, stream,
//! dispatch), proactive Compaction at Turn start, and the riders on a
//! tool-answering Pass (Steering, Explore Nudge, Anchor, Endgame tail rider,
//! Turn Limit, after-Pass hook). Executing a Pass's Tool Call batch lives in
//! [`super::batch`]; how a Turn ends when the model stops calling tools lives
//! in [`super::finish`]. Every heuristic decision — which Tools ride, what
//! rides the results tail, when the Turn Limit closes — comes from the
//! arbiter in [`super::governor`] (ADR-0026); this module only translates the
//! returned Interventions into effects.
//!
//! The Loop owns zero I/O and zero process concerns: every effect goes through
//! [`TurnDeps`]. Tool execution (the Plugin pipeline) runs in-loop as in baud,
//! over a `plugins` list and a `ToolCtx` the caller supplies — the Rust Session
//! carries plugin *names*, not `Registered` values, so these ride as explicit
//! `run` arguments (the shell builds them from the Session).

use serde_json::Value;

use crate::content::ContentBlock;
use crate::conversation::{self, Conversation};
use crate::event::Event;
use crate::llm::request::LlmRequest;
use crate::llm::response::{Response, StopReason};
use crate::llm::stream::StreamEvent;
use crate::plan::Plan;
use crate::plugins::Registered;
use crate::session::Session;
use crate::session::log;
use crate::tool::ToolCtx;
use crate::turn::deps::{AfterPass, Emitter, TurnDeps};
use crate::turn::governor::ledger::Ledger;
use crate::turn::governor::{
    self, AnswerIntervention, FinishIntervention, Governors, RequestIntervention, Rider,
};
use crate::turn::{batch, finish};
use crate::voice;

/// The Turn loop's outcome (baud's `outcome`).
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The Turn completed; carries the final Conversation and terminal stop
    /// reason.
    Ok(Conversation, OutcomeStop),
    /// The Turn closed at its Turn Limit with demonstrably unfinished work and
    /// the Endgame Governor issued the close-and-open-a-Recovery-Turn
    /// Intervention (CONTEXT.md: Recovery Turn). The Conversation is closed on
    /// the turn-limit marker exactly like an `Ok` limit close; the directive
    /// rides out so the Agent — which owns the Turn lifecycle — executes the
    /// opening.
    Recover(Conversation, log::StopReason, governor::endgame::Recovery),
    /// The response errored; carries the LLM error reason and the Conversation
    /// (with the partial text and the failed marker).
    Failed(String, Conversation),
    /// The Context Budget was exhausted and Compaction could not recover it: no
    /// request was ever sent.
    Error,
}

/// The terminal stop reason of an `Ok` outcome. Spans the enumerable reasons
/// ([`log::StopReason`]: `end_turn`, `max_tokens`, `turn_limit`, ...) and the
/// arbitrary atom an after-Pass `Stop` hook may name (baud's `{:stop, atom()}`).
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

/// Options for [`run`] (baud's `opts`): the restored Plan content, the
/// durable original task copy from the Compaction state, and the Recovery
/// Turns already consumed serving the current user request (an Agent-owned
/// cross-Turn fact the Ledger starts with).
#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    pub plan: Option<String>,
    pub original_task: Option<String>,
    pub recoveries_used: u64,
}

// The loop state that spans Passes: the effect bundle, the owned emission
// handle (obtained once from `deps.emitter()`, ADR-0025), the Plan/Anchor
// state, the Turn Ledger (the Turn's facts, written here and in `batch` at
// the firing sites — ADR-0026), and the Governors' trigger state + resolved
// Setpoints. The Session's fixed facts the loop needs are resolved into the
// Ledger and the Governors once at Turn start, so no Session reference rides.
// The Conversation stays a separate value the loop folds. Fields are
// `pub(super)` so `batch` and `finish` work on the state directly.
pub(super) struct LoopState<'a, D: TurnDeps> {
    pub(super) deps: &'a mut D,
    pub(super) emitter: Emitter,
    pub(super) plugins: &'a [Registered],
    pub(super) tool_ctx: &'a ToolCtx,
    pub(super) ledger: Ledger,
    pub(super) governors: Governors,
    pub(super) plan: Plan,
}

/// Runs the loop until the model stops asking for tools, the Turn Limit is hit,
/// or the response errors (baud's `Baud.Turn.Loop.run/4`).
///
/// `plugins` and `tool_ctx` supply the Plugin pipeline and Tool execution
/// context (Session-derived; the Rust Session carries plugin names only).
pub async fn run<D: TurnDeps>(
    mut conversation: Conversation,
    session: &Session,
    plugins: &[Registered],
    tool_ctx: &ToolCtx,
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
        plugins,
        tool_ctx,
        ledger: Ledger::new(session.turn_limit),
        governors: Governors::new(
            session.anchor_interval,
            session.plan_stale_after,
            session.no_think_rescue,
        )
        .with_recovery(governor::endgame::RecoverySetpoints {
            limit: session.recovery_limit,
            shape: session.recovery_shape,
        }),
        plan,
    };

    // Recovery Turns already consumed serving this user request: an Agent-
    // owned cross-Turn fact the Ledger starts with, read by the Endgame
    // Governor's recovery judgment.
    state.ledger.note_recoveries_used(opts.recoveries_used);

    // A Plan carried in from a previous Turn is a fact the Ledger starts
    // with: its recency clock runs from Turn start (a Plan set THIS Turn
    // starts its clock at `batch`'s firing site instead).
    if state.plan.content.is_some() {
        state.ledger.note_plan_carried();
    }

    conversation = maybe_compact_proactive(&mut state, conversation).await;
    run_loop(&mut state, conversation).await
}

// Proactive Compaction (ADR-0012): when the Conversation already exceeds the
// compaction target at Turn start, compact before the first Pass. A failed
// Compaction falls through to the reactive path at the budget cliff.
async fn maybe_compact_proactive<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Conversation {
    if proactive_compaction_due(&conversation) {
        match state.deps.compact(conversation.clone()).await {
            Ok(compacted) => {
                state.ledger.note_compacted();
                compacted
            }
            Err(_) => conversation,
        }
    } else {
        conversation
    }
}

// baud's `Baud.Compaction.proactive?/1`: the estimate exceeds the Compaction
// Target (the same low-water mark Eviction settles to).
fn proactive_compaction_due(conv: &Conversation) -> bool {
    conv.token_estimate() > conv.compaction_target()
}

async fn run_loop<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
) -> Outcome {
    loop {
        let (request, next_conv) = match build_request(state, conversation).await {
            Ok(pair) => pair,
            Err(()) => return Outcome::Error,
        };
        conversation = next_conv;

        state
            .emitter
            .emit(Event::message_start(state.ledger.pass() as u32));

        let response = complete_and_emit(state, request).await;

        state.emitter.emit(Event::message_end(
            response.content.clone(),
            response.stop_reason.clone(),
        ));

        conversation.note_usage(usage_of(&response));
        emit_context_pressure(state, &conversation);

        match dispatch(state, conversation, response).await {
            Flow::Done(outcome) => return outcome,
            Flow::Continue(next) => conversation = next,
        }
    }
}

// The invariant: streaming deltas are emitted AS THEY STREAM — every
// MessageUpdate goes out between MessageStart and MessageEnd, DURING the
// `complete` call, never buffered until it returns. `complete` exclusively
// borrows `state.deps`, which is exactly why emission is the owned `Emitter`
// beside the deps (ADR-0025): destructuring `state` borrows the disjoint
// `deps` and `emitter` fields, so the sink emits live while the model call
// holds the deps.
async fn complete_and_emit<D: TurnDeps>(
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
// Conversation and set the post-Compaction Anchor flag) or `Err(())` for
// context-budget exhaustion.
async fn build_request<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
) -> Result<(LlmRequest, Conversation), ()> {
    match conversation.for_request() {
        Ok(req) => {
            // The request-shaping moment (ADR-0026): the full registry rides
            // and Thinking stays on unless an Intervention narrows or
            // silences them.
            let mut tools = crate::tools::specs();
            let mut no_think = false;
            let shaped = governor::shape_request(&state.ledger, &mut state.governors);
            for intervention in shaped {
                match intervention {
                    RequestIntervention::NarrowTools(narrowed) => tools = narrowed,
                    RequestIntervention::SilenceThinking => no_think = true,
                }
            }
            let request = LlmRequest::new(req.system, req.messages, tools).with_no_think(no_think);
            Ok((request, conversation))
        }
        Err(_) => {
            // Compaction recovery: try summarizing before giving up.
            match state.deps.compact(conversation.clone()).await {
                Ok(compacted) => {
                    state.ledger.note_compacted();
                    Box::pin(build_request(state, compacted)).await
                }
                Err(_) => Err(()),
            }
        }
    }
}

// The result of a stop-reason dispatch: either the loop continues with an
// updated Conversation, or the Turn is done.
pub(super) enum Flow {
    Continue(Conversation),
    Done(Outcome),
}

async fn dispatch<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    match response.stop_reason {
        StopReason::Error => Flow::Done(finish::fail(state, conversation, response)),
        StopReason::ToolUse => {
            // A :tool_use stop with zero tool_use blocks is a server quirk, not
            // a request for tools; treat it as end_turn (never append an empty
            // Tool Results message and loop forever).
            if response.content.iter().any(ContentBlock::is_tool_use) {
                continue_tools(state, conversation, response).await
            } else {
                let content = response.content.clone();
                finish::finish(state, conversation, content, StopReason::ToolUse)
            }
        }
        StopReason::MaxTokens => {
            if response.content.iter().any(ContentBlock::is_tool_use) {
                truncated_batch(state, conversation, response).await
            } else {
                let content = response.content.clone();
                finish::finish(state, conversation, content, StopReason::MaxTokens)
            }
        }
        other => {
            let content = response.content.clone();
            finish::finish(state, conversation, content, other)
        }
    }
}

// The model asked for tools and gets them, in the order it emitted them.
async fn continue_tools<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    response: Response,
) -> Flow {
    let (results, conversation) =
        batch::execute_tools(state, conversation, &response.content).await;
    next_pass(state, conversation, response, results).await
}

// ADR-0009: a max_tokens stop cut the response mid-batch. The streamed
// arguments may be valid-but-incomplete JSON, so NOTHING executes; every call
// is answered with the re-issue error and the model retries in-band. The calls
// never enter the duplicate memory.
async fn truncated_batch<D: TurnDeps>(
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

            let content = voice::truncated_call_nudge().to_string();
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

    state.governors.next_pass();
    state.ledger.close_batch();
    next_pass(state, conversation, response, results).await
}

// Shared tail of every tool-answering Pass: drain Steering, append the batch
// (assistant blocks intact, results + Steering as ONE user message),
// checkpoint, then Turn Limit -> after-Pass hook -> loop.
async fn next_pass<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
    results: Vec<ContentBlock>,
) -> Flow {
    let steering = state.deps.drain_steering().await;

    conversation.add_assistant_blocks(response.content.clone());
    conversation.add_tool_results(results, steering.clone());

    for text in &steering {
        state.emitter.emit(Event::steering_delivered(text.clone()));
    }

    // The Tool Calls this Pass carried are a Ledger fact, recorded once here
    // (a truncated batch's calls count even though none executed).
    state
        .ledger
        .record_pass_calls(pass_calls(&response.content));

    // Verify-failed / Verify / Empty re-arm on progress (a Pass that made at
    // least one Tool Call). This is a tool-answering Pass, so calls is non-empty.
    state.governors.note_progress(state.ledger.pass_calls());

    // The results tail of the Tool Call answering moment (ADR-0026): the
    // arbiter decides what rides the trailing tool-results user message —
    // Explore Nudge, Anchor, the Endgame's rider — and this site applies it.
    let tail = governor::answer_tail(&state.ledger, &mut state.governors);
    state.ledger.note_tail_delivered();
    for intervention in tail {
        apply_tail(state, &mut conversation, intervention);
    }

    state.deps.checkpoint(&conversation);

    // The finish-settlement moment, consulted after a tool-answering Pass: at
    // the Turn Limit the arbiter closes the Turn on the marker (stop calling
    // the model; the marker keeps roles alternating) — carrying the Endgame
    // Governor's recovery directive out when the work is unfinished.
    match governor::settle_capped(&state.ledger, &state.governors) {
        Some(FinishIntervention::Close(reason)) => {
            return Flow::Done(finish::close(
                state,
                conversation,
                voice::turn_limit_marker(),
                reason,
            ));
        }
        Some(FinishIntervention::CloseRecover { reason, recovery }) => {
            return Flow::Done(finish::close_recover(
                state,
                conversation,
                voice::turn_limit_marker(),
                reason,
                recovery,
            ));
        }
        Some(FinishIntervention::Standalone { .. }) => {
            unreachable!("only a Close issues at the Turn Limit")
        }
        None => {}
    }

    match state.deps.after_pass(&response, &conversation).await {
        AfterPass::Continue => {
            state.ledger.advance_pass();
            Flow::Continue(conversation)
        }
        AfterPass::Stop(reason) => Flow::Done(finish::close_custom(
            state,
            conversation,
            voice::turn_stopped_marker(),
            reason,
        )),
        AfterPass::Inject(text) => {
            conversation.merge_user_text(text);
            state.ledger.advance_pass();
            Flow::Continue(conversation)
        }
    }
}

// Translates one results-tail Intervention into its effect: Voiced text is
// announced with its Transcript event and merged into the trailing
// tool-results user message; the Anchor injects the Plan's current anchor
// block on the same seam. The per-call Interventions never issue from the
// tail consultation (they are translated in `super::batch`).
fn apply_tail<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: &mut Conversation,
    intervention: AnswerIntervention,
) {
    match intervention {
        AnswerIntervention::RideTail(Rider::Voiced { tag, text }) => {
            state.emitter.emit(Event::voiced(tag, text.clone()));
            conversation.merge_user_text(text);
        }
        AnswerIntervention::RideTail(Rider::Anchor { stale_line }) => {
            // The Anchor crosses the same emit seam as the Voiced riders so
            // the Session Log records what the model read (CONTEXT.md: every
            // rider is logged); the Transcript ignores the event. The anchor
            // Governor's stale-plan line is appended before the emit, so the
            // logged text and the injected text stay one string.
            let mut anchor = state.plan.anchor();
            if let Some(line) = stale_line {
                anchor.push_str("\n\n");
                anchor.push_str(&line);
            }
            state.emitter.emit(Event::anchor(anchor.clone()));
            conversation.inject_anchor(anchor);
        }
        AnswerIntervention::ReplaceResult { .. } | AnswerIntervention::AnnotateResult(_) => {
            unreachable!("per-call Interventions never ride the results tail")
        }
    }
}

// This Pass's Tool Calls as {name, input} pairs, in order.
fn pass_calls(blocks: &[ContentBlock]) -> Vec<(String, Value)> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

// Live context-pressure indication, once the Pass's usage is noted.
fn emit_context_pressure<D: TurnDeps>(state: &mut LoopState<'_, D>, conversation: &Conversation) {
    state.emitter.emit(Event::context_pressure(
        conversation.token_estimate(),
        conversation.context_budget,
        conversation.max_tokens_reserve,
    ));
}

// Maps the Response's content::Usage to the Conversation's Usage (only
// input_tokens is load-bearing for the estimate floor).
fn usage_of(response: &Response) -> conversation::Usage {
    match response.usage.input_tokens {
        Some(tokens) => conversation::Usage::with_input_tokens(tokens),
        None => conversation::Usage::empty(),
    }
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
    use crate::llm::response::Response;
    use crate::llm::stream::{Delta, MALFORMED_INPUT_SENTINEL};
    use crate::plugin::{Plugin, Token};
    use crate::plugins::Registered;
    use crate::session::connection::Connection;
    use crate::session::{Session, SessionOpts};
    use crate::test_support::Entry;
    use crate::test_support::FakeDeps;
    use crate::tool::ToolCtx;
    use crate::turn::deps::CompactError;
    use crate::turn::fixtures::*;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // The harness fixtures (session builders, Response builders, `run_with`,
    // event inspectors) live in `crate::turn::fixtures`, one set for the split
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
        // sentinel into it from INSIDE `complete` — after every delta has gone
        // through the streaming sink, immediately before `complete` returns.
        // If the loop buffered deltas and emitted after the call (the defect
        // ADR-0025 removes), every MessageUpdate would land AFTER the sentinel.
        let events_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let sentinel = "__complete_returning__";
        let sentinel_log = Arc::clone(&events_log);
        let entry = Entry::dynamic(
            vec![Delta::Text("hi ".into()), Delta::Text("there".into())],
            move |_req| {
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
            "every MessageUpdate must precede the sentinel — updates are emitted \
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
        assert_eq!(pressures[0].1, session.context_budget);
        assert_eq!(pressures[0].2, session.connection.max_tokens);
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
    async fn after_pass_stop_closes_the_turn_with_the_stopped_marker() {
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

    // ---- verify nudge -----------------------------------------------------

    #[tokio::test]
    async fn unverified_write_draws_verify_nudge_once_then_ends() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(text_end("all done")),
                just(text_end("declining to verify")),
            ],
        );
        let (outcome, deps) = run_with(&session, "write a file", deps).await;
        let (conv, _) = ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "w1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if !is_error))
                .unwrap_or(false)
        );
        let nudges: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::VerifyNudge { .. }))
            .collect();
        assert_eq!(nudges.len(), 1);
        assert!(
            matches!(nudges[0], Event::VerifyNudge { text } if text.contains("files changed but nothing verified"))
        );
        assert!(conv.messages.iter().any(|m| matches!(m.role, Role::User) && m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("files changed but nothing verified")))));
    }

    #[tokio::test]
    async fn editing_pass_after_nudge_rearms_verify_nudge() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(text_end("all done")),
                just(tool_use_result(
                    "w2",
                    "edit_file",
                    json!({"path": "a.txt", "old_str": "hi", "new_str": "ho"}),
                )),
                just(text_end("done again, still unverified")),
                just(text_end("concluding")),
            ],
        );
        let (outcome, deps) = run_with(&session, "write a file", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyNudge { .. })),
            2
        );
    }

    #[tokio::test]
    async fn run_command_after_write_suppresses_verify_nudge() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(text_end("verified")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "write and verify", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn denied_run_command_counts_as_made() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "mix test"}),
                )),
                just(text_end("stopping here")),
            ],
        )
        .with_approvals(vec![false]);
        let (outcome, deps) = run_with(&session, "write and verify", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "r1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if *is_error))
                .unwrap_or(false)
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn web_fetch_passes_the_approval_gate_showing_the_url() {
        let root = root();
        let session = session(root.path());

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/docs"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("hello from the web", "text/plain"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/docs", server.uri());

        let denied_url = "https://denied.example/secret";
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "f1",
                    "web_fetch",
                    json!({"url": denied_url}),
                )),
                just(tool_use_result(
                    "f2",
                    "web_fetch",
                    json!({"url": url.clone()}),
                )),
                just(text_end("read the docs")),
            ],
        )
        .with_approvals(vec![false, true]);

        let (outcome, deps) = run_with(&session, "look up the docs", deps).await;
        ok(&outcome);
        let evs = events(&deps);

        // Each fetch requested Approval showing the full URL as the string —
        // the same string Standing Approval would match exactly (ADR-0024).
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::ApprovalRequest { command, .. }
            if command == denied_url))
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::ApprovalRequest { command, .. }
            if command == &url))
        );

        // Denial is the denied is_error result; approval executes the fetch.
        assert!(
            find_tool_result(&evs, "f1")
                .map(|e| matches!(e, Event::ToolResult { is_error, content, .. }
                if *is_error && content == voice::command_denied()))
                .unwrap_or(false)
        );
        assert!(
            find_tool_result(&evs, "f2")
                .map(|e| matches!(e, Event::ToolResult { is_error, content, .. }
                if !is_error && content.contains("hello from the web")))
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn verify_nudge_skipped_when_turn_limit_reached() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(2);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "write a file", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(e, Event::VerifyNudge { .. })),
            0
        );
    }

    // ---- verification-failing finish gate ---------------------------------

    #[tokio::test]
    async fn finishing_after_failing_run_command_fires_nudge_once_and_loops() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(text_end("giving up in prose")),
                just(text_end("ok, fixed it")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        let (conv, _) = ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "r1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if *is_error))
                .unwrap_or(false)
        );
        let n: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::VerifyFailedNudge { .. }))
            .collect();
        assert_eq!(n.len(), 1);
        assert!(
            matches!(n[0], Event::VerifyFailedNudge { text } if text.contains("last command you ran failed"))
        );
        assert!(conv.messages.iter().any(|m| matches!(m.role, Role::User) && m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("last command you ran failed")))));
    }

    #[tokio::test]
    async fn passing_run_command_clears_state_and_finishes_without_it() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(tool_use_result(
                    "r2",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(text_end("verified green")),
            ],
        )
        .with_approvals(vec![true, true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "r1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if *is_error))
                .unwrap_or(false)
        );
        assert!(
            find_tool_result(&evs, "r2")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if !is_error))
                .unwrap_or(false)
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyFailedNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn turn_with_no_run_command_never_fires_verify_failed() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("l1", "list_files", json!({"path": "."}))),
                just(text_end("here they are")),
            ],
        );
        let (outcome, deps) = run_with(&session, "list files", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::VerifyFailedNudge { .. }
            )),
            0
        );
    }

    #[tokio::test]
    async fn idle_model_capped_at_one_verify_failed() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(text_end("still stuck")),
                just(text_end("still stuck again")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::VerifyFailedNudge { .. }
            )),
            1
        );
    }

    #[tokio::test]
    async fn tool_call_pass_between_finishes_rearms_verify_failed() {
        let root = root();
        write(&root, "a.ex", "content");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(text_end("giving up in prose")),
                just(tool_use_result("l1", "read_file", json!({"path": "a.ex"}))),
                just(text_end("finishing again, still red")),
                just(text_end("third finish, now capped")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "l1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if !is_error))
                .unwrap_or(false)
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyFailedNudge { .. })),
            2
        );
    }

    #[tokio::test]
    async fn immediate_empty_finish_after_firing_does_not_rearm_verify_failed() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(empty(StopReason::EndTurn)),
                just(empty(StopReason::EndTurn)),
                just(text_end("finally saying something")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::VerifyFailedNudge { .. }
            )),
            1
        );
    }

    #[tokio::test]
    async fn verify_failed_precedence_over_verify_nudge() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(text_end("done, but tests are red")),
                just(text_end("second finish")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "write and verify", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyFailedNudge { .. })),
            1
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn verify_failed_skipped_when_turn_limit_reached() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(2);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(text_end("done")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::VerifyFailedNudge { .. }
            )),
            0
        );
    }

    // ---- empty-response nudge ---------------------------------------------

    #[tokio::test]
    async fn empty_response_fires_nudge_once_and_loops() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(text_end("here is my next step")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        let evs = events(&deps);
        let n: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::EmptyResponseNudge { .. }))
            .collect();
        assert_eq!(n.len(), 1);
        assert!(
            matches!(n[0], Event::EmptyResponseNudge { text } if text.contains("reply was empty"))
        );
        assert!(conv.messages.iter().any(|m| matches!(m.role, Role::User)
            && m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("reply was empty"))
            )));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "here is my next step")
        );
    }

    #[tokio::test]
    async fn second_consecutive_empty_finishes_with_marker() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(empty(StopReason::EndTurn)),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::EmptyResponseNudge { .. }
            )),
            1
        );
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::empty_response_marker())
        );
    }

    #[tokio::test]
    async fn parroted_empty_marker_counts_as_empty_and_gets_nudge() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(text_end(voice::empty_response_marker())),
                just(text_end("the real conclusion")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::EmptyResponseNudge { .. }
            )),
            1
        );
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "the real conclusion")
        );
    }

    #[tokio::test]
    async fn prose_containing_marker_string_is_a_real_conclusion() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![just(text_end(
                "I kept hitting [empty response] markers; here is my summary.",
            ))],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::EmptyResponseNudge { .. }
            )),
            0
        );
    }

    #[tokio::test]
    async fn tool_call_pass_between_empties_rearms_it() {
        let root = root();
        write(&root, "a.ex", "content");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(tool_use_result("l1", "read_file", json!({"path": "a.ex"}))),
                just(empty(StopReason::EndTurn)),
                just(text_end("the actual answer at last")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        let evs = events(&deps);
        assert!(
            find_tool_result(&evs, "l1")
                .map(|e| matches!(e, Event::ToolResult { is_error, .. } if !is_error))
                .unwrap_or(false)
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::EmptyResponseNudge { .. })),
            2
        );
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == "the actual answer at last")
        );
    }

    #[tokio::test]
    async fn empty_nudge_never_fires_with_content() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(&session, vec![just(text_end("a normal, non-empty reply"))]);
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::EmptyResponseNudge { .. }
            )),
            0
        );
    }

    #[tokio::test]
    async fn verify_failed_precedence_over_empty_response() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "false"}),
                )),
                just(empty(StopReason::EndTurn)),
                just(text_end("fixed it")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "run the tests", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyFailedNudge { .. })),
            1
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::EmptyResponseNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn empty_nudge_skipped_when_turn_limit_reached() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(2);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("l1", "list_files", json!({"path": "."}))),
                just(empty(StopReason::EndTurn)),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(
                e,
                Event::EmptyResponseNudge { .. }
            )),
            0
        );
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::empty_response_marker())
        );
    }

    // ---- no-think rescue --------------------------------------------------

    #[tokio::test]
    async fn pass_after_empty_nudge_carries_no_think_next_does_not() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        ok(&outcome);
        let requests = deps.requests.lock().unwrap();
        assert!(!requests[0].no_think);
        assert!(requests[1].no_think);
        assert!(!requests[2].no_think);
    }

    #[tokio::test]
    async fn second_empty_makes_rescue_sticky() {
        let root = root();
        write(&root, "a.ex", "content");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(tool_use_result("l1", "read_file", json!({"path": "a.ex"}))),
                just(empty(StopReason::EndTurn)),
                just(tool_use_result("l2", "read_file", json!({"path": "a.ex"}))),
                just(text_end("the answer")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        let (conv, _) = ok(&outcome);
        let requests = deps.requests.lock().unwrap();
        assert!(!requests[0].no_think);
        assert!(requests[1].no_think);
        assert!(!requests[2].no_think);
        assert!(requests[3].no_think);
        assert!(requests[4].no_think);
        let lm = last_message(conv);
        assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "the answer"));
    }

    #[tokio::test]
    async fn no_request_carries_no_think_when_knob_off() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.no_think_rescue = Some(false);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(text_end("here is my next step")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::EmptyResponseNudge { .. })),
            1
        );
        let requests = deps.requests.lock().unwrap();
        assert!(!requests[0].no_think);
        assert!(!requests[1].no_think);
    }

    #[tokio::test]
    async fn rearm_case_makes_both_post_nudge_calls_rescue_calls() {
        let root = root();
        write(&root, "a.ex", "content");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(empty(StopReason::EndTurn)),
                just(tool_use_result("l1", "read_file", json!({"path": "a.ex"}))),
                just(empty(StopReason::EndTurn)),
                just(text_end("the actual answer at last")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do something", deps).await;
        ok(&outcome);
        let requests = deps.requests.lock().unwrap();
        assert!(!requests[0].no_think);
        assert!(requests[1].no_think);
        assert!(!requests[2].no_think);
        assert!(requests[3].no_think);
    }

    // ---- wrap-up warning --------------------------------------------------

    fn request_last_texts(req: &LlmRequest) -> Vec<String> {
        req.messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn wrap_up_warning_rides_tool_results_when_two_passes_remain() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(4);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end("done, wrapping up")),
            ],
        );
        let (outcome, deps) = run_with(&session, "big task", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let w: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::WrapUpWarning { .. }))
            .collect();
        assert_eq!(w.len(), 1);
        assert!(
            matches!(w[0], Event::WrapUpWarning { text } if *text == voice::wrap_up_warning(2))
        );
        // Pass 3's request (index 2) carries the warning text.
        let requests = deps.requests.lock().unwrap();
        assert!(
            request_last_texts(&requests[2])
                .iter()
                .any(|t| t.contains("wrap up now"))
        );
    }

    #[tokio::test]
    async fn wrap_up_warning_never_fires_far_from_limit() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "small task", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::WrapUpWarning { .. })),
            0
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::FinalPass { .. })),
            0
        );
    }

    // ---- Verification Pass ------------------------------------------------

    #[tokio::test]
    async fn unverified_writes_verification_prompt_replaces_warning_and_narrows_tools() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(4);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(text_end("Verified and done.")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "write a file", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let v: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::VerificationPass { .. }))
            .collect();
        assert_eq!(v.len(), 1);
        assert!(
            matches!(v[0], Event::VerificationPass { text } if text == voice::verification_pass_prompt())
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::WrapUpWarning { .. })),
            0
        );
        let requests = deps.requests.lock().unwrap();
        // Pass 3's request (index 2): run_command ONLY, and the prompt text.
        let names: Vec<&str> = requests[2].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["run_command"]);
        assert!(
            request_last_texts(&requests[2])
                .iter()
                .any(|t| t.contains("run_command ONLY"))
        );
    }

    #[tokio::test]
    async fn verified_writes_ordinary_warning_and_full_tool_list() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(4);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                )),
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(text_end("done and verified")),
            ],
        )
        .with_approvals(vec![true]);
        let (outcome, deps) = run_with(&session, "write a file", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::WrapUpWarning { .. })),
            1
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerificationPass { .. })),
            0
        );
        let requests = deps.requests.lock().unwrap();
        assert!(requests[2].tools.len() > 1);
    }

    // ---- final Pass -------------------------------------------------------

    #[tokio::test]
    async fn final_request_no_tools_and_prompt_conclusion_ends_turn() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(3);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end(
                    "Accomplished: listed files twice. Remains: nothing.",
                )),
            ],
        );
        let (outcome, deps) = run_with(&session, "big task", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());
        let evs = events(&deps);
        let f: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::FinalPass { .. }))
            .collect();
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0], Event::FinalPass { text } if text == voice::final_pass_prompt()));
        let requests = deps.requests.lock().unwrap();
        assert!(requests[2].tools.is_empty());
        assert!(
            request_last_texts(&requests[2])
                .iter()
                .any(|t| t.contains("tools are withdrawn"))
        );
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text.contains("Accomplished"))
        );
    }

    #[tokio::test]
    async fn final_pass_tool_markup_as_text_closes_on_marker() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(3);
        let session = session_with(root.path(), opts);
        let markup = "<tool_call>\n<function=run_command>\n<parameter=command>\nmix test\n</parameter>\n</function>\n</tool_call>";
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end(markup)),
            ],
        );
        let (outcome, _deps) = run_with(&session, "big task", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::turn_limit_marker())
        );
        assert!(!conv.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("<tool_call")))
        }));
    }

    #[tokio::test]
    async fn final_pass_conclusion_mentioning_markup_still_ends_end_turn() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(3);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end(
                    "Done. I could not run mix test - the <tool_call> was withdrawn.",
                )),
            ],
        );
        let (outcome, _deps) = run_with(&session, "big task", deps).await;
        let (_conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::end_turn());
    }

    #[tokio::test]
    async fn prose_preamble_does_not_launder_final_pass_markup() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(3);
        let session = session_with(root.path(), opts);
        let text = "I need to update the DESIGN.md file to reflect the new behavior:\n\n<tool_call>\n<function=edit_file>\n<parameter=path>\ndocs/DESIGN.md\n</parameter>\n</function>\n</tool_call>";
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end(text)),
            ],
        );
        let (outcome, _deps) = run_with(&session, "big task", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::turn_limit_marker())
        );
    }

    #[tokio::test]
    async fn model_answers_final_pass_with_tools_still_closes_on_marker() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(3);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(tool_use_result("t3", "list_files", json!({"path": "."}))),
            ],
        );
        let (outcome, _deps) = run_with(&session, "big task", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::turn_limit_marker())
        );
    }

    // ---- Recovery Turn (the close-and-recover Intervention) ----------------

    use crate::session::RecoveryShape;
    use crate::turn::governor::endgame::Recovery;

    fn recover(outcome: &Outcome) -> (&Conversation, log::StopReason, Recovery) {
        match outcome {
            Outcome::Recover(c, reason, recovery) => (c, *reason, *recovery),
            other => panic!("expected Recover, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cap_with_unverified_writes_carries_the_recovery_directive_out() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(1);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "w1",
                "write_file",
                json!({"path": "a.txt", "content": "hi"}),
            ))],
        );

        let (outcome, _deps) = run_with(&session, "write it", deps).await;
        let (conv, reason, recovery) = recover(&outcome);

        assert_eq!(reason, log::StopReason::TurnLimit);
        assert_eq!(
            recovery,
            Recovery {
                shape: RecoveryShape::Handoff,
                verification_failing: false,
            }
        );
        // The Conversation closed on the turn-limit marker like any limit close.
        let lm = last_message(conv);
        assert!(
            matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::turn_limit_marker())
        );
    }

    #[tokio::test]
    async fn a_cap_with_a_failing_verification_recovers_naming_the_failure() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(1);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "r1",
                "run_command",
                json!({"command": "false"}),
            ))],
        )
        .with_approvals(vec![true]);

        let (outcome, _deps) = run_with(&session, "run it", deps).await;
        let (_conv, _reason, recovery) = recover(&outcome);
        assert!(recovery.verification_failing);
    }

    #[tokio::test]
    async fn the_shape_setpoint_rides_the_directive() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(1);
        opts.recovery_shape = Some(RecoveryShape::Continuation);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "w1",
                "write_file",
                json!({"path": "a.txt", "content": "hi"}),
            ))],
        );

        let (outcome, _deps) = run_with(&session, "write it", deps).await;
        let (_conv, _reason, recovery) = recover(&outcome);
        assert_eq!(recovery.shape, RecoveryShape::Continuation);
    }

    #[tokio::test]
    async fn a_spent_recovery_budget_settles_a_plain_turn_limit() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(1);
        let session = session_with(root.path(), opts);
        let mut deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "w1",
                "write_file",
                json!({"path": "a.txt", "content": "hi"}),
            ))],
        );

        // The Agent stamps the recoveries this user request already consumed.
        let conv = conversation(&session, "write it");
        let plugins: Vec<Registered> = Vec::new();
        let ctx = tool_ctx(&session);
        let outcome = run(
            conv,
            &session,
            &plugins,
            &ctx,
            &mut deps,
            RunOpts {
                recoveries_used: 1,
                ..Default::default()
            },
        )
        .await;

        let (_conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
    }

    #[tokio::test]
    async fn recovery_limit_zero_disables_the_mechanic() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(1);
        opts.recovery_limit = Some(0);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![just(tool_use_result(
                "w1",
                "write_file",
                json!({"path": "a.txt", "content": "hi"}),
            ))],
        );

        let (outcome, _deps) = run_with(&session, "write it", deps).await;
        let (_conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
    }

    // A clean cap (no writes, no failing command) settling Ok is covered by
    // `turn_limit_stops_the_loop_after_n_passes` below.

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
    async fn turn_limit_stops_the_loop_after_n_passes() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(2);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "lib"}))),
            ],
        );
        let (outcome, _deps) = run_with(&session, "explore", deps).await;
        let (conv, stop) = ok(&outcome);
        assert_eq!(*stop, OutcomeStop::Reason(log::StopReason::TurnLimit));
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

    #[tokio::test]
    async fn identical_tool_call_gets_nudge_not_rerun() {
        let root = root();
        let session = session(root.path());
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
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "t1").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
        );
        let t2 = find_tool_result(&evs, "t2").unwrap();
        assert!(
            matches!(t2, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("identical Tool Call repeated"))
        );
    }

    #[tokio::test]
    async fn write_clears_duplicate_memory_fix_then_retest() {
        let root = root();
        let session = session(root.path());
        let fix_and_retest = Response {
            content: vec![
                ContentBlock::tool_use(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "fixed"}),
                ),
                ContentBlock::tool_use("r2", "run_command", json!({"command": "true"})),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "r1",
                    "run_command",
                    json!({"command": "true"}),
                )),
                just(fix_and_retest),
                just(text_end("verified")),
            ],
        )
        .with_approvals(vec![true, true]);
        let (outcome, deps) = run_with(&session, "test, fix, retest", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let r2 = find_tool_result(&evs, "r2").unwrap();
        assert!(
            matches!(r2, Event::ToolResult { is_error, content, .. } if !is_error && !content.contains("identical Tool Call repeated"))
        );
        assert_eq!(
            count_voiced(&evs, |e| matches!(e, Event::VerifyNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn only_calls_after_last_write_carry_into_next_duplicate_check() {
        let root = root();
        let session = session(root.path());
        let list_write_read = Response {
            content: vec![
                ContentBlock::tool_use("l1", "list_files", json!({"path": "."})),
                ContentBlock::tool_use(
                    "w1",
                    "write_file",
                    json!({"path": "a.txt", "content": "hi"}),
                ),
                ContentBlock::tool_use("rd1", "read_file", json!({"path": "a.txt"})),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        };
        let list_and_read_again = Response {
            content: vec![
                ContentBlock::tool_use("l2", "list_files", json!({"path": "."})),
                ContentBlock::tool_use("rd2", "read_file", json!({"path": "a.txt"})),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![
                just(list_write_read),
                just(list_and_read_again),
                just(text_end("done")),
                just(text_end("declining to verify")),
            ],
        );
        let (outcome, deps) = run_with(&session, "explore, write, re-check", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let l2 = find_tool_result(&evs, "l2").unwrap();
        assert!(
            matches!(l2, Event::ToolResult { is_error, content, .. } if !is_error && !content.contains("identical Tool Call repeated"))
        );
        let rd2 = find_tool_result(&evs, "rd2").unwrap();
        assert!(
            matches!(rd2, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("identical Tool Call repeated"))
        );
    }

    #[tokio::test]
    async fn third_consecutive_failure_gets_step_back_suffix() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "f1",
                    "read_file",
                    json!({"path": "no1.txt"}),
                )),
                just(tool_use_result(
                    "f2",
                    "read_file",
                    json!({"path": "no2.txt"}),
                )),
                just(tool_use_result(
                    "f3",
                    "read_file",
                    json!({"path": "no3.txt"}),
                )),
                just(text_end("giving up")),
            ],
        );
        let (outcome, deps) = run_with(&session, "read things", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let c = |id| match find_tool_result(&evs, id).unwrap() {
            Event::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert!(!c("f1").contains("consecutive"));
        assert!(!c("f2").contains("consecutive"));
        let c3 = c("f3");
        assert!(c3.contains("enoent"));
        assert!(c3.contains("[3 consecutive read_file failures - step back:"));
        assert!(c3.contains("file not found (enoent)"));
    }

    #[tokio::test]
    async fn success_resets_failure_counter() {
        let root = root();
        write(&root, "ok.txt", "content");
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "f1",
                    "read_file",
                    json!({"path": "no1.txt"}),
                )),
                just(tool_use_result(
                    "f2",
                    "read_file",
                    json!({"path": "no2.txt"}),
                )),
                just(tool_use_result(
                    "ok",
                    "read_file",
                    json!({"path": "ok.txt"}),
                )),
                just(tool_use_result(
                    "f3",
                    "read_file",
                    json!({"path": "no3.txt"}),
                )),
                just(tool_use_result(
                    "f4",
                    "read_file",
                    json!({"path": "no4.txt"}),
                )),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "read things", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "ok").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
        );
        let c4 = match find_tool_result(&evs, "f4").unwrap() {
            Event::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert!(!c4.contains("consecutive"));
    }

    #[tokio::test]
    async fn another_tools_success_does_not_reset_counter() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "f1",
                    "read_file",
                    json!({"path": "no1.txt"}),
                )),
                just(tool_use_result(
                    "f2",
                    "read_file",
                    json!({"path": "no2.txt"}),
                )),
                just(tool_use_result("ok", "list_files", json!({"path": "."}))),
                just(tool_use_result(
                    "f3",
                    "read_file",
                    json!({"path": "no3.txt"}),
                )),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "read things", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "ok").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
        );
        let c3 = match find_tool_result(&evs, "f3").unwrap() {
            Event::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert!(c3.contains("3 consecutive read_file failures"));
    }

    #[tokio::test]
    async fn duplicate_nudge_results_count_toward_failure_counter() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(tool_use_result("t3", "list_files", json!({"path": "."}))),
                just(tool_use_result("t4", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "list forever", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(
            matches!(find_tool_result(&evs, "t1").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
        );
        assert!(
            matches!(find_tool_result(&evs, "t2").unwrap(), Event::ToolResult { is_error, .. } if *is_error)
        );
        let c3 = match find_tool_result(&evs, "t3").unwrap() {
            Event::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        let c4 = match find_tool_result(&evs, "t4").unwrap() {
            Event::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert!(!c3.contains("consecutive"));
        assert!(c4.contains("identical Tool Call repeated"));
        assert!(c4.contains("3 consecutive list_files failures"));
    }

    #[tokio::test]
    async fn context_budget_exhaustion_fails_before_any_request() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.context_budget = Some(60);
        opts.eviction_slack = Some(0.0);
        opts.connection = Some(Connection::new("http://localhost:0/v1", "", "", 50));
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
                        json!({ MALFORMED_INPUT_SENTINEL: "{\"path\": \"oops" }),
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

    // ---- plugin lifecycle (ADR-0007) --------------------------------------

    struct HaltEdits;
    impl Plugin for HaltEdits {
        fn pre_run(&self, token: Token, _opts: &Value) -> Token {
            if token.tool == "edit_file" {
                token.halt("[edits are frozen by HaltEdits]")
            } else {
                token
            }
        }
    }

    struct Artifactor;
    impl Plugin for Artifactor {
        fn post_run(&self, token: Token, _opts: &Value) -> Token {
            let tool = token.tool.clone();
            token.put_artifact("mark", Value::String(tool))
        }
    }

    struct PreBoomer;
    impl Plugin for PreBoomer {
        fn pre_run(&self, _token: Token, _opts: &Value) -> Token {
            panic!("pre boom")
        }
    }

    async fn run_with_plugins(
        session: &Session,
        prompt: &str,
        mut deps: FakeDeps,
        plugins: Vec<Registered>,
    ) -> (Outcome, FakeDeps) {
        let conv = conversation(session, prompt);
        let ctx = tool_ctx(session);
        let outcome = run(conv, session, &plugins, &ctx, &mut deps, RunOpts::default()).await;
        (outcome, deps)
    }

    #[tokio::test]
    async fn halting_plugin_denies_the_call_with_its_own_wording() {
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
        let plugins = vec![Registered::new("HaltEdits", Box::new(HaltEdits), json!([]))];
        let (outcome, deps) = run_with_plugins(&session, "edit something", deps, plugins).await;
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
        let plugins = vec![Registered::new(
            "Artifactor",
            Box::new(Artifactor),
            json!([]),
        )];
        let (outcome, deps) = run_with_plugins(&session, "look around", deps, plugins).await;
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
    async fn crashing_plugin_is_fail_open() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("ok")),
            ],
        );
        let plugins = vec![Registered::new("PreBoomer", Box::new(PreBoomer), json!([]))];
        let (outcome, deps) = run_with_plugins(&session, "look around", deps, plugins).await;
        ok(&outcome);
        let evs = events(&deps);
        let pe = evs
            .iter()
            .find(|e| matches!(e, Event::PluginError { .. }))
            .unwrap();
        assert!(matches!(pe, Event::PluginError { plugin, stage, message }
            if plugin == "PreBoomer" && *stage == Stage::PreRun && message.contains("pre boom")));
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
                    "plan",
                    json!({"plan": "Goal: X. 1. read [ ]"}),
                )),
                just(text_end("planned, done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do X", deps).await;
        ok(&outcome);
        let plans = deps.plans.lock().unwrap();
        assert_eq!(plans.as_slice(), &["Goal: X. 1. read [ ]".to_string()]);
    }

    #[tokio::test]
    async fn failed_plan_call_does_not_store_a_plan() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("p1", "plan", json!({}))),
                just(text_end("recovered")),
            ],
        );
        let (outcome, deps) = run_with(&session, "do X", deps).await;
        ok(&outcome);
        assert!(deps.plans.lock().unwrap().is_empty());
    }

    // ---- Anchor injection -------------------------------------------------

    fn tool_each_pass(count: usize) -> Vec<Entry> {
        (1..=count)
            .map(|i| {
                if i == count {
                    just(text_end("done"))
                } else {
                    just(tool_use_result(
                        &format!("t{i}"),
                        "list_files",
                        json!({"path": "."}),
                    ))
                }
            })
            .collect()
    }

    fn anchors_in(conv: &Conversation) -> Vec<String> {
        conv.messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } if voice::is_anchor(b) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn injects_anchor_every_interval_and_not_between() {
        let root = root();
        write(&root, "a.txt", "");
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(5);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);
        let deps = deps_for(&session, tool_each_pass(12));
        let (outcome, _deps) = run_with(&session, "the original task", deps).await;
        let (conv, _) = ok(&outcome);
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.iter().all(|a| a.contains("the original task")));
    }

    #[tokio::test]
    async fn injected_anchor_carries_current_plan_verbatim() {
        let root = root();
        write(&root, "a.txt", "");
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(2);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "p1",
                    "plan",
                    json!({"plan": "Goal: ship. 1. code [ ]"}),
                )),
                just(tool_use_result("t2", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        );
        let (outcome, _deps) = run_with(&session, "ship it", deps).await;
        let (conv, _) = ok(&outcome);
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 1);
        assert!(anchors[0].contains("Goal: ship. 1. code [ ]"));
        assert!(anchors[0].contains("ship it"));
    }

    #[tokio::test]
    async fn stale_plan_line_rides_every_qualifying_anchor_and_is_logged_as_injected() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(2);
        opts.plan_stale_after = Some(2);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);

        // Pass 1 sets the Plan; Passes 2-8 keep writing without touching it —
        // the audited f5 shape (a pass-5 plan re-injected verbatim while the
        // model debugged 20 passes deep).
        let mut script = vec![just(tool_use_result(
            "p1",
            "plan",
            json!({"plan": "Goal: ship. 1. code [ ]"}),
        ))];
        for i in 2..=8 {
            script.push(just(tool_use_result(
                &format!("w{i}"),
                "write_file",
                json!({"path": format!("f{i}.txt"), "content": "x"}),
            )));
        }
        script.push(just(text_end("done")));
        // The unverified writes draw the verify Nudge one extra Pass.
        script.push(just(text_end("done, unverified")));

        let deps = deps_for(&session, script);
        let (outcome, deps) = run_with(&session, "ship it", deps).await;
        let (conv, _) = ok(&outcome);

        // Anchors ride Passes 2, 4, 6, 8. Passes since the Pass-1 update are
        // 1, 3, 5, 7: past the threshold of 2 from the second Anchor on, and
        // the line rides EVERY qualifying Anchor with a fresh count.
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 4);
        assert!(!anchors[0].contains("has not changed"));
        assert!(anchors[1].ends_with(&voice::stale_plan_line(3)));
        assert!(anchors[2].ends_with(&voice::stale_plan_line(5)));
        assert!(anchors[3].ends_with(&voice::stale_plan_line(7)));

        // Rider persistence: the emitted Anchor events (what the Session Log
        // records) are byte-for-byte the injected blocks, stale line included.
        let logged: Vec<String> = events(&deps)
            .iter()
            .filter_map(|e| match e {
                Event::Anchor { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(logged, anchors);
    }

    #[tokio::test]
    async fn a_plan_updated_as_it_goes_is_never_called_stale() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(2);
        opts.plan_stale_after = Some(2);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);

        // Writes land every Pass, but the model refreshes its Plan on Pass 4
        // — inside the window every Anchor would otherwise go stale in.
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("p1", "plan", json!({"plan": "1. a [ ]"}))),
                just(tool_use_result(
                    "w2",
                    "write_file",
                    json!({"path": "f2.txt", "content": "x"}),
                )),
                just(tool_use_result(
                    "w3",
                    "write_file",
                    json!({"path": "f3.txt", "content": "x"}),
                )),
                just(tool_use_result("p4", "plan", json!({"plan": "1. a [x]"}))),
                just(tool_use_result(
                    "w5",
                    "write_file",
                    json!({"path": "f5.txt", "content": "x"}),
                )),
                just(text_end("done")),
                // The unverified writes draw the verify Nudge one extra Pass.
                just(text_end("done, unverified")),
            ],
        );
        let (outcome, _deps) = run_with(&session, "ship it", deps).await;
        let (conv, _) = ok(&outcome);

        // Anchors on Passes 2 and 4: 1 Pass since the Pass-1 plan, then 0
        // since the Pass-4 refresh — the update reset the clock.
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.iter().all(|a| !a.contains("has not changed")));
    }

    #[tokio::test]
    async fn a_plan_carried_from_a_previous_turn_goes_stale_from_turn_start() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(2);
        opts.plan_stale_after = Some(2);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);

        // The Plan rides in through RunOpts (a previous Turn set it) and this
        // Turn only writes: the recency clock runs from Turn start.
        let mut deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "w1",
                    "write_file",
                    json!({"path": "f1.txt", "content": "x"}),
                )),
                just(tool_use_result(
                    "w2",
                    "write_file",
                    json!({"path": "f2.txt", "content": "x"}),
                )),
                just(tool_use_result(
                    "w3",
                    "write_file",
                    json!({"path": "f3.txt", "content": "x"}),
                )),
                just(tool_use_result(
                    "w4",
                    "write_file",
                    json!({"path": "f4.txt", "content": "x"}),
                )),
                just(text_end("done")),
                // The unverified writes draw the verify Nudge one extra Pass.
                just(text_end("done, unverified")),
            ],
        );

        let conv = conversation(&session, "keep going");
        let plugins: Vec<Registered> = Vec::new();
        let ctx = tool_ctx(&session);
        let outcome = run(
            conv,
            &session,
            &plugins,
            &ctx,
            &mut deps,
            RunOpts {
                plan: Some("Goal: ship. 1. code [ ]".to_string()),
                original_task: Some("ship it".to_string()),
                ..Default::default()
            },
        )
        .await;
        let (conv, _) = ok(&outcome);

        // Anchors on Passes 2 and 4: 1 then 3 Passes since Turn start — the
        // carried Plan crosses the threshold without ever being set this Turn.
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 2);
        assert!(!anchors[0].contains("has not changed"));
        assert!(anchors[1].ends_with(&voice::stale_plan_line(3)));
    }

    #[tokio::test]
    async fn injects_anchor_first_pass_after_compaction_off_interval() {
        let root = root();
        write(&root, "a.txt", "");
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(999);
        opts.turn_limit = Some(50);
        let session = session_with(root.path(), opts);

        // Reactive compaction: a big conversation that only fits once the
        // compactor drops the bulky assistant message, keeping the head.
        let compacted = Arc::new(Mutex::new(0usize));
        let c = Arc::clone(&compacted);
        let mut deps = deps_for(
            &session,
            vec![
                just(tool_use_result("t1", "list_files", json!({"path": "."}))),
                just(text_end("done")),
            ],
        )
        .with_compact(move |conv: Conversation| {
            *c.lock().unwrap() += 1;
            let mut out = conv.clone();
            out.messages = vec![conv.messages[0].clone()];
            Ok(out)
        });
        let _ = deps.requests_handle();

        let mut conv =
            Conversation::new("sys", crate::conversation::ConversationOpts::new(1000, 100));
        conv.add_user_text("compact me");
        conv.add_assistant_blocks(vec![ContentBlock::text("x".repeat(4000))]);

        let plugins: Vec<Registered> = Vec::new();
        let ctx = tool_ctx(&session);
        let outcome = run(
            conv,
            &session,
            &plugins,
            &ctx,
            &mut deps,
            RunOpts::default(),
        )
        .await;
        let (conv, _) = ok(&outcome);
        assert!(*compacted.lock().unwrap() >= 1);
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 1);
        assert!(anchors[0].contains("compact me"));
    }

    // ---- proactive Compaction (ADR-0012) ----------------------------------

    #[tokio::test]
    async fn proactive_compacts_before_first_pass_and_refreshes_anchor() {
        let root = root();
        let mut opts = SessionOpts::default();
        opts.anchor_interval = Some(999);
        opts.turn_limit = Some(50);
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
            crate::conversation::ConversationOpts::new(4000, 100).eviction_slack(0.3),
        );
        conv.add_user_text("original task");
        conv.add_assistant_blocks(vec![ContentBlock::text("x".repeat(12_000))]);

        let plugins: Vec<Registered> = Vec::new();
        let ctx = tool_ctx(&session);
        let outcome = run(
            conv,
            &session,
            &plugins,
            &ctx,
            &mut deps,
            RunOpts::default(),
        )
        .await;
        let (conv, _) = ok(&outcome);
        assert!(*compacted.lock().unwrap());
        // Compaction ran before the first model call: the first recorded request
        // reflects the compacted (single-message) conversation.
        let requests = deps.requests.lock().unwrap();
        assert_eq!(requests[0].messages.len(), 1);
        let anchors = anchors_in(conv);
        assert_eq!(anchors.len(), 1);
        assert!(anchors[0].contains("original task"));
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

    // ---- Scout isolation (adapted: the real Scout is a later phase) --------
    //
    // baud drives the Scout's internal Passes through the same FakeLLM. The Rust
    // Scout is not yet ported, so we wire the ctx's `scout` capture to return a
    // canned ScoutOutcome — exercising the SAME loop behaviour (only the explore
    // Tool Call and its Tool Result enter the Conversation; a Scout failure
    // becomes an ordinary is_error Tool Result, never failing the Turn).

    fn ctx_with_scout(session: &Session, outcome: crate::scout::ScoutOutcome) -> ToolCtx {
        use std::sync::Arc;
        let mut ctx = session.tool_ctx();
        let out = outcome;
        ctx.scout = Some(Arc::new(move |_task: String| {
            let out = out.clone();
            Box::pin(async move { out })
        }));
        ctx
    }

    async fn run_with_ctx(
        session: &Session,
        prompt: &str,
        mut deps: FakeDeps,
        ctx: ToolCtx,
    ) -> (Outcome, FakeDeps) {
        let conv = conversation(session, prompt);
        let plugins: Vec<Registered> = Vec::new();
        let outcome = run(conv, session, &plugins, &ctx, &mut deps, RunOpts::default()).await;
        (outcome, deps)
    }

    #[tokio::test]
    async fn only_explore_call_and_report_enter_the_conversation() {
        let root = root();
        write(&root, "widget.ex", "defmodule Widget do\nend\n");
        let report = "Locations: widget.ex:1. How it works: defines Widget.";
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "ex_1",
                    "explore",
                    json!({"task": "where is Widget"}),
                )),
                just(text_end("Done: Widget is in widget.ex.")),
            ],
        );
        let ctx = ctx_with_scout(&session, crate::scout::ScoutOutcome::Ok(report.to_string()));
        let (outcome, deps) = run_with_ctx(&session, "find Widget", deps, ctx).await;
        let (conv, _) = ok(&outcome);

        // Only the explore Tool Call entered as an assistant tool_use.
        let tool_uses: Vec<String> = conv
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::Assistant))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert!(tool_uses.contains(&"explore".to_string()));
        assert!(!tool_uses.contains(&"list_files".to_string()));

        // The report entered as the Tool Result for ex_1.
        let explore_results: Vec<String> = conv
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == "ex_1" => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(explore_results.len(), 1);
        assert!(explore_results[0].contains("widget.ex:1"));

        let evs = events(&deps);
        assert!(evs.iter().any(
            |e| matches!(e, Event::ToolCall { id, name, .. } if id == "ex_1" && name == "explore")
        ));
        assert!(evs.iter().any(
            |e| matches!(e, Event::ToolResult { id, name, .. } if id == "ex_1" && name == "explore")
        ));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, Event::ToolCall { name, .. } if name == "list_files"))
        );
    }

    #[tokio::test]
    async fn scout_failure_becomes_error_result_never_failing_turn() {
        let root = root();
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "ex_1",
                    "explore",
                    json!({"task": "find something"}),
                )),
                just(text_end("Handled the failed exploration.")),
            ],
        );
        let ctx = ctx_with_scout(
            &session,
            crate::scout::ScoutOutcome::LlmError {
                partial: String::new(),
            },
        );
        let (outcome, deps) = run_with_ctx(&session, "explore then recover", deps, ctx).await;
        ok(&outcome);
        let evs = events(&deps);
        assert!(evs.iter().any(|e| matches!(e, Event::ToolResult { id, name, is_error, .. } if id == "ex_1" && name == "explore" && *is_error)));
    }

    // ---- explore nudge ----------------------------------------------------

    fn seed_abc(root: &TempDir) {
        for f in ["a", "b", "c"] {
            write(root, &format!("{f}.txt"), "x");
        }
    }

    #[tokio::test]
    async fn explore_nudge_fires_on_third_read_only_pass() {
        let root = root();
        seed_abc(&root);
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("r1", "read_file", json!({"path": "a.txt"}))),
                just(tool_use_result("r2", "read_file", json!({"path": "b.txt"}))),
                just(tool_use_result("r3", "read_file", json!({"path": "c.txt"}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "evaluate this project", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let n: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::ExploreNudge { .. }))
            .collect();
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0], Event::ExploreNudge { text } if text.contains("explore")));
        // Rides the 3rd Pass's tool-results user message (request index 3).
        let requests = deps.requests.lock().unwrap();
        let last = requests[3].messages.last().unwrap();
        assert!(matches!(&last.role, Role::User));
        assert!(
            last.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("explore")))
        );
        assert!(last.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "r3")
        ));
    }

    #[tokio::test]
    async fn non_exploration_pass_resets_streak() {
        let root = root();
        seed_abc(&root);
        let session = session(root.path());
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result("r1", "read_file", json!({"path": "a.txt"}))),
                just(tool_use_result(
                    "p1",
                    "plan",
                    json!({"plan": "1. read things"}),
                )),
                just(tool_use_result("r2", "read_file", json!({"path": "b.txt"}))),
                just(tool_use_result("r3", "read_file", json!({"path": "c.txt"}))),
                just(text_end("done")),
            ],
        );
        let (outcome, deps) = run_with(&session, "look, plan, look again", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(e, Event::ExploreNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn three_search_shaped_run_commands_fire_the_nudge() {
        let root = root();
        write(&root, "a.txt", "a\n");
        write(&root, "b.txt", "b\n");
        write(&root, "c.txt", "c\n");
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(20);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "c1",
                    "run_command",
                    json!({"command": "grep a a.txt"}),
                )),
                just(tool_use_result(
                    "c2",
                    "run_command",
                    json!({"command": "grep b b.txt"}),
                )),
                just(tool_use_result(
                    "c3",
                    "run_command",
                    json!({"command": "grep c c.txt"}),
                )),
                just(text_end("done")),
            ],
        )
        .with_approvals(vec![true, true, true]);
        let (outcome, deps) = run_with(&session, "explore via shell", deps).await;
        ok(&outcome);
        let evs = events(&deps);
        let n: Vec<&Event> = evs
            .iter()
            .filter(|e| matches!(e, Event::ExploreNudge { .. }))
            .collect();
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0], Event::ExploreNudge { text } if text.contains("explore")));
    }

    #[tokio::test]
    async fn mix_test_run_command_resets_streak() {
        let root = root();
        write(&root, "a.txt", "a\n");
        write(&root, "b.txt", "b\n");
        write(&root, "c.txt", "c\n");
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(20);
        let session = session_with(root.path(), opts);
        let deps = deps_for(
            &session,
            vec![
                just(tool_use_result(
                    "c1",
                    "run_command",
                    json!({"command": "grep a a.txt"}),
                )),
                just(tool_use_result(
                    "c2",
                    "run_command",
                    json!({"command": "grep b b.txt"}),
                )),
                just(tool_use_result(
                    "v1",
                    "run_command",
                    json!({"command": "mix test"}),
                )),
                just(tool_use_result(
                    "c3",
                    "run_command",
                    json!({"command": "grep c c.txt"}),
                )),
                just(text_end("done")),
            ],
        )
        .with_approvals(vec![true, true, true, true]);
        let (outcome, deps) = run_with(&session, "explore then verify", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(e, Event::ExploreNudge { .. })),
            0
        );
    }

    #[tokio::test]
    async fn explore_nudge_fires_again_on_sixth_pass() {
        let root = root();
        for n in 1..=6 {
            std::fs::create_dir_all(root.path().join(format!("d{n}"))).unwrap();
        }
        let mut opts = SessionOpts::default();
        opts.turn_limit = Some(20);
        let session = session_with(root.path(), opts);
        let mut entries: Vec<Entry> = (1..=6)
            .map(|n| {
                just(tool_use_result(
                    &format!("l{n}"),
                    "list_files",
                    json!({"path": format!("d{n}")}),
                ))
            })
            .collect();
        entries.push(just(text_end("done")));
        let deps = deps_for(&session, entries);
        let (outcome, deps) = run_with(&session, "keep exploring", deps).await;
        ok(&outcome);
        assert_eq!(
            count_voiced(&events(&deps), |e| matches!(e, Event::ExploreNudge { .. })),
            2
        );
    }
}
