//! Tool Call batch - executing one Pass's Tool Calls (carved from the Run
//! Loop; "batch" is the domain word: Steering is delivered after a Tool Call
//! batch completes, and ADR-0009's truncated response means none of its batch
//! executes).
//!
//! [`execute_tools`] runs a Pass's Tool Calls in emission order. Each call goes
//! through the gates in sequence: the malformed-input sentinel (the LLM layer
//! tags inputs that never parsed - those are answered, never run), then the one
//! mode-aware verdict fold ([`crate::approvals::Approvals::classify`], ADR-0067)
//! over EVERY call - which Allows, opens an Approval (ADR-0005), or Blocks
//! (plan mode) - with the Hook permission composition (ADR-0066) ahead of it,
//! then execution with Shaping (the Result Cap). A tool shapes its own model-facing output and
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

    maybe_store_plan(state, &name, &input, is_error).await;
    maybe_activate_skills(state, &name, &input, is_error);
    maybe_register_skill_hooks(state, &name, &input, is_error);

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

// Conditional-skill activation at the tool-success seam (ADR-0058, qwen's
// `matchAndActivateByPath`). A SUCCESSFUL Tool Call that names a file path
// activates any conditional (`paths:`) skill whose globs match that path,
// resolved relative to the Project Root; the skill then appears in the `skill`
// tool's `<available_skills>` catalog on its next description read (the manager
// is shared, so no change-listener). Checking SUCCESS (not attempt) means a
// failed touch never leaks a hidden skill into the catalog. A no-activation Run
// (no `skill_activation` threaded, an errored call, or a tool that touches no
// file) is a cheap no-op.
fn maybe_activate_skills<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    if is_error {
        return;
    }
    let Some(activation) = state.skill_activation.as_ref() else {
        return;
    };
    for path in touched_paths(name, input) {
        activation
            .skills
            .activate_by_path(std::path::Path::new(path), &activation.project_root);
    }
}

// The file path(s) a Tool Call named in its input (ADR-0058). Reads the standard
// path keys the file-touching tools carry - `file_path` (read_file/edit/
// write_file), `notebook_path` (notebook_edit), and `path` (list_directory/glob/
// grep_search, a search-root that can still match a `src/**`-style skill glob) -
// so activation keys off the same signal the tool result carries. A tool that
// names no path (or names it under an unrecognized key) yields nothing.
fn touched_paths<'a>(_name: &str, input: &'a Value) -> Vec<&'a str> {
    const PATH_KEYS: [&str; 3] = ["file_path", "notebook_path", "path"];
    PATH_KEYS
        .iter()
        .filter_map(|key| input.get(*key).and_then(Value::as_str))
        .filter(|p| !p.is_empty())
        .collect()
}

// Skill-hook registration at the tool-success seam (Phase 4c, ADR-0066): a
// SUCCESSFUL `skill` tool call - the MODEL invoking a skill - registers that
// skill's frontmatter `hooks:` as SESSION-scoped hooks in the live HookManager,
// so they fire at their events for the rest of the Run (qwen semantics:
// model-invocation only, session-scoped, carrying SUSPENDERS_SKILL_ROOT). The
// skill is resolved by the `skill` input arg through the shared
// `Arc<SkillManager>` (the same handle `maybe_activate_skills` uses), and its
// `base_dir` becomes the hook skill_root so a registered command hook sees
// SUSPENDERS_SKILL_ROOT when the Phase 2 runner fires it. Registration is
// idempotent (a skill invoked twice registers once). This keeps the `skill` tool
// hook-free: the RUN layer detects the invocation and does the registration,
// mirroring the todo/subagent hook wiring (ADR-0066's layering preference). A Run
// with no hooks handle, no skill_activation, an errored call, or a non-`skill`
// tool is a cheap no-op. The user `/<name>` slash path (Phase 4b) does NOT reach
// here - it never emits a `skill` tool call - so only the model path registers
// hooks, faithful to qwen.
fn maybe_register_skill_hooks<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    if is_error || name != "skill" {
        return;
    }
    let (Some(hooks), Some(activation)) = (state.hooks, state.skill_activation.as_ref()) else {
        return;
    };
    let Some(skill_name) = input.get("skill").and_then(Value::as_str).map(str::trim) else {
        return;
    };
    let Some(skill) = activation.skills.find(skill_name) else {
        return;
    };
    // Only a skill that actually carries a parsed `hooks:` block registers
    // anything; a skill with no hooks is a no-op (registers nothing).
    if let Some(skill_hooks) = &skill.hooks {
        hooks.register_skill(&skill.name, &skill.base_dir.to_string_lossy(), skill_hooks);
    }
}

