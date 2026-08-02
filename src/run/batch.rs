//! Tool Call batch - executing one Pass's Tool Calls (carved from the Run
//! Loop; "batch" is the domain word: Steering is delivered after a Tool Call
//! batch completes, and ADR-0009's truncated response means none of its batch
//! executes).
//!
//! [`execute_tools`] runs a Pass's Tool Calls in emission order. Each call goes
//! through the gates in sequence: the malformed-input sentinel (the LLM layer
//! tags inputs that never parsed - those are answered, never run), then Approval
//! (ADR-0005) on the raw tool input for the tools that require it, then execution
//! with Shaping (the Result Cap). A tool shapes its own model-facing output and
//! attaches any display Artifacts (ADR-0007: the diff / todos / exit-code badge
//! live in the tools now, not a wrapper pipeline). Once the batch finishes the
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
use crate::llm::malformed_tool_input;
use crate::plan::Update;
use crate::run::deps::RunDeps;
use crate::run::loop_::LoopState;
use crate::tools;
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

// Executes one Tool Call. The caller filters to tool_use blocks, so the guard
// destructures the call and a non-tool_use block (which the filter never yields)
// answers a benign error rather than panicking - no unreachable in the run path.
async fn execute_tool<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    block: &ContentBlock,
) -> ContentBlock {
    let ContentBlock::ToolUse { id, name, input } = block else {
        return ContentBlock::tool_result(
            String::new(),
            voice::malformed_input("not a tool call"),
            true,
        );
    };
    let (id, name, input) = (id.clone(), name.clone(), input.clone());

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

// How the batch answered one Tool Call: the Tool Result the model will read,
// whether it was an error, and the display Artifacts the tool attached. Built
// only through the constructors below.
struct Answer {
    content: Vec<ResultBlock>,
    is_error: bool,
    artifacts: HashMap<String, Value>,
}

impl Answer {
    /// A malformed-input answer is recorded like any error - it reads as a
    /// run.
    fn malformed(raw: &str) -> Self {
        Answer::text(voice::malformed_input(raw), true)
    }

    /// An Approval denial (ADR-0005): the command never ran.
    fn denied() -> Self {
        Answer::text(voice::Marker::CommandDenied.text(), true)
    }

    /// The tool executed and was Shaped (the Result Cap): the block list, the
    /// error flag, and the tool's display Artifacts ride straight through
    /// (ADR-0059).
    fn ran(result: tools::ToolResult) -> Self {
        Answer {
            content: result.content,
            is_error: result.is_error,
            artifacts: result.artifacts,
        }
    }

    /// A single-Text-block answer with no Artifacts - the shape every
    /// Voice-worded outcome (malformed / denied) takes.
    fn text(content: impl Into<String>, is_error: bool) -> Self {
        Answer {
            content: vec![ResultBlock::text(content)],
            is_error,
            artifacts: HashMap::new(),
        }
    }
}

// The Tool Call lifecycle (ADR-0007, pipeline retired): the LLM layer tags
// malformed inputs - never run those. Otherwise Approval on the raw tool input,
// then execution with Shaping. There is no input-adjust and no halt - a tool
// shapes its own output and attaches its own display Artifacts.
async fn run_block<D: RunDeps>(state: &mut LoopState<'_, D>, name: &str, input: &Value) -> Answer {
    if let Some(raw) = malformed_tool_input(input) {
        return Answer::malformed(raw);
    }

    // The one Approval seam: Some(text) gates and text is exactly what the user
    // reads (the command, or web_fetch's URL), read from the raw tool input;
    // None means no gate (approvals::gate_text).
    match approvals::gate_text(name, input) {
        Some(text) => {
            let id = new_ref();
            if state.deps.request_approval(id, text).await {
                execute_tool_call(state, name, input).await
            } else {
                Answer::denied()
            }
        }
        None => execute_tool_call(state, name, input).await,
    }
}

// Runs the named tool with Shaping (the Result Cap) - the extension-free dispatch
// path (`tools::run`). A tool can never crash the Run: an unknown name or an
// `Err` return both come back as an `is_error` result (ADR-0018 fail-open).
async fn execute_tool_call<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> Answer {
    Answer::ran(tools::run(name, input, state.tool_ctx).await)
}

// The per-call Approval reference (baud's `make_ref()`), an opaque unique id.
fn new_ref() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("approval-{n}")
}

#[cfg(test)]
#[path = "../../tests/run/batch.rs"]
mod tests;
