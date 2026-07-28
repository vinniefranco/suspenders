//! Tool Call batch - executing one Pass's Tool Calls (carved from the Run
//! Loop port of baud's `Baud.Turn.Loop`; "batch" is the domain word: Steering
//! is delivered after a Tool Call batch completes, and ADR-0009's truncated
//! response means none of its batch executes).
//!
//! [`execute_tools`] runs a Pass's Tool Calls in emission order. Each call goes
//! through the gates in sequence: the malformed-input sentinel (the LLM layer
//! tags inputs that never parsed - those are answered, never run), the
//! offered-set check (a call this Pass's request did not offer is refused,
//! never run - ADR-0035), the Tool
//! Call answering arbiter before execution ([`governor::answer_sent`],
//! ADR-0026: an identical repeat draws a replacement Tool Result instead of a
//! rerun), then the Extension lifecycle (ADR-0007: pre_run - which may halt the
//! call with the Extension's own wording - execution, post_run/Shaping), with
//! Approval (ADR-0005) requested between pre_run and execution for the tools
//! that require it, on the Middleware-adjusted input; the arbiter is consulted
//! again after execution ([`governor::answer_read`] - the consecutive-failure
//! annotation). Once the batch finishes the Conversation is checkpointed with
//! only the answered Tool Calls, so the checkpoint never persists an unanswered
//! tool_use block.
//!
//! The loop skeleton lives in [`super::loop_`]; how a Run ends when the model
//! stops calling tools lives in [`super::finish`].

use std::collections::HashMap;

use serde_json::Value;

use crate::approvals;
use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::extensions;
use crate::llm::malformed_tool_input;
use crate::middleware::Token;
use crate::plan::Update;
use crate::run::deps::RunDeps;
use crate::run::governor::ledger::{CallOutcome, ToolResult};
use crate::run::governor::{self, AnswerIntervention};
use crate::run::loop_::LoopState;
use crate::voice;

// Run tool calls in emission order; results keep that order. Checkpoint ONCE
// after the batch, carrying every answered Tool Call. After the batch the
// duplicate memory advances and the Ledger's failure-recency clock ticks.
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
    state.governors.next_pass();
    state.ledger.close_batch();
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

    // The Answer's facts go on the Ledger first, written once at this firing
    // site (ADR-0026) - replaced results included: a replaced duplicate still
    // counts toward the failure tally, and a duplicated write/run_command
    // still moves the verify state. The batch states the typed fact of
    // whether the call ran; the Ledger's one recording method owns the
    // routing (a denial and an offered-set refusal move fewer facts), so the
    // Ledger never sniffs Voice wording. Then the answering moment's second
    // consultation: the arbiter judges what the model will READ, possibly
    // annotating it with the consecutive-failure suffix.
    let result = answer.result();
    state.ledger.record(&name, &input, &result, answer.outcome);
    let intervention =
        governor::answer_read(&state.ledger, &mut state.governors, &name, &input, &result);
    let is_error = answer.is_error;
    let content = match intervention {
        Some(AnswerIntervention::AnnotateResult(annotated)) => annotated,
        Some(AnswerIntervention::ReplaceResult { .. } | AnswerIntervention::RideTail(_)) => {
            unreachable!("only an annotation issues after execution")
        }
        None => answer.content,
    };

    maybe_store_plan(state, &name, &input, is_error);

    state.emitter.emit(Event::tool_result(
        id.clone(),
        name.clone(),
        content.clone(),
        is_error,
        answer.artifacts,
    ));

    ContentBlock::tool_result(id, content, is_error)
}

// A successful plan Tool Call updates the Plan value and stores its content
// through the set_plan Dep; the Loop's copy keeps this Run's Anchors current,
// and the Ledger's plan-recency fact resets at the same firing site (written
// once as it happens - ADR-0026). The updated Plan's checkbox facts (ADR-0043:
// the Open Plan) go to the Ledger too - computed HERE where the Plan is in
// scope, so the Ledger stores facts and never parses markdown.
fn maybe_store_plan<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    if let Update::Updated(plan) = state.plan.update(name, input, is_error) {
        state
            .deps
            .set_plan(plan.content.clone().unwrap_or_default());
        let progress = plan.progress();
        let checked = plan.checked_steps();
        state.plan = plan;
        state.ledger.note_plan_updated(progress, checked);
    }
}

pub(super) fn display_input(input: &Value) -> Value {
    if malformed_tool_input(input).is_some() {
        Value::Object(Default::default())
    } else {
        input.clone()
    }
}

// How the batch answered one Tool Call (see CONTEXT.md: Answer): the Tool
// Result the model will read plus the typed fact of whether the call ran.
// Built only through the constructors below, each pairing the Voice's
// wording with its [`CallOutcome`] so the two cannot drift; the Ledger
// records an Answer through its one method and owns what each fact moves.
struct Answer {
    content: String,
    is_error: bool,
    artifacts: HashMap<String, Value>,
    outcome: CallOutcome,
}

impl Answer {
    /// A malformed-input answer is recorded like any error - it reads as a
    /// run.
    fn malformed(raw: &str) -> Self {
        Answer {
            content: voice::malformed_input(raw),
            is_error: true,
            artifacts: Default::default(),
            outcome: CallOutcome::Ran,
        }
    }

