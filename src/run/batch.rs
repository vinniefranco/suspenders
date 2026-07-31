//! Tool Call batch - executing one Pass's Tool Calls (carved from the Run
//! Loop; "batch" is the domain word: Steering is delivered after a Tool Call
//! batch completes, and ADR-0009's truncated response means none of its batch
//! executes).
//!
//! [`execute_tools`] runs a Pass's Tool Calls in emission order. Each call goes
//! through the gates in sequence: the malformed-input sentinel (the LLM layer
//! tags inputs that never parsed - those are answered, never run), then the
//! Extension lifecycle (ADR-0007: pre_run - which may halt the call with the
//! Extension's own wording - execution, post_run/Shaping), with Approval
//! (ADR-0005) requested between pre_run and execution for the tools that
//! require it, on the Middleware-adjusted input. Once the batch finishes the
//! Conversation is checkpointed with only the answered Tool Calls, so the
//! checkpoint never persists an unanswered tool_use block.
//!
//! The loop skeleton lives in [`super::loop_`]; how a Run ends when the model
//! stops calling tools lives in [`super::finish`].

use std::collections::HashMap;

use serde_json::Value;

use crate::approvals;
use crate::content::{ContentBlock, ResultBlock, result_blocks_text};
use crate::conversation::Conversation;
use crate::event::Event;
use crate::extensions;
use crate::llm::malformed_tool_input;
use crate::middleware::Token;
use crate::plan::Update;
use crate::run::deps::RunDeps;
use crate::run::loop_::LoopState;
use crate::voice;

// Run tool calls in emission order; results keep that order. Checkpoint ONCE
// after the batch, carrying every answered Tool Call.
pub(super) async fn execute_tools<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    blocks: &[ContentBlock],
) -> (Vec<ContentBlock>, Conversation) {
    let mut results: Vec<ContentBlock> = Vec::new();
    for block in blocks.iter().filter(|b| b.is_tool_use()) {
        let result = execute_tool(state, block).await;
        results.push(result);
    }
    // Per-BATCH, not per-tool, is the correct checkpoint granularity: crash
    // recency comes from the Session Log's per-event tool_result entries
    // (ADR-0010, flushed as each result is emitted), so a mid-batch crash keeps
    // completed work through the log - not this checkpoint. The in-memory
    // checkpoint is only the settlement fallback, so one over the finished
    // batch is enough (and must not be dropped: it holds in-flight settlement
    // state should the Run end here).
    let provenance = state.deps.provenance();
    let checkpoint = build_checkpoint(&conversation, blocks, &results, provenance);
    state.deps.checkpoint(&checkpoint);
    (results, conversation)
}

// The end-of-batch checkpoint: only the answered Tool Calls, paired with their
// results (never a bare, unanswered tool_use block). The kept blocks are the
// model's, so the message carries the Run's captured Provenance (ADR-0037) -
// this checkpoint becomes the settled Conversation if the Run ends here.
fn build_checkpoint(
    conversation: &Conversation,
    blocks: &[ContentBlock],
    results: &[ContentBlock],
    provenance: crate::content::Provenance,
) -> Conversation {
    use std::collections::HashSet;
    let answered: HashSet<&str> = results
        .iter()
        .filter_map(|r| match r {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let kept: Vec<ContentBlock> = blocks
        .iter()
        .filter(|b| match b {
            ContentBlock::ToolUse { id, .. } => answered.contains(id.as_str()),
            _ => true,
        })
        .cloned()
        .collect();

    let mut conv = conversation.clone();
    conv.add_assistant_response(kept, provenance);
    conv.add_tool_results(results.to_vec(), Vec::new());
    conv
}

async fn execute_tool<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    block: &ContentBlock,
) -> ContentBlock {
    let (id, name, input) = match block {
        ContentBlock::ToolUse { id, name, input } => (id.clone(), name.clone(), input.clone()),
        _ => unreachable!("execute_tool only sees tool_use blocks"),
    };

    state.emitter.emit(Event::tool_call(
        id.clone(),
        name.clone(),
        display_input(&input),
    ));

    let answer = run_block(state, &name, &input).await;

    let content = answer.content;
    let is_error = answer.is_error;

    maybe_store_plan(state, &name, &input, is_error);

    // The UI event carries the text projection (ADR-0059): a media block renders
    // as a short placeholder there, while the Conversation keeps the full block
    // list for the wire.
    state.emitter.emit(Event::tool_result(
        id.clone(),
        name.clone(),
        result_blocks_text(&content),
        is_error,
        answer.artifacts,
    ));

    ContentBlock::tool_result_blocks(id, content, is_error)
}

// A successful todo_write Tool Call replaces the Plan's task list and stores its
// rendered form through the set_plan Dep; the Loop keeps this Run's copy.
fn maybe_store_plan<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    if let Update::Updated(plan) = state.plan.update(name, input, is_error) {
        state.deps.set_plan(plan.render());
        state.plan = plan;
    }
}

