//! Integration tests for the hook firing seam (Phase 3a, ADR-0066): the four
//! tool events wired into the live run loop via `batch`, exercised through the
//! injected fake capabilities (a fake ShellExec returning crafted JSON outcomes)
//! and the real `HookManager`. Each test drives a full Run so the decision folds
//! (PreToolUse block / permission composition / Post context / stop / fail-open)
//! are proven end to end, not in isolation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

use crate::content::{ContentBlock, Usage};
use crate::event::Event;
use crate::hooks::{HttpPost, ShellExec, ShellResult};
use crate::llm::response::{Response, StopReason};
use crate::run::Outcome;
use crate::run::fixtures::{
    deps_for, events, just, run_with_hooks, run_with_hooks_and_skills, session, text_end,
};
use crate::run::hooks::Hooks;
use crate::skills::SkillManager;
use crate::test_support::{Entry, FakeLlm};

// ---- fakes -------------------------------------------------------------------

/// A fake ShellExec that answers every command-hook run with the SAME crafted
/// stdout JSON (a multi-call fake, unlike the single-shot Phase 2 unit fake), and
/// counts how many times it was called so a test can assert a hook did / did not
/// fire. Exit 0 so the stdout JSON is parsed straight into the outcome.
struct ScriptedShell {
    stdout: String,
    calls: Mutex<usize>,
}

impl ScriptedShell {
    fn new(stdout: &str) -> Self {
        ScriptedShell {
            stdout: stdout.to_string(),
            calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl ShellExec for ScriptedShell {
    async fn run(
        &self,
        _command: &str,
        _stdin_json: &str,
        _cwd: &str,
        _env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        *self.calls.lock().unwrap() += 1;
        Ok(ShellResult {
            exit_code: 0,
            stdout: self.stdout.clone(),
            stderr: String::new(),
        })
    }
}

/// A fake ShellExec that always errs (a spawn failure / timeout): the fail-open
/// path (the runner turns this into the steers-nothing default outcome).
struct ErringShell {
    calls: Mutex<usize>,
}

impl ErringShell {
    fn new() -> Self {
        ErringShell {
            calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl ShellExec for ErringShell {
    async fn run(
        &self,
        _command: &str,
        _stdin_json: &str,
        _cwd: &str,
        _env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        *self.calls.lock().unwrap() += 1;
        Err("boom: could not spawn hook".to_string())
    }
}

/// A ShellExec wrapping a shared [`ScriptedShell`], so a test can keep the call
/// counter while the `Hooks` handle owns a boxed ShellExec (the fire-count fakes
/// share ONE counter across a Run).
struct SharedShell(std::sync::Arc<ScriptedShell>);

#[async_trait]
impl ShellExec for SharedShell {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        self.0.run(command, stdin_json, cwd, env, timeout_secs).await
    }
}

/// A never-called HttpPost (the tests exercise the command path only).
struct UnusedHttp;
#[async_trait]
impl HttpPost for UnusedHttp {
    async fn post(&self, _u: &str, _b: &str, _t: u64) -> Result<(u16, String), String> {
        panic!("http should not be called");
    }
}

// ---- helpers -----------------------------------------------------------------

/// A `HookManager` from a `config.json`-shaped `hooks` block that fires a single
/// `command` hook on `event` for every tool (no matcher). The command hook routes
/// through the injected ShellExec, so a test's ScriptedShell crafts the outcome.
fn manager_for(event: &str) -> crate::hooks::HookManager {
    let hooks = json!({
        event: [ { "hooks": [ { "type": "command", "command": "guard.sh" } ] } ]
    });
    crate::hooks::HookManager::from_config(Some(&hooks))
}

/// A tool-use Pass calling `list_directory` on `path` (a tool that succeeds on an
/// absolute path in the Project Root - the clean success path for a PostToolUse
/// test) or `run_shell_command` (the gated path for the permission tests).
fn tool_pass(id: &str, name: &str, input: serde_json::Value) -> Response {
    Response {
        content: vec![ContentBlock::tool_use(id, name, input)],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    }
}

/// The ToolResult event for `id`, or panic. The visible result the model reads.
fn tool_result<'a>(evs: &'a [Event], id: &str) -> &'a Event {
    evs.iter()
        .find(|e| matches!(e, Event::ToolResult { id: i, .. } if i == id))
        .expect("tool result present")
}

/// Whether a hook-decision line was surfaced for `event` (the fail-open-with-
/// visibility seam, ADR-0018): an `ExtensionError` labelled `hook <event>`.
fn hook_line_for(evs: &[Event], event: &str) -> Option<String> {
    evs.iter().find_map(|e| match e {
        Event::ExtensionError {
            extension, message, ..
        } if extension == &format!("hook {event}") => Some(message.clone()),
        _ => None,
    })
}

// ---- PreToolUse --------------------------------------------------------------

/// A PreToolUse `deny` blocks the call: the tool does NOT run and the model reads
/// the hook's reason as the (error) result. A visible block line is surfaced.
#[tokio::test]
async fn pre_tool_use_block_prevents_execution_and_returns_reason() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ScriptedShell::new(r#"{"decision":"deny","reason":"blocked by policy"}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // The model calls a tool that WOULD succeed; the hook must stop it.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(*is_error, "a blocked call reads as an error result");
    assert!(
        content.contains("blocked by policy"),
        "the hook reason reaches the model: {content}"
    );
    // The block is visible, never a silent veto (ADR-0018).
    let line = hook_line_for(&evs, "PreToolUse").expect("a block line was surfaced");
    assert!(line.contains("blocked"), "{line}");
}

/// A PreToolUse allow with additionalContext lets the tool run AND appends the
/// injected context to the result the model reads (ADR-0066).
#[tokio::test]
async fn pre_tool_use_allow_injects_additional_context() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ScriptedShell::new(
        r#"{"decision":"allow","hookSpecificOutput":{"additionalContext":"lint: clean"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(!*is_error, "the tool ran (allow, not block)");
    assert!(
        content.contains("lint: clean"),
        "the injected context is appended to the result: {content}"
    );
}

// ---- PermissionRequest -------------------------------------------------------

/// A PermissionRequest `allow` auto-approves a gated call with NO modal: no
/// ApprovalRequest event is emitted and the command runs (ADR-0050 revised).
#[tokio::test]
async fn permission_request_allow_auto_approves_without_a_modal() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PermissionRequest");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // run_shell_command GATES (ADR-0005); the hook auto-approves it. No canned
    // approvals are seeded, so a fall-through gate would DENY - proving the hook
    // (not the queue) approved.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "g1",
                "run_shell_command",
                json!({ "command": "echo hi" }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::ApprovalRequest { .. })),
        "an auto-approve opens NO modal"
    );
    let Event::ToolResult { content, .. } = tool_result(&evs, "g1") else {
        unreachable!()
    };
    assert!(
        content.contains("hi"),
        "the auto-approved command ran: {content}"
    );
    assert!(
        hook_line_for(&evs, "PermissionRequest")
            .map(|l| l.contains("auto-approved"))
            .unwrap_or(false),
        "the auto-approve is visible"
    );
}

