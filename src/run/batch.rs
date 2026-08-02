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
use crate::run::hooks;
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
// malformed inputs - never run those. Otherwise the hook-fire seam (Phase 3a,
// ADR-0066) wraps the Approval + execution: PreToolUse may block the call or feed
// a permission decision, PermissionRequest composes with the Approval, and
// Post{ToolUse,ToolUseFailure} enrich the result. A tool shapes its own output
// and attaches its own display Artifacts; the hooks only decide/enrich around it.
async fn run_block<D: RunDeps>(state: &mut LoopState<'_, D>, name: &str, input: &Value) -> Answer {
    if let Some(raw) = malformed_tool_input(input) {
        return Answer::malformed(raw);
    }

    // The PreToolUse seam (ADR-0066): fire before Approval/execution. A blocking
    // hook (block/deny, or a prompt hook's ok:false) stops the call - the tool
    // does NOT run and the hook's reason is fed to the model as the result (a
    // Ran-with-blocked outcome, not a crash). Otherwise the call proceeds carrying
    // the hook's permission decision, injected context, and any stop request.
    let pre = fire_pre_tool_use(state, name, input).await;
    let (pre_permission, pre_context) = match pre {
        PreOutcome::Block(answer) => return answer,
        PreOutcome::Proceed {
            permission,
            context,
        } => (permission, context),
    };

    let answer = gated_execute(state, name, input, pre_permission).await;

    // The Post seam (ADR-0066): a successful result runs PostToolUse (context +
    // stop); a failed result runs PostToolUseFailure (context only). The injected
    // context is appended to the result the model reads, PreToolUse's context
    // first so a guard's note precedes an audit's note.
    post_process(state, name, answer, pre_context).await
}