pub(super) fn display_input(input: &Value) -> Value {
    if malformed_tool_input(input).is_some() {
        Value::Object(Default::default())
    } else {
        input.clone()
    }
}

// How the batch answered one Tool Call: the Tool Result the model will read
// plus whether it was an error. Built only through the constructors below.
struct Answer {
    content: Vec<ResultBlock>,
    is_error: bool,
    artifacts: HashMap<String, Value>,
}

impl Answer {
    /// A malformed-input answer is recorded like any error - it reads as a
    /// run.
    fn malformed(raw: &str) -> Self {
        Answer::text(voice::malformed_input(raw), true, Default::default())
    }

    /// A Middleware halt reads as a failed run.
    fn halted(reason: String, artifacts: HashMap<String, Value>) -> Self {
        Answer::text(reason, true, artifacts)
    }

    /// An Approval denial (ADR-0005): the command never ran.
    fn denied() -> Self {
        Answer::text(voice::command_denied(), true, Default::default())
    }

    /// The Extension pipeline executed the call: the shaped block list rides
    /// straight through (ADR-0059).
    fn ran(result: extensions::PipelineResult) -> Self {
        Answer {
            content: result.content,
            is_error: result.is_error,
            artifacts: result.artifacts,
        }
    }

    /// A single-Text-block answer - the shape every Voice-worded outcome
    /// (malformed/halted/denied) takes.
    fn text(content: impl Into<String>, is_error: bool, artifacts: HashMap<String, Value>) -> Self {
        Answer {
            content: vec![ResultBlock::text(content)],
            is_error,
            artifacts,
        }
    }
}

// The Extension lifecycle (ADR-0007): the LLM layer tags malformed inputs -
// never run those. Otherwise pre_run, Approval on the Middleware-adjusted
// command, and execution with post_run and Shaping.
async fn run_block<D: RunDeps>(state: &mut LoopState<'_, D>, name: &str, input: &Value) -> Answer {
    if let Some(raw) = malformed_tool_input(input) {
        return Answer::malformed(raw);
    }

    run_lifecycle(state, name, input).await
}

async fn run_lifecycle<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> Answer {
    let token = Token::new(name, input.clone(), state.tool_ctx.clone());
    let (token, failures) = extensions::pre_run(state.extensions, token);
    emit_extension_errors(state, &failures);

    if token.halted {
        let reason = token.halt_reason.clone().unwrap_or_default();
        return Answer::halted(reason, token.artifacts.clone());
    }

    // The one Approval seam: Some(text) gates and text is exactly what the
    // user reads (the command, or web_fetch's URL), read from the
    // Middleware-adjusted input; None means no gate (approvals::gate_text).
    match approvals::gate_text(name, &token.input) {
        Some(text) => {
            let id = new_ref();
            if state.deps.request_approval(id, text).await {
                execute_token(state, token).await
            } else {
                Answer::denied()
            }
        }
        None => execute_token(state, token).await,
    }
}