/// A PermissionRequest `deny` rejects a gated call outright with the hook reason
/// and never opens the gate (ADR-0050 revised).
#[tokio::test]
async fn permission_request_deny_rejects_with_reason() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PermissionRequest");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no shell here"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // Seed an APPROVE in the canned queue: the hook deny must override it (no
    // modal, no run), proving deny beats a would-be approval.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "g1",
                "run_shell_command",
                json!({ "command": "echo hi" }),
            )),
            just(text_end("done")),
        ],
    )
    .with_approvals(vec![true]);
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::ApprovalRequest { .. })),
        "a hook deny opens NO modal"
    );
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "g1")
    else {
        unreachable!()
    };
    assert!(*is_error, "the denied call reads as an error");
    assert!(
        content.contains("no shell here"),
        "the deny reason reaches the model: {content}"
    );
    assert!(
        !content.contains("hi"),
        "the command never ran: {content}"
    );
}

/// A PermissionRequest `ask` (and any hook returning no decision) falls through to
/// the normal gate: the modal opens and the canned approval decides (ADR-0050).
#[tokio::test]
async fn permission_request_ask_falls_through_to_the_gate() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PermissionRequest");
    let shell = ScriptedShell::new(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // The gate falls through; the canned APPROVE lets it run.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "g1",
                "run_shell_command",
                json!({ "command": "echo hi" }),
            )),
            just(text_end("done")),
        ],
    )
    .with_approvals(vec![true]);
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    assert!(
        evs.iter().any(|e| matches!(e, Event::ApprovalRequest { .. })),
        "an ask falls through to the normal gate (a modal opens)"
    );
    let Event::ToolResult { content, .. } = tool_result(&evs, "g1") else {
        unreachable!()
    };
    assert!(content.contains("hi"), "the approved command ran: {content}");
}

// ---- PostToolUse -------------------------------------------------------------

/// A PostToolUse hook appends its additionalContext to a SUCCESSFUL result the
/// model reads (ADR-0066).
#[tokio::test]
async fn post_tool_use_appends_additional_context() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PostToolUse");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"additionalContext":"audited ok"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(!*is_error, "the tool succeeded");
    assert!(
        content.contains("audited ok"),
        "the PostToolUse context is appended: {content}"
    );
}

// ---- PostToolUseFailure ------------------------------------------------------

/// A PostToolUseFailure hook appends its additionalContext to a FAILED result
/// (context only - it cannot stop, ADR-0066). An unknown tool name is the clean
/// failure the batch answers as an error.
#[tokio::test]
async fn post_tool_use_failure_appends_context_on_error() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PostToolUseFailure");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"additionalContext":"failure noted"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // An unknown tool name fails (is_error), triggering PostToolUseFailure.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass("t1", "no_such_tool", json!({}))),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(*is_error, "the unknown tool failed");
    assert!(
        content.contains("failure noted"),
        "the failure context is appended: {content}"
    );
}

// ---- UserPromptSubmit (Phase 3b) ---------------------------------------------

