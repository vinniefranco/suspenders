//! Integration tests for the hook firing seam (Phase 3a, ADR-0066): the four
//! tool events wired into the live run loop via `batch`, exercised through the
//! injected fake capabilities (a fake ShellExec returning crafted JSON outcomes)
//! and the real `HookManager`. Each test drives a full Run so the decision folds
//! (PreToolUse block / permission composition / Post context / stop / fail-open)
//! are proven end to end, not in isolation.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

use crate::content::{ContentBlock, Usage};
use crate::event::Event;
use crate::hooks::{HttpPost, ShellExec, ShellResult};
use crate::llm::response::{Response, StopReason};
use crate::run::Outcome;
use crate::run::fixtures::{deps_for, events, just, run_with_hooks, session, text_end};
use crate::run::hooks::Hooks;
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
