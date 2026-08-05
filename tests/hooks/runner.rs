//! Unit tests for the hook runner (ADR-0066): command / http / prompt execution
//! via INJECTED fakes, covering the qwen exit-code decision rules, the http
//! non-2xx/plain-text branches, the prompt ok/block mapping, and fail-open on a
//! capability error.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::content::ContentBlock;
use crate::hooks::config::{Hook, HookKind};
use crate::hooks::outcome::{Decision, HookOutcome};
use crate::llm::model::{Api, Model};
use crate::llm::response::{Response, StopReason};
use crate::test_support::{Entry, FakeLlm};

/// The invocation a [`FakeShell`] records, so a test can assert the command,
/// payload-on-stdin, cwd, env, and timeout reached the child.
#[derive(Clone)]
struct ShellCall {
    command: String,
    stdin: String,
    cwd: String,
    env: HashMap<String, String>,
    timeout_secs: u64,
}

/// A fake ShellExec returning a scripted [`ShellResult`] and recording the last
/// invocation so a test can assert the payload and the SUSPENDERS_SKILL_ROOT
/// reached the child.
#[derive(Default)]
struct FakeShell {
    result: Mutex<Option<Result<ShellResult, String>>>,
    seen: Mutex<Option<ShellCall>>,
}

impl FakeShell {
    fn ok(result: ShellResult) -> Self {
        FakeShell {
            result: Mutex::new(Some(Ok(result))),
            seen: Mutex::new(None),
        }
    }
    fn err(reason: &str) -> Self {
        FakeShell {
            result: Mutex::new(Some(Err(reason.to_string()))),
            seen: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ShellExec for FakeShell {
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        *self.seen.lock().unwrap() = Some(ShellCall {
            command: command.to_string(),
            stdin: stdin_json.to_string(),
            cwd: cwd.to_string(),
            env: env.clone(),
            timeout_secs,
        });
        self.result.lock().unwrap().take().unwrap()
    }
}

/// A fake HttpPost returning a scripted `(status, body)` or an error, recording
/// the posted body.
#[derive(Default)]
struct FakeHttp {
    result: Mutex<Option<Result<(u16, String), String>>>,
    seen_body: Mutex<Option<String>>,
}

impl FakeHttp {
    fn ok(status: u16, body: &str) -> Self {
        FakeHttp {
            result: Mutex::new(Some(Ok((status, body.to_string())))),
            seen_body: Mutex::new(None),
        }
    }
    fn err(reason: &str) -> Self {
        FakeHttp {
            result: Mutex::new(Some(Err(reason.to_string()))),
            seen_body: Mutex::new(None),
        }
    }
}

#[async_trait]
impl HttpPost for FakeHttp {
    async fn post(
        &self,
        _url: &str,
        json_body: &str,
        _timeout_secs: u64,
    ) -> Result<(u16, String), String> {
        *self.seen_body.lock().unwrap() = Some(json_body.to_string());
        self.result.lock().unwrap().take().unwrap()
    }
}

/// A never-called ShellExec, for tests that exercise the http/prompt path.
struct UnusedShell;
#[async_trait]
impl ShellExec for UnusedShell {
    async fn run(
        &self,
        _c: &str,
        _s: &str,
        _cwd: &str,
        _e: &HashMap<String, String>,
        _t: u64,
    ) -> Result<ShellResult, String> {
        panic!("shell should not be called");
    }
}

/// A never-called HttpPost.
struct UnusedHttp;
#[async_trait]
impl HttpPost for UnusedHttp {
    async fn post(&self, _u: &str, _b: &str, _t: u64) -> Result<(u16, String), String> {
        panic!("http should not be called");
    }
}

/// A test Model to evaluate prompt hooks on.
fn test_model() -> Model {
    Model::new("local", "test-model", Api::OpenaiCompletions, 8192, 2048)
}

/// A Response carrying a single Text block (a fake model reply).
fn text_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        ..Response::default()
    }
}

fn command_hook(cmd: &str) -> Hook {
    Hook {
        kind: HookKind::Command {
            command: cmd.to_string(),
        },
        timeout_secs: Some(7),
    }
}

// --- command hook -------------------------------------------------------------