/// A UserPromptSubmit `deny` vetoes the prompt: the model is NEVER called (the
/// script is empty, so a call would panic), and the Run closes on the hook's
/// reason. A visible reject line is surfaced (ADR-0018).
#[tokio::test]
async fn user_prompt_submit_block_vetoes_the_run() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("UserPromptSubmit");
    let shell = ScriptedShell::new(r#"{"decision":"deny","reason":"prompt not allowed"}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // An EMPTY script: if the vetoed prompt reached the model, `complete` would
    // panic (no scripted response), proving the model was never called.
    let deps = deps_for(&session, vec![]);
    let (outcome, deps) = run_with_hooks(&session, "do the thing", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let line = hook_line_for(&evs, "UserPromptSubmit").expect("a reject line was surfaced");
    assert!(line.contains("rejected"), "{line}");
    // The model was never asked (no MessageStart at all).
    assert!(
        !evs.iter().any(|e| matches!(e, Event::MessageStart { .. })),
        "a vetoed prompt never reaches the model"
    );
}

/// A UserPromptSubmit hook injecting additionalContext prepends it onto the
/// prompt the model reads (the first request's user turn carries the note).
#[tokio::test]
async fn user_prompt_submit_injects_additional_context() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("UserPromptSubmit");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"additionalContext":"context from a hook"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(&session, vec![just(text_end("ok"))]);
    let (outcome, deps) = run_with_hooks(&session, "the prompt", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    // The first request the model saw carries the injected context on the user
    // turn alongside the prompt.
    let requests = deps.requests.lock().unwrap();
    let first = requests.first().expect("a request was built");
    let user_text: String = first
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        user_text.contains("context from a hook"),
        "the injected context reaches the model: {user_text}"
    );
    let evs = events(&deps);
    assert!(
        hook_line_for(&evs, "UserPromptSubmit")
            .map(|l| l.contains("injected"))
            .unwrap_or(false),
        "the inject is visible"
    );
}

// ---- Stop (Phase 3b) ---------------------------------------------------------

/// A Stop hook that ALWAYS blocks forces continuations up to the resolved cap
/// (A2, qwen's `DEFAULT_STOP_HOOK_BLOCK_CAP=8`), then the Run ends despite the still-
/// blocking hook and qwen's cap warning is emitted. This is the configurable-cap
/// replacement for the old one-shot boolean: the hook cannot loop forever, but it
/// gets `cap` tries, not one.
#[tokio::test]
async fn stop_hook_forces_continuations_up_to_the_cap_then_ends() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("Stop");
    // continue:false with a stopReason INVERTS to "do not stop" + feedback.
    let shell = ScriptedShell::new(r#"{"continue":false,"stopReason":"keep going"}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // The default cap is 8: the hook forces continuations at counts 1..7 (7 extra
    // Passes) and the Run ends on the 8th block. So the loop consumes exactly 8
    // no-tool replies (the first end + 7 forced continuations); a 9th is scripted
    // as a safety net that must never be consumed (if it were, the cap failed to
    // stop the loop and the count assertion would reveal it).
    let cap = crate::run::hooks::DEFAULT_STOP_HOOK_BLOCK_CAP;
    let replies: Vec<_> = (0..=cap).map(|i| just(text_end(&format!("end {i}")))).collect();
    let deps = deps_for(&session, replies);
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    // Exactly `cap` model Passes: the initial end + (cap-1) forced continuations,
    // then the cap stops it (the safety-net reply is never consumed).
    let starts = evs
        .iter()
        .filter(|e| matches!(e, Event::MessageStart { .. }))
        .count();
    assert_eq!(starts as u64, cap, "the Stop hook forced continuations up to the cap");
    // The feedback was delivered as steering (the qwen-wrapped stopReason).
    let steered = evs.iter().any(|e| matches!(
        e,
        Event::SteeringDelivered { text } if text.contains("Stop hook feedback") && text.contains("keep going")
    ));
    assert!(steered, "the Stop feedback was injected as guidance");
    // qwen's cap warning is surfaced when the cap is hit (any Stop line carries it).
    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::ExtensionError { extension, message, .. }
                if extension == "hook Stop" && message.contains("overriding and ending the turn")
        )),
        "the cap warning is surfaced when the Stop hook is overridden"
    );
}

/// A Stop hook that does NOT block lets the Run end normally (the common case):
/// one Pass, no forced continuation.
#[tokio::test]
async fn stop_hook_that_allows_lets_the_run_end() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("Stop");
    let shell = ScriptedShell::new(r#"{"continue":true}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(&session, vec![just(text_end("done"))]);
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let starts = evs
        .iter()
        .filter(|e| matches!(e, Event::MessageStart { .. }))
        .count();
    assert_eq!(starts, 1, "a non-blocking Stop hook does not force a continuation");
}

// ---- Todo / Subagent fire-happened (Phase 3b) --------------------------------

/// A `todo_write` that adds items fires TodoCreated for each new item and
/// TodoCompleted for each completed one, detected at the RUN layer from the Plan
/// fold (the tool never touches the hook subsystem). The ScriptedShell counts the
/// fires across both events.
#[tokio::test]
async fn todo_write_fires_created_and_completed_from_the_run_layer() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    // One manager firing the SAME command hook on BOTH events, so the shared
    // ScriptedShell call count is the total TodoCreated + TodoCompleted fires.
    let hooks_cfg = json!({
        "TodoCreated": [ { "hooks": [ { "type": "command", "command": "t.sh" } ] } ],
        "TodoCompleted": [ { "hooks": [ { "type": "command", "command": "t.sh" } ] } ],
    });
    let manager = crate::hooks::HookManager::from_config(Some(&hooks_cfg));
    let llm = FakeLlm::script([]);
    // A shared shell so the test keeps the call counter after handing a boxed
    // ShellExec to the `Hooks` handle.
    let shell = std::sync::Arc::new(ScriptedShell::new(r#"{}"#));
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // A todo_write with one pending + one completed item: 2 created, 1 completed.
    let todos = json!({
        "todos": [
            { "id": "a", "content": "task a", "status": "pending" },
            { "id": "b", "content": "task b", "status": "completed" },
        ]
    });
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass("t1", "todo_write", todos)),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    // 2 created (both new) + 1 completed (task b) = 3 fires.
    assert_eq!(
        *shell.calls.lock().unwrap(),
        3,
        "TodoCreated fired per new item and TodoCompleted per completed item"
    );
}