// The Approval seam with the hook permission composition (ADR-0066, ADR-0050
// revised). Only the gated Tools reach it (`approvals::gate_text`); for those,
// the PermissionRequest hook composes with the mode: `allow` auto-approves with
// no modal, `deny` rejects outright (overriding even Yolo) with the hook reason,
// `ask`/no-decision falls through to the normal `request_approval` gate. The
// PreToolUse permission decision joins the composition with the same precedence.
async fn gated_execute<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    pre_permission: Option<crate::hooks::PermissionDecision>,
) -> Answer {
    let Some(text) = approvals::gate_text(name, input) else {
        // An ungated Tool: a PreToolUse `deny` still applies (it was handled as a
        // Block above), and a bare `allow` is moot (there is no gate to open).
        return execute_tool_call(state, name, input).await;
    };

    // Compose the hook verdict with the mode. `None` hooks handle => Ask (the
    // normal gate); a fired hook may Allow / Deny / Ask.
    let verdict = match state.hooks {
        Some(hooks) => hooks.permission_request(name, input, pre_permission).await,
        None => match pre_permission {
            // With no PermissionRequest hooks, a PreToolUse permission decision
            // still composes (ADR-0050 revised): allow auto-approves, deny rejects.
            Some(crate::hooks::PermissionDecision::Allow) => hooks::PermissionVerdict::Allow,
            Some(crate::hooks::PermissionDecision::Deny) => hooks::PermissionVerdict::Deny {
                reason: voice::Marker::CommandDenied.text().to_string(),
            },
            _ => hooks::PermissionVerdict::Ask,
        },
    };

    match verdict {
        hooks::PermissionVerdict::Allow => {
            emit_hook_decision(state, "PermissionRequest", "auto-approved a Tool Call");
            execute_tool_call(state, name, input).await
        }
        hooks::PermissionVerdict::Deny { reason } => {
            emit_hook_decision(
                state,
                "PermissionRequest",
                &format!("denied a Tool Call: {reason}"),
            );
            Answer::text(reason, true)
        }
        hooks::PermissionVerdict::Ask => {
            let id = new_ref();
            if state.deps.request_approval(id, text).await {
                execute_tool_call(state, name, input).await
            } else {
                Answer::denied()
            }
        }
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

// The PreToolUse fold from the Run's view (ADR-0066): either the call is blocked
// (the tool must not run; the Answer carries the hook's reason) or it proceeds
// carrying the permission decision + injected context the later seams honor.
enum PreOutcome {
    Block(Answer),
    Proceed {
        permission: Option<crate::hooks::PermissionDecision>,
        context: Option<String>,
    },
}

// Fires the PreToolUse hooks (ADR-0066). With no hooks wired the call proceeds
// unchanged. A blocking outcome becomes a Ran-with-blocked Answer (the reason,
// plus any additionalContext the blocking hook still carried, fed to the model as
// an error result - the tool never ran). A `continue:false` records the minimal
// Stop on the LoopState. Every deciding fire is surfaced visibly (ADR-0018).
async fn fire_pre_tool_use<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> PreOutcome {
    let Some(hooks) = state.hooks else {
        return PreOutcome::Proceed {
            permission: None,
            context: None,
        };
    };

    match hooks.pre_tool_use(name, input).await {
        hooks::PreToolDecision::Block { reason, context } => {
            emit_hook_decision(state, "PreToolUse", &format!("blocked a Tool Call: {reason}"));
            // The blocked call reads as an error result: the reason, with any
            // additionalContext the blocking hook still injected appended.
            let content = append_context(reason, context);
            PreOutcome::Block(Answer::text(content, true))
        }
        hooks::PreToolDecision::Proceed {
            permission,
            context,
            stop,
        } => {
            if let Some(reason) = stop {
                emit_hook_decision(state, "PreToolUse", &format!("requested stop: {reason}"));
                state.hook_stop.get_or_insert(reason);
            }
            if context.is_some() {
                emit_hook_decision(state, "PreToolUse", "injected additional context");
            }
            PreOutcome::Proceed {
                permission,
                context,
            }
        }
    }
}

// The Post seam (ADR-0066): fire PostToolUse on a success (context + stop) or
// PostToolUseFailure on an error (context only), then append the collected
// additionalContext (PreToolUse's first, then Post's) to the result the model
// reads. A `continue:false` from PostToolUse records the minimal Stop. No hooks
// wired => the Answer passes through with only the PreToolUse context appended.
async fn post_process<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    answer: Answer,
    pre_context: Option<String>,
) -> Answer {
    let output = result_blocks_text(&answer.content);
    let post_context = match state.hooks {
        Some(hooks) if !answer.is_error => {
            let decision = hooks.post_tool_use(name, &output).await;
            if let Some(reason) = decision.stop {
                emit_hook_decision(state, "PostToolUse", &format!("requested stop: {reason}"));
                state.hook_stop.get_or_insert(reason);
            }
            decision.context
        }
        Some(hooks) => hooks.post_tool_use_failure(name, &output).await,
        None => None,
    };

    if post_context.is_some() {
        let event = if answer.is_error {
            "PostToolUseFailure"
        } else {
            "PostToolUse"
        };
        emit_hook_decision(state, event, "injected additional context");
    }

    // Nothing to append: the Answer is unchanged.
    if pre_context.is_none() && post_context.is_none() {
        return answer;
    }

    // Append the injected context as a trailing text block on the result the model
    // reads (PreToolUse's note first, then Post's), so a hook can hand the model
    // lint output or a policy note. The is_error flag and Artifacts are untouched.
    let extra = [pre_context, post_context]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let mut content = answer.content;
    content.push(ResultBlock::text(extra));
    Answer {
        content,
        ..answer
    }
}

// Appends the optional hook `additionalContext` to a reason string as a trailing
// paragraph (the blocked-call answer). No context leaves the reason unchanged.
fn append_context(reason: String, context: Option<String>) -> String {
    match context {
        Some(ctx) => format!("{reason}\n{ctx}"),
        None => reason,
    }
}

// Surfaces a deciding hook fire as a visible line (ADR-0018 fail-open-with-
// visibility, ADR-0066): a block / auto-approve / deny / stop / inject is never
// silent. Reuses the fail-open report seam skills/MCP use (an `extension_error`
// with a `hook <event>` label + the `Present` mid-Run stage), so the operator
// reads what a hook did on the same channel a launch notice takes.
fn emit_hook_decision<D: RunDeps>(state: &mut LoopState<'_, D>, event: &str, what: &str) {
    state.emitter.emit(Event::extension_error(
        format!("hook {event}"),
        crate::event::Stage::Present,
        what.to_string(),
    ));
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