/// A command hook parses structured JSON stdout into the outcome, preserving
/// hookSpecificOutput, and delivers the payload on stdin with the skill root env.
#[tokio::test]
async fn command_json_stdout_parses_and_delivers_payload() {
    let shell = FakeShell::ok(ShellResult {
        exit_code: 0,
        stdout: r#"{"decision":"block","reason":"nope","hookSpecificOutput":{"additionalContext":"ctx"}}"#
            .to_string(),
        stderr: String::new(),
    });
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &shell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/work",
        skill_root: Some("/skills/fmt"),
    };
    let out = run_hook(&command_hook("guard.sh"), "{\"e\":1}", &ctx, &caps).await;

    assert_eq!(out.decision, Some(Decision::Block));
    assert!(out.is_blocking());
    assert_eq!(out.additional_context().as_deref(), Some("ctx"));

    let call = shell.seen.lock().unwrap().clone().unwrap();
    assert_eq!(call.command, "guard.sh");
    assert_eq!(call.stdin, "{\"e\":1}", "payload delivered on stdin");
    assert_eq!(call.cwd, "/work");
    assert_eq!(
        call.env.get("SUSPENDERS_SKILL_ROOT").map(String::as_str),
        Some("/skills/fmt")
    );
    assert_eq!(call.timeout_secs, 7, "hook timeout honored");
}

/// A config.json command hook (no skill root) sets no SUSPENDERS_SKILL_ROOT.
#[tokio::test]
async fn command_without_skill_root_sets_no_env() {
    let shell = FakeShell::ok(ShellResult {
        exit_code: 0,
        stdout: "{}".to_string(),
        stderr: String::new(),
    });
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &shell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/work",
        skill_root: None,
    };
    let _ = run_hook(&command_hook("x"), "{}", &ctx, &caps).await;
    let env = shell.seen.lock().unwrap().clone().unwrap().env;
    assert!(!env.contains_key("SUSPENDERS_SKILL_ROOT"));
}

/// Plain-text stdout with exit 0 becomes an allow carrying the text as a
/// systemMessage (qwen convertPlainTextToHookOutput).
#[tokio::test]
async fn command_plaintext_exit0_is_allow_with_system_message() {
    let out = run_command_with(ShellResult {
        exit_code: 0,
        stdout: "all good".to_string(),
        stderr: String::new(),
    })
    .await;
    assert_eq!(out.decision, Some(Decision::Allow));
    assert_eq!(out.system_message.as_deref(), Some("all good"));
}

/// Plain-text exit 1 is a non-blocking warning allow.
#[tokio::test]
async fn command_plaintext_exit1_is_nonblocking_warning() {
    let out = run_command_with(ShellResult {
        exit_code: 1,
        stdout: "hiccup".to_string(),
        stderr: String::new(),
    })
    .await;
    assert_eq!(out.decision, Some(Decision::Allow));
    assert!(!out.is_blocking());
    assert_eq!(out.reason.as_deref(), Some("Non-blocking error: hiccup"));
    assert_eq!(out.system_message.as_deref(), Some("Warning: hiccup"));
}

/// Exit 2 is a blocking error and reads STDERR only (ignoring stdout), yielding a
/// deny (qwen: exit 2 branch).
#[tokio::test]
async fn command_exit2_reads_stderr_and_denies() {
    let out = run_command_with(ShellResult {
        exit_code: 2,
        stdout: "IGNORED stdout".to_string(),
        stderr: "blocked: dangerous".to_string(),
    })
    .await;
    assert_eq!(out.decision, Some(Decision::Deny));
    assert!(out.is_blocking());
    assert_eq!(out.reason.as_deref(), Some("blocked: dangerous"));
}

/// No output at all yields the default (steers-nothing) outcome.
#[tokio::test]
async fn command_no_output_is_default() {
    let out = run_command_with(ShellResult {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    })
    .await;
    assert_eq!(out, HookOutcome::default());
}

/// A shell spawn error fails open to a steers-nothing outcome (a present hook
/// never fails the Run) that carries the error as `firing_error`, so the firing
/// layer can surface it visibly (ADR-0018).
#[tokio::test]
async fn command_shell_error_fails_open() {
    let shell = FakeShell::err("spawn failed");
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &shell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&command_hook("x"), "{}", &ctx, &caps).await;
    assert_eq!(
        out,
        HookOutcome {
            firing_error: Some("spawn failed".to_string()),
            ..HookOutcome::default()
        }
    );
    // Steers nothing: no block, no stop, no decision.
    assert!(!out.is_blocking() && !out.should_stop() && out.decision.is_none());
}