/// The `agent` tool dispatch brackets SubagentStart / SubagentStop at the PARENT
/// run layer (Phase 3b, ADR-0066): both fire around the child-Run spawn even when
/// the spawn itself is unavailable in the test ctx (the bracket is on the tool
/// dispatch, not the child's success). One shared command hook on both events, so
/// the fire count is start + stop = 2.
#[tokio::test]
async fn agent_tool_brackets_subagent_start_and_stop() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let hooks_cfg = json!({
        "SubagentStart": [ { "hooks": [ { "type": "command", "command": "s.sh" } ] } ],
        "SubagentStop": [ { "hooks": [ { "type": "command", "command": "s.sh" } ] } ],
    });
    let manager = crate::hooks::HookManager::from_config(Some(&hooks_cfg));
    let shell = std::sync::Arc::new(ScriptedShell::new(r#"{}"#));
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "a1",
                "agent",
                json!({ "subagent_type": "general", "prompt": "do it", "description": "x" }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    assert_eq!(
        *shell.calls.lock().unwrap(),
        2,
        "SubagentStart and SubagentStop each fired once around the agent dispatch"
    );
}

// ---- SessionStart / Compact / Notification facade fires (Phase 3b) -----------

/// SessionStart returns the hook's injected additionalContext, which `init_agent`
/// folds onto the system prompt as initial context. Exercised through the SAME
/// `Hooks` facade the Agent builds.
#[tokio::test]
async fn session_start_returns_injected_initial_context() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("SessionStart");
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"additionalContext":"project convention: use tabs"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let context = hooks.session_start("startup").await;
    assert_eq!(
        context.as_deref(),
        Some("project convention: use tabs"),
        "SessionStart injects initial context"
    );
}

/// PreCompact returns the hook's injected compaction instruction; PostCompact is
/// observe-only (it returns nothing). Both fire through the facade the loop uses
/// around the compact Dep.
#[tokio::test]
async fn pre_compact_injects_instruction_post_compact_observes() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreCompact");
    let shell = std::sync::Arc::new(ScriptedShell::new(
        r#"{"hookSpecificOutput":{"additionalContext":"keep the API decisions"}}"#,
    ));
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let instruction = hooks.pre_compact().await;
    assert_eq!(
        instruction.as_deref(),
        Some("keep the API decisions"),
        "PreCompact injects a compaction instruction"
    );
    // PostCompact fires (observe-only): the shell was NOT configured for it, so a
    // second manager proves the observe-only fire returns nothing meaningful.
    hooks.post_compact().await;
    assert!(
        *shell.calls.lock().unwrap() >= 1,
        "PreCompact fired through the command hook"
    );
}

/// The Notification fire reaches the command hook (the "agent is waiting" alert),
/// exercised through the facade the Agent builds off the ask-request broadcast.
#[tokio::test]
async fn notification_fires_the_command_hook() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("Notification");
    let shell = std::sync::Arc::new(ScriptedShell::new(r#"{}"#));
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    hooks.notification("Approval requested: rm -rf /tmp/x").await;
    assert_eq!(
        *shell.calls.lock().unwrap(),
        1,
        "the Notification hook fired"
    );
}

// ---- fail-open ---------------------------------------------------------------

/// A hook runner ERROR (a spawn failure) is fail-open: the tool proceeds as if no
/// hook fired, and the result carries no injected block/context (ADR-0018).
#[tokio::test]
async fn hook_runner_error_is_fail_open() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ErringShell::new();
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult { is_error, .. } = tool_result(&evs, "t1") else {
        unreachable!()
    };
    // The tool ran normally: a failed hook never blocks the call (fail-open).
    assert!(!*is_error, "a hook error must not block the tool");
    // A fail-open (default) outcome decides nothing, so no deciding line fires.
    assert!(
        hook_line_for(&evs, "PreToolUse").is_none(),
        "a fail-open hook surfaces no deciding line"
    );
}

// ---- A1: universal permission-deny + ask-forces-gate on UNGATED tools --------

/// A PreToolUse hook returning ONLY a permission-channel `deny` (no base
/// `decision`) blocks an UNGATED tool universally (A1, qwen toolHickTriggers
/// `isDenied()` blocks regardless of gating): `list_directory` is ungated, yet the
/// deny still stops it and the reason reaches the model.
#[tokio::test]
async fn pre_tool_use_permission_deny_blocks_an_ungated_tool() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    // permissionDecision deny with NO base `decision` - the channel qwen honors
    // universally. `list_directory` does NOT gate, so the pre-fix code dropped it.
    let shell = ScriptedShell::new(
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"path off-limits"}}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(*is_error, "an ungated tool is still blocked by a permission deny");
    assert!(
        content.contains("path off-limits"),
        "the permissionDecisionReason reaches the model: {content}"
    );
}