// A successful todo_write Tool Call replaces the Plan's task list and stores its
// rendered form through the set_plan Dep; the Loop keeps this Run's copy. The
// TodoCreated / TodoCompleted hooks (Phase 3b, ADR-0066) fire here at the RUN
// layer: the Plan fold detects the created/completed transitions from the old vs.
// new todo lists, so a todo_write tool never depends on the hook subsystem
// (ADR-0066's layering preference - tools stay hook-free).
async fn maybe_store_plan<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    is_error: bool,
) {
    let Update::Updated(plan) = state.plan.update(name, input, is_error) else {
        return;
    };
    fire_todo_hooks(state, &plan).await;
    state.deps.set_plan(plan.render());
    state.plan = plan;
}

// Fires TodoCreated / TodoCompleted for the transitions between the Run's current
// Plan (`state.plan`) and the freshly-folded `new_plan` (Phase 3b, ADR-0066). A
// created todo is one whose content is new to the list; a completed todo is one
// that is Completed now and was NOT Completed before (a fresh content that lands
// Completed counts as both created and completed, matching qwen's per-item
// validation/postWrite split firing on each). Fire-and-observe: these events do
// not steer, so any outcome is ignored.
async fn fire_todo_hooks<D: RunDeps>(state: &mut LoopState<'_, D>, new_plan: &crate::plan::Plan) {
    let Some(hooks) = state.hooks else {
        return;
    };
    use crate::plan::TodoStatus;
    let was_completed = |content: &str| {
        state
            .plan
            .todos
            .iter()
            .any(|t| t.content == content && t.status == TodoStatus::Completed)
    };
    let existed = |content: &str| state.plan.todos.iter().any(|t| t.content == content);

    for todo in &new_plan.todos {
        if !existed(&todo.content) {
            hooks.todo_created(&todo.content).await;
        }
        if todo.status == TodoStatus::Completed && !was_completed(&todo.content) {
            hooks.todo_completed(&todo.content).await;
        }
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

    let answer = classified_execute(state, name, input, pre_permission).await;

    // The Post seam (ADR-0066): a successful result runs PostToolUse (context +
    // stop); a failed result runs PostToolUseFailure (context only). The injected
    // context is appended to the result the model reads, PreToolUse's context
    // first so a guard's note precedes an audit's note.
    post_process(state, name, answer, pre_context).await
}

// The one mode-aware verdict fold over EVERY Tool Call (ADR-0067), with the hook
// permission composition (ADR-0066, ADR-0050 revised) layered ahead of it - the
// two together in qwen's precedence order (coreToolScheduler.ts): a permission
// `deny` rejects first, a permission `allow` auto-approves next (bypassing the
// mode, including a plan-mode block, matching qwen's `finalPermission==='allow'`
// early continue), and only then does the mode's own `classify` decide. Every
// call folds through this now - the old gated-tools-only short-circuit is gone,
// so the mode is the single axis every call is evaluated against (ADR-0067).
//
// The precedence, in order:
//  1. Hook `deny` (PreToolUse decision or a PermissionRequest hook) -> reject the
//     call with the hook reason, overriding even Yolo/Plan (an operator guard).
//  2. Hook `allow` -> auto-approve with no modal, bypassing the mode - so a
//     PermissionRequest `allow` runs a call plan mode would otherwise block
//     (qwen bypasses plan-block on `finalPermission==='allow'`).
//  3. Otherwise `classify` (the live mode + the tool's Kind) decides:
//     - `Verdict::Block(reason)` -> the plan-mode block: return an error Answer
//       with qwen's verbatim reason and NO modal (a mutating Kind in plan mode).
//     - `Verdict::Ask(text)` -> the normal gate: round-trip `request_approval`.
//     - `Verdict::Allow` -> execute. A PreToolUse `ask` still FORCES a
//       confirmation here even on an ungated Allow (qwen `isAsk()`), so an Allow
//       with a pending `ask` synthesizes the generic confirm gate.
async fn classified_execute<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    pre_permission: Option<crate::hooks::PermissionDecision>,
) -> Answer {
    // Compose the hook permission verdict FIRST (deny/allow/ask), spanning the
    // PreToolUse decision and any PermissionRequest hook, exactly as before. This
    // now runs for EVERY tool (not just gated ones): with no PermissionRequest
    // hooks and no pre-decision it folds to `Ask` (fall through to `classify`),
    // so an ungated tool with no hooks reaches the same `classify` path a gated
    // one does.
    let verdict = compose_permission(state, name, input, pre_permission).await;

    match verdict {
        hooks::PermissionVerdict::Deny { reason } => {
            emit_hook_decision(
                state,
                "PermissionRequest",
                &format!("denied a Tool Call: {reason}"),
            );
            Answer::text(reason, true)
        }
        hooks::PermissionVerdict::Allow => {
            // A hook `allow` auto-approves the call with no modal, bypassing the
            // mode - so plan mode does not block a call an operator hook allowed
            // (qwen's `finalPermission==='allow'` continue precedes its
            // plan-mode block). Visible either way.
            emit_hook_decision(state, "PermissionRequest", "auto-approved a Tool Call");
            execute_tool_call(state, name, input).await
        }
        hooks::PermissionVerdict::Ask => classify_and_run(state, name, input, pre_permission).await,
    }
}