/// Helper: run a command hook against a scripted ShellResult.
async fn run_command_with(result: ShellResult) -> HookOutcome {
    let shell = FakeShell::ok(result);
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &shell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    run_hook(&command_hook("x"), "{}", &ctx, &caps).await
}

// --- http hook ----------------------------------------------------------------

fn http_hook(url: &str) -> Hook {
    Hook {
        kind: HookKind::Http {
            url: url.to_string(),
        },
        timeout_secs: None,
    }
}

/// An http hook POSTs the payload and parses a 2xx JSON body into the outcome.
#[tokio::test]
async fn http_2xx_json_parses_and_posts_payload() {
    let http = FakeHttp::ok(200, r#"{"decision":"deny","reason":"policy"}"#);
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &http,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&http_hook("https://x/hook"), "{\"p\":2}", &ctx, &caps).await;
    assert_eq!(out.decision, Some(Decision::Deny));
    assert_eq!(http.seen_body.lock().unwrap().clone().unwrap(), "{\"p\":2}");
}

/// A non-2xx status is a non-blocking continue (NOT a failure), per qwen.
#[tokio::test]
async fn http_non_2xx_is_nonblocking_continue() {
    let http = FakeHttp::ok(500, r#"{"decision":"block"}"#);
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &http,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&http_hook("https://x"), "{}", &ctx, &caps).await;
    assert_eq!(out.continue_, Some(true));
    assert!(out.decision.is_none(), "non-2xx body is ignored");
}

/// A 2xx plain-text (non-JSON) body rides as a systemMessage continue.
#[tokio::test]
async fn http_plaintext_body_is_system_message_continue() {
    let http = FakeHttp::ok(200, "just a note");
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &http,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&http_hook("https://x"), "{}", &ctx, &caps).await;
    assert_eq!(out.continue_, Some(true));
    assert_eq!(out.system_message.as_deref(), Some("just a note"));
}

/// A transport error fails open to a steers-nothing outcome carrying the error
/// as `firing_error` (surfaced visibly by the firing layer, ADR-0018).
#[tokio::test]
async fn http_transport_error_fails_open() {
    let http = FakeHttp::err("connection refused");
    let llm = FakeLlm::script([]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &http,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&http_hook("https://x"), "{}", &ctx, &caps).await;
    assert_eq!(
        out,
        HookOutcome {
            firing_error: Some("connection refused".to_string()),
            ..HookOutcome::default()
        }
    );
    // Steers nothing: no block, no stop, no decision.
    assert!(!out.is_blocking() && !out.should_stop() && out.decision.is_none());
}

// --- prompt hook --------------------------------------------------------------

fn prompt_hook(template: &str) -> Hook {
    Hook {
        kind: HookKind::Prompt {
            prompt: template.to_string(),
        },
        timeout_secs: None,
    }
}

/// A prompt hook replies ok:false -> a blocking outcome (continue:false,
/// stopReason=reason, decision:block), with additionalContext threaded through.
#[tokio::test]
async fn prompt_ok_false_is_block() {
    let llm = FakeLlm::script([Entry::just(text_response(
        r#"{"ok": false, "reason": "unsafe edit", "additionalContext": "see policy"}"#,
    ))]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("evaluate: $ARGUMENTS"), "{}", &ctx, &caps).await;
    assert_eq!(out.continue_, Some(false));
    assert!(out.should_stop());
    assert_eq!(out.decision, Some(Decision::Block));
    assert_eq!(out.reason.as_deref(), Some("unsafe edit"));
    assert_eq!(out.stop_reason.as_deref(), Some("unsafe edit"));
    assert_eq!(out.additional_context().as_deref(), Some("see policy"));
}