/// A PreToolUse hook returning permission `ask` on an UNGATED tool FORCES the
/// confirmation gate (A1, qwen `isAsk()` -> requires-confirmation regardless of
/// gating) rather than silently proceeding: a modal opens and the canned deny
/// stops the call.
#[tokio::test]
async fn pre_tool_use_permission_ask_forces_gate_on_an_ungated_tool() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ScriptedShell::new(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // list_directory is ungated; the ask must synthesize a gate. Seed a DENY so a
    // gate that opens rejects the call - proving the gate opened (an ungated tool
    // would otherwise just run).
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    )
    .with_approvals(vec![false]);
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    assert!(
        evs.iter().any(|e| matches!(e, Event::ApprovalRequest { .. })),
        "an ungated tool with a PreToolUse ask opens the confirmation gate"
    );
    let Event::ToolResult { is_error, .. } = tool_result(&evs, "t1") else {
        unreachable!()
    };
    assert!(*is_error, "the canned deny at the forced gate stopped the call");
}

// ---- A4: multi-hook PreToolUse fold ------------------------------------------

/// TWO PreToolUse hooks fire; when EITHER blocks, the call is blocked and BOTH
/// hooks' reasons AND additionalContexts are folded (A4, qwen mergeWithOrLogic
/// joins all reasons + contexts across every matched hook).
#[tokio::test]
async fn pre_tool_use_folds_all_hooks_when_any_blocks() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    // Two DISTINCT command hooks on PreToolUse (two entries), so both fire and both
    // outcomes fold. The first injects context but does not block; the second
    // blocks. qwen folds ALL reasons/contexts, not just the blocker's.
    let hooks_cfg = json!({
        "PreToolUse": [
            { "hooks": [ { "type": "command", "command": "a.sh" } ] },
            { "hooks": [ { "type": "command", "command": "b.sh" } ] },
        ]
    });
    let manager = crate::hooks::HookManager::from_config(Some(&hooks_cfg));
    // A shell that answers by COMMAND: a.sh injects context (allow), b.sh blocks.
    let shell = ByCommandShell::new();
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    let evs = events(&deps);
    let Event::ToolResult {
        content, is_error, ..
    } = tool_result(&evs, "t1")
    else {
        unreachable!()
    };
    assert!(*is_error, "any blocking hook blocks the call");
    assert!(
        content.contains("policy violation"),
        "the blocker's reason folds in: {content}"
    );
    assert!(
        content.contains("note from a.sh"),
        "the NON-blocking hook's additionalContext still folds in: {content}"
    );
}

/// A shell that answers per command name: `a.sh` allows with an additionalContext,
/// `b.sh` blocks with a reason. Lets a multi-hook test drive two distinct outcomes.
struct ByCommandShell;

impl ByCommandShell {
    fn new() -> Self {
        ByCommandShell
    }
}

#[async_trait]
impl ShellExec for ByCommandShell {
    async fn run(
        &self,
        command: &str,
        _stdin_json: &str,
        _cwd: &str,
        _env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        let stdout = if command.contains("a.sh") {
            r#"{"decision":"allow","hookSpecificOutput":{"additionalContext":"note from a.sh"}}"#
        } else {
            r#"{"decision":"deny","reason":"policy violation"}"#
        };
        Ok(ShellResult {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }
}

// ---- H1/H2: payload base fields + tool_response object -----------------------

/// A shell that CAPTURES the stdin payload each fire saw, so a test can assert the
/// exact JSON a command hook receives. Shared behind an `Arc`.
struct CapturingShell {
    stdout: String,
    stdin: Mutex<Vec<String>>,
}

impl CapturingShell {
    fn new(stdout: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(CapturingShell {
            stdout: stdout.to_string(),
            stdin: Mutex::new(Vec::new()),
        })
    }
}

struct SharedCapturingShell(std::sync::Arc<CapturingShell>);

#[async_trait]
impl ShellExec for SharedCapturingShell {
    async fn run(
        &self,
        _command: &str,
        stdin_json: &str,
        _cwd: &str,
        _env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        self.0.stdin.lock().unwrap().push(stdin_json.to_string());
        Ok(ShellResult {
            exit_code: 0,
            stdout: self.0.stdout.clone(),
            stderr: String::new(),
        })
    }
}

/// A PreToolUse payload carries the five qwen base fields (H1): `session_id`,
/// `transcript_path`, `cwd`, `hook_event_name`, `timestamp` - plus the tool fields.
#[tokio::test]
async fn pre_tool_use_payload_carries_the_base_fields() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = CapturingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedCapturingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let _ = run_with_hooks(&session, "go", deps, &hooks).await;

    let payloads = shell.stdin.lock().unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
    // The with_caps synthetic identity (H1): every required base field is present.
    assert_eq!(payload["session_id"], "test-session");
    assert_eq!(payload["transcript_path"], "/tmp/test-session.jsonl");
    assert_eq!(payload["cwd"], session.root);
    assert_eq!(payload["hook_event_name"], "PreToolUse");
    assert!(
        payload["timestamp"].as_str().unwrap().contains('T'),
        "timestamp is ISO-8601: {}",
        payload["timestamp"]
    );
    assert_eq!(payload["tool_name"], "list_directory");
}

/// A PostToolUse payload carries `tool_response` as a JSON OBJECT (H2), never a
/// bare string: a plain-text tool output is wrapped as `{"output": <string>}` so a
/// qwen hook can index `tool_response` as a record.
#[tokio::test]
async fn post_tool_use_payload_wraps_tool_response_as_an_object() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PostToolUse");
    let shell = CapturingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedCapturingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let _ = run_with_hooks(&session, "go", deps, &hooks).await;

