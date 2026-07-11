//! Tool Call batch — executing one Pass's Tool Calls (carved from the Turn
//! Loop port of baud's `Baud.Turn.Loop`; "batch" is the domain word: Steering
//! is delivered after a Tool Call batch completes, and ADR-0009's truncated
//! response means none of its batch executes).
//!
//! [`execute_tools`] runs a Pass's Tool Calls in emission order. Each call goes
//! through the gates in sequence: the malformed-input sentinel (the LLM layer
//! tags inputs that never parsed — those are answered, never run), the Tool
//! Call answering arbiter before execution ([`governor::answer_sent`],
//! ADR-0026: an identical repeat draws a replacement Tool Result instead of a
//! rerun), then the Plugin lifecycle (ADR-0007: pre_run — which may halt the
//! call with the plugin's own wording — execution, post_run/Shaping), with
//! Approval (ADR-0005) requested between pre_run and execution for the tools
//! that require it, on the plugin-adjusted input; the arbiter is consulted
//! again after execution ([`governor::answer_read`] — the consecutive-failure
//! annotation). After every result the Conversation is checkpointed with only
//! the answered Tool Calls, so a crash mid-batch never persists an unanswered
//! tool_use block.
//!
//! The loop skeleton lives in [`super::loop_`]; how a Turn ends when the model
//! stops calling tools lives in [`super::finish`].

use serde_json::Value;

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::stream::MALFORMED_INPUT_SENTINEL;
use crate::plan::Update;
use crate::plugin::Token;
use crate::plugins;
use crate::tools;
use crate::turn::deps::TurnDeps;
use crate::turn::governor::ledger::ToolResult;
use crate::turn::governor::{self, AnswerIntervention};
use crate::turn::loop_::LoopState;
use crate::voice;

// Run tool calls in emission order; results keep that order. After each result,
// checkpoint with only the answered Tool Calls. After the batch the duplicate
// memory advances and the Ledger's failure-recency clock ticks.
pub(super) async fn execute_tools<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    blocks: &[ContentBlock],
) -> (Vec<ContentBlock>, Conversation) {
    let mut results: Vec<ContentBlock> = Vec::new();
    for block in blocks.iter().filter(|b| b.is_tool_use()) {
        let result = execute_tool(state, block).await;
        results.push(result);
        let checkpoint = build_checkpoint(&conversation, blocks, &results);
        state.deps.checkpoint(&checkpoint);
    }
    state.governors.next_pass();
    state.ledger.close_batch();
    (results, conversation)
}

// The checkpoint after a partial batch: only the answered Tool Calls, paired
// with their results.
fn build_checkpoint(
    conversation: &Conversation,
    blocks: &[ContentBlock],
    results: &[ContentBlock],
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
    conv.add_assistant_blocks(kept);
    conv.add_tool_results(results.to_vec(), Vec::new());
    conv
}

async fn execute_tool<D: TurnDeps>(
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

    let (raw_content, is_error, artifacts) = run_block(state, &name, &input).await;

    // The outcome's facts go on the Ledger first, written once at this firing
    // site (ADR-0026) — replaced results included: a replaced duplicate still
    // counts toward the failure tally, and a duplicated write/run_command
    // still moves the verify state. Then the answering moment's second
    // consultation: the arbiter judges what the model will READ, possibly
    // annotating it with the consecutive-failure suffix.
    let result = ToolResult {
        content: &raw_content,
        is_error,
    };
    state.ledger.record_result(&name, &result);
    let content =
        match governor::answer_read(&state.ledger, &mut state.governors, &name, &input, &result) {
            Some(AnswerIntervention::AnnotateResult(annotated)) => annotated,
            Some(AnswerIntervention::ReplaceResult { .. } | AnswerIntervention::RideTail(_)) => {
                unreachable!("only an annotation issues after execution")
            }
            None => raw_content,
        };

    maybe_store_plan(state, &name, &input, is_error);

    state.emitter.emit(Event::tool_result(
        id.clone(),
        name.clone(),
        content.clone(),
        is_error,
        artifacts,
    ));

    ContentBlock::tool_result(id, content, is_error)
}

// A successful plan Tool Call updates the Plan value and stores its content
// through the set_plan Dep; the Loop's copy keeps this Turn's Anchors current.
fn maybe_store_plan<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    if let Update::Updated(plan) = state.plan.update(name, input, is_error) {
        state
            .deps
            .set_plan(plan.content.clone().unwrap_or_default());
        state.plan = plan;
    }
}

pub(super) fn display_input(input: &Value) -> Value {
    if input.get(MALFORMED_INPUT_SENTINEL).is_some() {
        Value::Object(Default::default())
    } else {
        input.clone()
    }
}

// The Plugin lifecycle (ADR-0007): the LLM layer tags malformed inputs — never
// run those (mechanics, not a Governor's judgment). Otherwise the answering
// arbiter judges what the model SENT (a replaced Tool Result skips execution),
// then pre_run, Approval on the plugin-adjusted command, and execution with
// post_run and Shaping.
async fn run_block<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> (String, bool, std::collections::HashMap<String, Value>) {
    if let Some(raw) = input.get(MALFORMED_INPUT_SENTINEL) {
        let raw_str = raw.as_str().unwrap_or("");
        return (voice::malformed_input(raw_str), true, Default::default());
    }

    match governor::answer_sent(&state.governors, name, input) {
        Some(AnswerIntervention::ReplaceResult { content, is_error }) => {
            return (content, is_error, Default::default());
        }
        Some(AnswerIntervention::AnnotateResult(_) | AnswerIntervention::RideTail(_)) => {
            unreachable!("only a replacement Tool Result issues before execution")
        }
        None => {}
    }

    run_lifecycle(state, name, input).await
}

async fn run_lifecycle<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> (String, bool, std::collections::HashMap<String, Value>) {
    let token = Token::new(name, input.clone(), state.tool_ctx.clone());
    let (token, failures) = plugins::pre_run(state.plugins, token);
    emit_plugin_errors(state, &failures);

    if token.halted {
        let reason = token.halt_reason.clone().unwrap_or_default();
        return (reason, true, token.artifacts.clone());
    }

    if tools::requires_approval(name) {
        // The string the modal shows (the command, or web_fetch's URL) —
        // extracted from the plugin-adjusted input, as before.
        let text = tools::approval_text(name, &token.input).unwrap_or_default();
        let id = new_ref();
        if state.deps.request_approval(id, text).await {
            execute_token(state, token).await
        } else {
            (
                voice::command_denied().to_string(),
                true,
                Default::default(),
            )
        }
    } else {
        execute_token(state, token).await
    }
}

async fn execute_token<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    token: Token,
) -> (String, bool, std::collections::HashMap<String, Value>) {
    let (result, failures) = plugins::execute(state.plugins, token).await;
    emit_plugin_errors(state, &failures);
    (result.content, result.is_error, result.artifacts)
}

fn emit_plugin_errors<D: TurnDeps>(state: &mut LoopState<'_, D>, failures: &[plugins::Failure]) {
    for failure in failures {
        state.emitter.emit(Event::plugin_error(
            failure.plugin.clone(),
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