    /// An offered-set refusal (ADR-0035): the Pass did not offer the Tool,
    /// so the call never ran.
    fn refused(name: &str) -> Self {
        Answer {
            content: voice::tool_not_offered(name),
            is_error: true,
            artifacts: Default::default(),
            outcome: CallOutcome::Refused,
        }
    }

    /// A Governor's replaced result still reads as a run (ADR-0026).
    fn replaced(content: String, is_error: bool) -> Self {
        Answer {
            content,
            is_error,
            artifacts: Default::default(),
            outcome: CallOutcome::Ran,
        }
    }

    /// A Middleware halt reads as a failed run.
    fn halted(reason: String, artifacts: HashMap<String, Value>) -> Self {
        Answer {
            content: reason,
            is_error: true,
            artifacts,
            outcome: CallOutcome::Ran,
        }
    }

    /// An Approval denial (ADR-0005): the command never ran.
    fn denied() -> Self {
        Answer {
            content: voice::command_denied().to_string(),
            is_error: true,
            artifacts: Default::default(),
            outcome: CallOutcome::Denied,
        }
    }

    /// The Extension pipeline executed the call.
    fn ran(result: extensions::PipelineResult) -> Self {
        Answer {
            content: result.content,
            is_error: result.is_error,
            artifacts: result.artifacts,
            outcome: CallOutcome::Ran,
        }
    }

    /// The Tool Result the model will read, borrowed for the Ledger and the
    /// arbiter.
    fn result(&self) -> ToolResult<'_> {
        ToolResult {
            content: &self.content,
            is_error: self.is_error,
        }
    }
}

// The Extension lifecycle (ADR-0007): the LLM layer tags malformed inputs -
// never run those (mechanics, not a Governor's judgment). Otherwise the
// answering arbiter judges what the model SENT (a replaced Tool Result skips
// execution), then pre_run, Approval on the Middleware-adjusted command, and
// execution with post_run and Shaping.
async fn run_block<D: RunDeps>(state: &mut LoopState<'_, D>, name: &str, input: &Value) -> Answer {
    if let Some(raw) = malformed_tool_input(input) {
        return Answer::malformed(raw);
    }

    // The Offer is enforced at dispatch (ADR-0035): a call naming a Tool
    // the Pass's Offer does not name is answered with the Voice's refusal
    // and never runs - mechanics, not a Governor's judgment, at the same
    // seam as the malformed-input sentinel. This is what makes the
    // Verification Pass's run_command-only narrowing and the final Pass's
    // no-Tools withdrawal real for a model that insists anyway.
    if !state.offer.offers(name) {
        return Answer::refused(name);
    }

    match governor::answer_sent(&state.governors, name, input) {
        Some(AnswerIntervention::ReplaceResult { content, is_error }) => {
            return Answer::replaced(content, is_error);
        }
        Some(AnswerIntervention::AnnotateResult(_) | AnswerIntervention::RideTail(_)) => {
            unreachable!("only a replacement Tool Result issues before execution")
        }
        None => {}
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

    // The Answer constructors fuse the Voice's wording with the ran-fact so
    // the pairing cannot drift (CONTEXT.md: Answer).

    #[test]
    fn a_refusal_pairs_the_not_offered_voice_with_the_refused_fact() {
        // ADR-0035: the Pass did not offer the Tool; the call never ran.
        let answer = Answer::refused("write_file");
        assert_eq!(answer.content, voice::tool_not_offered("write_file"));
        assert!(answer.is_error);
        assert!(matches!(answer.outcome, CallOutcome::Refused));
        assert!(answer.artifacts.is_empty());
    }

    #[test]
    fn a_denial_pairs_the_command_denied_voice_with_the_denied_fact() {
        // ADR-0005: the Approval gate; the command never ran.
        let answer = Answer::denied();
        assert_eq!(answer.content, voice::command_denied());
        assert!(answer.is_error);
        assert!(matches!(answer.outcome, CallOutcome::Denied));
    }

    #[test]
    fn a_malformed_input_answer_reads_as_a_run() {
        // Recorded like any error - the Ledger sees a failed run.
        let answer = Answer::malformed("{not json");
        assert_eq!(answer.content, voice::malformed_input("{not json"));
        assert!(answer.is_error);
        assert!(matches!(answer.outcome, CallOutcome::Ran));
    }

    #[test]
    fn an_extension_halt_reads_as_a_failed_run() {
        let answer = Answer::halted("blocked by plugin".to_string(), Default::default());
        assert_eq!(answer.content, "blocked by plugin");
        assert!(answer.is_error);
        assert!(matches!(answer.outcome, CallOutcome::Ran));
    }

    #[test]
    fn the_result_view_borrows_the_answers_content_and_flag() {
        // What the Ledger records and the arbiter reads is exactly the Tool
        // Result the model will read.
        let answer = Answer::replaced("cached result".to_string(), false);
        let result = answer.result();
        assert_eq!(result.content, "cached result");
        assert!(!result.is_error);
        assert!(matches!(answer.outcome, CallOutcome::Ran));
    }
}