    let payloads = shell.stdin.lock().unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
    let tr = &payload["tool_response"];
    assert!(tr.is_object(), "tool_response is an object, not a string: {tr}");
    assert!(
        tr.get("output").and_then(|v| v.as_str()).is_some(),
        "a plain-text tool output is wrapped under `output`: {tr}"
    );
}

// ---- A5: systemMessage surfaced, gated on suppressOutput ---------------------

/// A PreToolUse hook's `systemMessage` is surfaced on the visible hook channel
/// (A5, qwen `processCommonHookOutputFields`) when `suppressOutput` is not set.
#[tokio::test]
async fn pre_tool_use_system_message_is_surfaced() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ScriptedShell::new(r#"{"decision":"allow","systemMessage":"heads up: slow disk"}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (_outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;

    let evs = events(&deps);
    assert!(
        hook_line_for(&evs, "PreToolUse")
            .into_iter()
            .chain(
                evs.iter().filter_map(|e| match e {
                    Event::ExtensionError { extension, message, .. }
                        if extension == "hook PreToolUse" => Some(message.clone()),
                    _ => None,
                })
            )
            .any(|l| l.contains("heads up: slow disk")),
        "the systemMessage is surfaced on the hook channel"
    );
}

/// A `systemMessage` accompanied by `suppressOutput: true` is NOT surfaced (A5's
/// gate): the user channel stays quiet.
#[tokio::test]
async fn system_message_is_suppressed_when_suppress_output_is_set() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let manager = manager_for("PreToolUse");
    let shell = ScriptedShell::new(
        r#"{"decision":"allow","systemMessage":"hidden note","suppressOutput":true}"#,
    );
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(shell),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (_outcome, deps) = run_with_hooks(&session, "go", deps, &hooks).await;

    let evs = events(&deps);
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            Event::ExtensionError { message, .. } if message.contains("hidden note")
        )),
        "a suppressed systemMessage is never surfaced"
    );
}

// ---- skill-hook registration on invocation (Phase 4c) ------------------------

/// A fake ShellExec that answers every command hook with the SAME crafted stdout
/// and RECORDS the `env` each fire saw (so a test can assert a registered skill
/// command hook carried `SUSPENDERS_SKILL_ROOT`). Shared behind an `Arc` so the
/// test keeps the recorder after the boxed handle moves into `Hooks`.
struct RecordingShell {
    stdout: String,
    envs: Mutex<Vec<HashMap<String, String>>>,
}

impl RecordingShell {
    fn new(stdout: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(RecordingShell {
            stdout: stdout.to_string(),
            envs: Mutex::new(Vec::new()),
        })
    }
}

struct SharedRecordingShell(std::sync::Arc<RecordingShell>);

#[async_trait]
impl ShellExec for SharedRecordingShell {
    async fn run(
        &self,
        _command: &str,
        _stdin_json: &str,
        _cwd: &str,
        env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        self.0.envs.lock().unwrap().push(env.clone());
        Ok(ShellResult {
            exit_code: 0,
            stdout: self.0.stdout.clone(),
            stderr: String::new(),
        })
    }
}

/// Writes a `SKILL.md` under `<skills_root>/<name>/` (the skills root
/// `SkillManager::discover` takes directly) and discovers a shared
/// [`SkillManager`] over it. The disk path matters: the skill's real `base_dir`
/// (`<skills_root>/<name>`) becomes the hook `skill_root` a registered command hook
/// carries in `SUSPENDERS_SKILL_ROOT`. Returns the manager and the skill's
/// `base_dir` so a test can assert the exact env value.
fn skill_manager_with(
    skills_root: &std::path::Path,
    name: &str,
    content: &str,
) -> (Arc<SkillManager>, std::path::PathBuf) {
    let dir = skills_root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
    (Arc::new(SkillManager::discover(skills_root, None)), dir)
}

/// A `HookManager` with NO standing config (so every fired hook comes from a
/// registered skill). The empty manager Phase 4c registers skills into.
fn empty_manager() -> crate::hooks::HookManager {
    crate::hooks::HookManager::from_config(None)
}