/// A prompt hook replies ok:true -> an allow, optional reason/context carried.
#[tokio::test]
async fn prompt_ok_true_is_allow() {
    let llm = FakeLlm::script([Entry::just(text_response(r#"{"ok": true}"#))]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("go"), "{}", &ctx, &caps).await;
    assert_eq!(out.continue_, Some(true));
    assert_eq!(out.decision, Some(Decision::Allow));
    assert!(!out.is_blocking());
}

/// The $ARGUMENTS placeholder is replaced with the PRETTY-printed payload JSON in
/// the request (L1, qwen `promptHookRunner.ts`: `JSON.stringify(input, null, 2)`).
/// The prompt hook alone pretty-prints - command/http deliver the compact payload.
#[tokio::test]
async fn prompt_splices_pretty_payload_into_template() {
    use std::sync::Arc;
    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let llm = FakeLlm::script([Entry::dynamic(vec![], move |req, _model| {
        // Capture the last user message text.
        let text = req
            .messages
            .last()
            .and_then(|m| m.content.first())
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        *seen2.lock().unwrap() = Some(text);
        text_response(r#"{"ok": true}"#)
    })]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let _ = run_hook(
        &prompt_hook("check <$ARGUMENTS>"),
        "{\"tool\":\"x\"}",
        &ctx,
        &caps,
    )
    .await;
    // 2-space indented, not the compact `{"tool":"x"}`.
    assert_eq!(
        seen.lock().unwrap().clone().unwrap(),
        "check <{\n  \"tool\": \"x\"\n}>"
    );
}

/// A model reply that is not the {ok} shape defaults to an EXPLICIT allow (L2,
/// qwen `parseResponse` fail-open -> `{ok:true, ...defaulting to allow}` ->
/// `{continue:true, decision:'allow'}`), NOT the steers-nothing default outcome -
/// so a downstream fold sees the same allow qwen produces.
#[tokio::test]
async fn prompt_non_ok_reply_defaults_to_explicit_allow() {
    let llm = FakeLlm::script([Entry::just(text_response("I cannot comply, sorry."))]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("go"), "{}", &ctx, &caps).await;
    assert_eq!(out.continue_, Some(true));
    assert_eq!(out.decision, Some(Decision::Allow));
    assert!(!out.is_blocking());
    assert!(
        out.reason
            .as_deref()
            .unwrap()
            .contains("defaulting to allow"),
        "the allow carries qwen's fail-open reason: {:?}",
        out.reason
    );
}

/// An LLM FAILURE (the boundary folds it into a Response with `stop_reason:
/// Error`, ADR-0002) fails open to a steers-nothing outcome that carries the
/// error as `firing_error`, exactly like a command spawn failure or an http
/// transport failure, so the firing layer surfaces it visibly (ADR-0018) -
/// never a silent skip, never a block.
#[tokio::test]
async fn prompt_llm_error_fails_open() {
    let llm = FakeLlm::script([Entry::error("model down")]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("go"), "{}", &ctx, &caps).await;
    assert_eq!(
        out,
        HookOutcome {
            firing_error: Some("model down".to_string()),
            ..HookOutcome::default()
        }
    );
    // Steers nothing: no block, no stop, no decision.
    assert!(!out.is_blocking() && !out.should_stop() && out.decision.is_none());
}

/// A prompt hook reply wrapped in a ```json ... ``` markdown fence is unwrapped
/// before parsing (L2, qwen `parseResponse` regex): the inner block's decision is
/// honored, so a small local model that fences its JSON still blocks/allows.
#[tokio::test]
async fn prompt_reply_wrapped_in_json_fence_is_parsed() {
    let llm = FakeLlm::script([Entry::just(text_response(
        "```json\n{\"ok\": false, \"reason\": \"fenced block\"}\n```",
    ))]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("go"), "{}", &ctx, &caps).await;
    assert_eq!(out.decision, Some(Decision::Block));
    assert_eq!(out.reason.as_deref(), Some("fenced block"));
}

/// A bare ``` fence (no `json` language tag) is also unwrapped (qwen's regex makes
/// the tag optional).
#[tokio::test]
async fn prompt_reply_wrapped_in_bare_fence_is_parsed() {
    let llm = FakeLlm::script([Entry::just(text_response("```\n{\"ok\": true}\n```"))]);
    let model = test_model();
    let caps = HookCaps {
        shell: &UnusedShell,
        http: &UnusedHttp,
        llm: &llm,
        prompt_model: &model,
    };
    let ctx = HookRunContext {
        cwd: "/w",
        skill_root: None,
    };
    let out = run_hook(&prompt_hook("go"), "{}", &ctx, &caps).await;
    assert_eq!(out.decision, Some(Decision::Allow));
    assert_eq!(out.continue_, Some(true));
}