// The hook permission composition (deny/allow/ask), factored out of the fold so
// the mode-aware `classify` reads as one step. Spans the PreToolUse permission
// decision and any PermissionRequest hook, with `deny` overriding `allow`
// (ADR-0050 revised). With no hooks wired it folds the bare PreToolUse decision:
// allow -> Allow, deny -> Deny (generic reason), else Ask.
async fn compose_permission<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    pre_permission: Option<crate::hooks::PermissionDecision>,
) -> hooks::PermissionVerdict {
    match state.hooks {
        Some(hooks) => hooks.permission_request(name, input, pre_permission).await,
        None => match pre_permission {
            Some(crate::hooks::PermissionDecision::Allow) => hooks::PermissionVerdict::Allow,
            Some(crate::hooks::PermissionDecision::Deny) => hooks::PermissionVerdict::Deny {
                reason: voice::Marker::CommandDenied.text().to_string(),
            },
            _ => hooks::PermissionVerdict::Ask,
        },
    }
}

// The mode-aware `classify` fold + its side effects (ADR-0067), reached when the
// hook composition did not force allow/deny. Reads the LIVE Approval mode from
// the shared atomic mirror and the tool's self-declared Kind from the registry,
// then folds them into a Verdict:
//  - `Block(reason)` -> the plan-mode block, an error Answer with no modal.
//  - `Ask(text)` -> the gate: round-trip `request_approval` (the Agent applies
//    Yolo / any Standing Approval and opens the modal or auto-approves).
//  - `Allow` -> execute; a pending PreToolUse `ask` still forces a confirmation.
async fn classify_and_run<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
    pre_permission: Option<crate::hooks::PermissionDecision>,
) -> Answer {
    let mode = state.tool_ctx.caps.approval_mode.load();
    let kind = state.tool_ctx.registry().kind_of(name);

    match approvals::Approvals::with_mode(mode).classify(name, kind, input) {
        approvals::Verdict::Block(reason) => {
            // Plan mode blocked a mutating / non-read-only tool (ADR-0067): the
            // tool never runs, qwen's verbatim reason is fed to the model as an
            // error result, and NO modal opens. Surfaced visibly like a hook
            // decision so the operator reads why the call was blocked.
            emit_plan_block(state, name);
            Answer::text(reason, true)
        }
        approvals::Verdict::Ask(text) => {
            let id = new_ref();
            if state.deps.request_approval(id, text).await {
                execute_tool_call(state, name, input).await
            } else {
                Answer::denied()
            }
        }
        approvals::Verdict::Allow => {
            // A PreToolUse `ask` FORCES a confirmation even on an ungated Allow
            // (qwen `isAsk()` -> requires-confirmation regardless of gating), so
            // synthesize the generic confirm gate and let the user decide.
            if pre_permission == Some(crate::hooks::PermissionDecision::Ask) {
                emit_hook_decision(state, "PreToolUse", "requested confirmation");
                let id = new_ref();
                let text = format!("Confirm {name}");
                return if state.deps.request_approval(id, text).await {
                    execute_tool_call(state, name, input).await
                } else {
                    Answer::denied()
                };
            }
            execute_tool_call(state, name, input).await
        }
    }
}