/// The model invoking the `skill` tool registers that skill's `hooks:` as
/// session-scoped: a skill carrying a PreToolUse hook makes that hook FIRE on the
/// next matching tool call in the same Run, and the registered COMMAND hook carries
/// the skill's base_dir in `SUSPENDERS_SKILL_ROOT`.
#[tokio::test]
async fn model_invoked_skill_registers_hooks_that_fire_and_carry_skill_root() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    // A skill whose frontmatter (between the fences) declares a PreToolUse command
    // hook. The nested `hooks:` block lives IN the frontmatter.
    let skill_md = "---\nname: fmt\ndescription: a formatter\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: guard.sh\n---\nbody text\n";
    let (skills, base_dir) = skill_manager_with(root.path(), "fmt", skill_md);

    let manager = empty_manager();
    // exit 0 + empty JSON => a steers-nothing allow; the fire COUNTS via the env log.
    let shell = RecordingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedRecordingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // Pass 1: the model invokes the skill (registers its PreToolUse hook). Pass 2:
    // the model calls list_directory - the registered hook fires BEFORE it.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass("s1", "skill", json!({ "skill": "fmt" }))),
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks_and_skills(
        &session,
        "go",
        deps,
        &hooks,
        Arc::clone(&skills),
        root.path().to_path_buf(),
    )
    .await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    // The registered PreToolUse hook fired exactly once (on the list_directory
    // call, not the skill call - the skill call preceded registration). Its env
    // carries the skill's base_dir as SUSPENDERS_SKILL_ROOT.
    let envs = shell.envs.lock().unwrap();
    assert_eq!(envs.len(), 1, "the registered hook fired once, on the next tool");
    let expected_root = base_dir.to_string_lossy().into_owned();
    assert_eq!(
        envs[0].get("SUSPENDERS_SKILL_ROOT").map(String::as_str),
        Some(expected_root.as_str()),
        "the registered command hook carries the skill root: {envs:?}"
    );
}

/// A model-invoked skill with NO `hooks:` block registers nothing: a later tool
/// call fires no hook.
#[tokio::test]
async fn model_invoked_skill_without_hooks_registers_nothing() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let skill_md = "---\nname: plain\ndescription: no hooks here\n---\njust a body\n";
    let (skills, _base_dir) = skill_manager_with(root.path(), "plain", skill_md);

    let manager = empty_manager();
    let shell = RecordingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedRecordingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass("s1", "skill", json!({ "skill": "plain" }))),
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks_and_skills(
        &session,
        "go",
        deps,
        &hooks,
        Arc::clone(&skills),
        root.path().to_path_buf(),
    )
    .await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    assert!(
        shell.envs.lock().unwrap().is_empty(),
        "a hook-less skill registers no hooks, so nothing fires"
    );
}

/// Invoking the SAME skill twice registers its hooks ONCE (idempotent, ADR-0066):
/// the live manager ends the Run with exactly one skill hook for the event, not a
/// stacked pair. Asserting on the manager (not the fire count) isolates the
/// registration, since the registered PreToolUse hook otherwise fires before every
/// SUBSEQUENT tool call (including the second `skill` call), which would confound a
/// fire-count assertion.
#[tokio::test]
async fn double_skill_invocation_is_idempotent() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let skill_md = "---\nname: fmt\ndescription: a formatter\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: guard.sh\n---\nbody\n";
    let (skills, _base_dir) = skill_manager_with(root.path(), "fmt", skill_md);

    let manager = empty_manager();
    let shell = RecordingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedRecordingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // Invoke the SAME skill twice (two Passes), then end. The second invocation
    // must register nothing new.
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass("s1", "skill", json!({ "skill": "fmt" }))),
            Entry::just(tool_pass("s2", "skill", json!({ "skill": "fmt" }))),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks_and_skills(
        &session,
        "go",
        deps,
        &hooks,
        Arc::clone(&skills),
        root.path().to_path_buf(),
    )
    .await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    // The manager holds exactly ONE PreToolUse skill hook after two invocations of
    // the same skill: registration is idempotent.
    assert_eq!(
        manager
            .hooks_for(crate::hooks::HookEvent::PreToolUse, Some("anything"))
            .len(),
        1,
        "a skill invoked twice registers its hook exactly once"
    );
}