async fn execute_token<D: RunDeps>(state: &mut LoopState<'_, D>, token: Token) -> Answer {
    let (result, failures) = extensions::execute(state.extensions, token).await;
    emit_extension_errors(state, &failures);
    Answer::ran(result)
}

fn emit_extension_errors<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    failures: &[extensions::Failure],
) {
    for failure in failures {
        state.emitter.emit(Event::extension_error(
            failure.extension.clone(),
            failure.stage,
            failure.message.clone(),
        ));
    }
}

// The per-call Approval reference (baud's `make_ref()`), an opaque unique id.
fn new_ref() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("approval-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Usage};
    use crate::llm::response::{Response, StopReason};
    use crate::run::Outcome;
    use crate::run::fixtures::{deps_for, events, just, run_with, session, text_end};
    use crate::test_support::Entry;
    use serde_json::json;
    use tempfile::TempDir;

    // Batch-sequential invariant (ADR-0049 Risk 1): the whole newest-pending
    // attach safety rests on gated calls running ONE AT A TIME. With two gated
    // `run_command` calls in a single Pass, the second's approval/execution must
    // NOT begin before the first fully resolves. We assert the event ordering:
    // the log shows r1's ApprovalRequest → r1's ToolResult BEFORE r2's
    // ApprovalRequest ever appears (no interleaving).
    #[tokio::test]
    async fn two_gated_calls_never_overlap_the_second_waits_for_the_first() {
        let root = TempDir::new().unwrap();
        let session = session(root.path());
        let two_gated_pass = Response {
            content: vec![
                ContentBlock::tool_use("g1", "run_shell_command", json!({"command": "echo first"})),
                ContentBlock::tool_use(
                    "g2",
                    "run_shell_command",
                    json!({"command": "echo second"}),
                ),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        };
        let deps = deps_for(
            &session,
            vec![Entry::just(two_gated_pass), just(text_end("done"))],
        )
        // Approve both, front-to-back.
        .with_approvals(vec![true, true]);
        let (outcome, deps) = run_with(&session, "run two", deps).await;
        assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

        let evs = events(&deps);
        // Index the boundary events by their distinguishing text/id.
        let pos = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).expect("event present");
        let req1 = pos(
            &|e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo first"),
        );
        let res1 = pos(&|e| matches!(e, Event::ToolResult { id, .. } if id == "g1"));
        let req2 = pos(
            &|e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo second"),
        );
        let res2 = pos(&|e| matches!(e, Event::ToolResult { id, .. } if id == "g2"));

        // r1 is requested, then resolved to a result, ALL before r2 is even
        // requested - the sequential gate proven, no concurrency.
        assert!(req1 < res1, "r1 requested before it resolves");
        assert!(res1 < req2, "r1 fully resolves before r2's approval begins");
        assert!(req2 < res2, "r2 requested before it resolves");
    }

    // The Answer constructors fuse the Voice's wording with the ran-fact so
    // the pairing cannot drift (CONTEXT.md: Answer).

    #[test]
    fn a_denial_pairs_the_command_denied_voice_with_the_denied_fact() {
        // ADR-0005: the Approval gate; the command never ran.
        let answer = Answer::denied();
        assert_eq!(result_blocks_text(&answer.content), voice::command_denied());
        assert!(answer.is_error);
    }

    #[test]
    fn a_malformed_input_answer_reads_as_a_run() {
        let answer = Answer::malformed("{not json");
        assert_eq!(
            result_blocks_text(&answer.content),
            voice::malformed_input("{not json")
        );
        assert!(answer.is_error);
    }

    #[test]
    fn an_extension_halt_reads_as_a_failed_run() {
        let answer = Answer::halted("blocked by plugin".to_string(), Default::default());
        assert_eq!(result_blocks_text(&answer.content), "blocked by plugin");
        assert!(answer.is_error);
    }
}