// Runs the named tool with Shaping (the Result Cap) - the extension-free dispatch
// path (`tools::run`). A tool can never crash the Run: an unknown name or an
// `Err` return both come back as an `is_error` result (ADR-0018 fail-open). The
// `agent` tool spawns a child Run, so the SubagentStart / SubagentStop hooks
// (Phase 3b, ADR-0066) bracket its dispatch here at the PARENT run layer - the
// child's own inner Run stays hook-free (the Phase 3a choice).
async fn execute_tool_call<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    name: &str,
    input: &Value,
) -> Answer {
    let subagent_type = (name == "agent")
        .then(|| input.get("subagent_type").and_then(Value::as_str))
        .flatten();

    if let (Some(hooks), Some(kind)) = (state.hooks, subagent_type) {
        hooks.subagent_start(kind).await;
    }
    let answer = Answer::ran(tools::run(name, input, state.tool_ctx).await);
    if let (Some(hooks), Some(kind)) = (state.hooks, subagent_type) {
        hooks.subagent_stop(kind).await;
    }
    answer
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
        hooks::PreToolDecision::Block {
            reason,
            context,
            system_message,
        } => {
            emit_hook_decision(
                state,
                "PreToolUse",
                &format!("blocked a Tool Call: {reason}"),
            );
            surface_system_message(state, "PreToolUse", system_message);
            // The blocked call reads as an error result: the reason, with any
            // additionalContext the blocking hook still injected appended.
            let content = append_context(reason, context);
            PreOutcome::Block(Answer::text(content, true))
        }
        hooks::PreToolDecision::Proceed {
            permission,
            context,
            stop,
            system_message,
        } => {
            if let Some(reason) = stop {
                emit_hook_decision(state, "PreToolUse", &format!("requested stop: {reason}"));
                state.hook_stop.get_or_insert(reason);
            }
            if context.is_some() {
                emit_hook_decision(state, "PreToolUse", "injected additional context");
            }
            surface_system_message(state, "PreToolUse", system_message);
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
            surface_system_message(state, "PostToolUse", decision.system_message);
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
    Answer { content, ..answer }
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

// Surfaces a plan-mode block as a visible line (ADR-0067), on the same fail-open
// report channel the hook decisions use: an `extension_error` labelled `plan
// mode` with the `Present` mid-Run stage, so the operator reads that plan mode
// blocked a mutating tool (the model reads qwen's verbatim reason as the result;
// this is the user-facing notice).
fn emit_plan_block<D: RunDeps>(state: &mut LoopState<'_, D>, name: &str) {
    state.emitter.emit(Event::extension_error(
        "plan mode".to_string(),
        crate::event::Stage::Present,
        format!("blocked a non-read-only Tool Call: {name}"),
    ));
}

// Surfaces a hook's `systemMessage` on the same visible channel (A5, qwen's
// `processCommonHookOutputFields`): a hook can hand the USER a note (distinct from
// the model-facing additionalContext) UNLESS it set `suppressOutput` (the fold
// already applied that gate, so this only sees the messages qwen would show). `None`
// is a no-op - the common case where no hook set a systemMessage.
fn surface_system_message<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    event: &str,
    message: Option<String>,
) {
    if let Some(message) = message {
        emit_hook_decision(state, event, &format!("system message: {message}"));
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
#[path = "../../tests/run/batch.rs"]
mod tests;