/// The user `/<name>` slash path does NOT register skill hooks (Phase 4b is a
/// submit-prompt injection, not a `skill` tool call). A Run that never emits a
/// `skill` tool call registers nothing, so a later tool fires no skill hook - only
/// the model tool-invocation path registers (qwen semantics).
#[tokio::test]
async fn user_slash_path_does_not_register_skill_hooks() {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    // The skill carries a PreToolUse hook, but it is never invoked via the tool.
    let skill_md = "---\nname: fmt\ndescription: a formatter\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: guard.sh\n---\nbody\n";
    let (skills, _base_dir) = skill_manager_with(root.path(), "fmt", skill_md);

    let manager = empty_manager();
    let shell = RecordingShell::new(r#"{}"#);
    let llm = FakeLlm::script([]);
    let hooks = Hooks::with_caps(
        &manager,
        Box::new(SharedRecordingShell(std::sync::Arc::clone(&shell))),
        Box::new(UnusedHttp),
        &llm,
        &session.model,
        session.root.clone(),
    );

    // No `skill` tool call: the model just calls a plain tool (the slash path would
    // have injected the skill body into the prompt, not emitted a tool call).
    let deps = deps_for(
        &session,
        vec![
            Entry::just(tool_pass(
                "t1",
                "list_directory",
                json!({ "path": root.path().to_string_lossy() }),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with_hooks_and_skills(
        &session,
        "go",
        deps,
        &hooks,
        Arc::clone(&skills),
        root.path().to_path_buf(),
    )
    .await;
    assert!(matches!(outcome, Outcome::Ok(..)), "{outcome:?}");

    assert!(
        shell.envs.lock().unwrap().is_empty(),
        "no `skill` tool call means no registration, so the skill hook never fires"
    );
}

// ---- C-F1: ProcessGroupShell concurrency + timeout ---------------------------

/// The production [`ProcessGroupShell`] must drain stdout WHILE writing a large
/// stdin payload (C-F1): a >128KB payload piped to a `cat`-style hook that echoes
/// stdin to stdout completes without deadlock and round-trips every byte. Before
/// the fix the whole payload was written before any read, so it deadlocked once the
/// payload exceeded the pipe buffer (~64KB) and the hook's own stdout filled.
#[cfg(unix)]
#[tokio::test]
async fn process_group_shell_large_stdin_does_not_deadlock() {
    use crate::run::hooks::ProcessGroupShell;
    let root = TempDir::new().unwrap();
    // A payload well beyond the ~64KB pipe buffer.
    let payload = "x".repeat(256 * 1024);
    let out = ProcessGroupShell
        .run(
            "cat", // echoes stdin to stdout
            &payload,
            &root.path().to_string_lossy(),
            &HashMap::new(),
            30,
        )
        .await
        .expect("a large stdin round-trips without deadlock");
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.len(), payload.len(), "the whole payload echoed back");
}

/// A normal command round-trips its stdout (the happy path stays intact after the
/// concurrency rework).
#[cfg(unix)]
#[tokio::test]
async fn process_group_shell_round_trips_stdout() {
    use crate::run::hooks::ProcessGroupShell;
    let root = TempDir::new().unwrap();
    let out = ProcessGroupShell
        .run(
            "printf 'hello hook'",
            "{}",
            &root.path().to_string_lossy(),
            &HashMap::new(),
            30,
        )
        .await
        .expect("a normal hook round-trips its stdout");
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "hello hook");
}

/// A hook that sleeps past its timeout is killed (killpg) and returns fail-open
/// (an `Err` the leaf turns into the steers-nothing outcome). The write is small
/// here; the timeout wraps the whole write+wait, so the sleep is what elapses.
#[cfg(unix)]
#[tokio::test]
async fn process_group_shell_times_out_and_kills() {
    use crate::run::hooks::ProcessGroupShell;
    let root = TempDir::new().unwrap();
    let start = std::time::Instant::now();
    let result = ProcessGroupShell
        .run(
            "sleep 30",
            "{}",
            &root.path().to_string_lossy(),
            &HashMap::new(),
            1, // 1s timeout, far below the 30s sleep
        )
        .await;
    assert!(result.is_err(), "a hook past its timeout is an Err (fail-open)");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "the timeout fired promptly (killpg), not after the full sleep"
    );
}

// ---- A2/H1: pure-function unit coverage --------------------------------------

/// The Stop-hook cap resolver honors the env override, clamps to the ceiling, and
/// falls back to the default for empty/non-numeric/non-positive values (A2, qwen's
/// `resolveStopHookBlockingCap` + `normalizeStopHookBlockingCap`). Serialized by a
/// mutex because it mutates the process env.
#[test]
fn resolve_stop_hook_cap_honors_env_and_clamps() {
    use crate::run::hooks::{
        DEFAULT_STOP_HOOK_BLOCK_CAP, MAX_STOP_HOOK_BLOCK_CAP, STOP_HOOK_BLOCK_CAP_ENV,
        resolve_stop_hook_cap,
    };
    // No env: the default.
    unsafe { std::env::remove_var(STOP_HOOK_BLOCK_CAP_ENV) };
    assert_eq!(resolve_stop_hook_cap(), DEFAULT_STOP_HOOK_BLOCK_CAP);
    // A valid override.
    unsafe { std::env::set_var(STOP_HOOK_BLOCK_CAP_ENV, "3") };
    assert_eq!(resolve_stop_hook_cap(), 3);
    // Above the ceiling: clamped.
    unsafe { std::env::set_var(STOP_HOOK_BLOCK_CAP_ENV, "10000") };
    assert_eq!(resolve_stop_hook_cap(), MAX_STOP_HOOK_BLOCK_CAP);
    // Non-positive / non-numeric: the default.
    unsafe { std::env::set_var(STOP_HOOK_BLOCK_CAP_ENV, "0") };
    assert_eq!(resolve_stop_hook_cap(), DEFAULT_STOP_HOOK_BLOCK_CAP);
    unsafe { std::env::set_var(STOP_HOOK_BLOCK_CAP_ENV, "abc") };
    assert_eq!(resolve_stop_hook_cap(), DEFAULT_STOP_HOOK_BLOCK_CAP);
    unsafe { std::env::remove_var(STOP_HOOK_BLOCK_CAP_ENV) };
}

/// The cap warning wording matches qwen's `formatStopHookBlockingCapWarning`,
/// including the singular/plural of "time(s)" (A2).
#[test]
fn stop_hook_cap_warning_wording() {
    use crate::run::hooks::format_stop_hook_cap_warning;
    assert_eq!(
        format_stop_hook_cap_warning(8),
        "Stop hook blocked continuation 8 consecutive times; overriding and ending the turn."
    );
    assert_eq!(
        format_stop_hook_cap_warning(1),
        "Stop hook blocked continuation 1 consecutive time; overriding and ending the turn."
    );
}

/// `session_id` is derived from the transcript path's file stem, empty when absent
/// (H1).
#[test]
fn session_id_from_log_path_uses_the_file_stem() {
    use crate::run::hooks::session_id_from_log_path;
    assert_eq!(
        session_id_from_log_path("/sessions/20260802-120000-42.jsonl"),
        "20260802-120000-42"
    );
    assert_eq!(session_id_from_log_path(""), "");
}
