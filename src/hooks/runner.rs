//! [`run_hook`] - executing ONE resolved hook to a [`HookOutcome`] (ADR-0066).
//!
//! A hook runs one of three ways (ADR-0066): a `command` shells out (the JSON
//! payload on stdin, stdout parsed back), an `http` POSTs the payload and reads
//! the body, a `prompt` evaluates via the LLM. Faithful to qwen's
//! `hookRunner.ts` / `httpHookRunner.ts` / `promptHookRunner.ts`.
//!
//! ## Layering: inject, do not reach up
//!
//! `src/hooks/` is a LEAF (ADR-0066): it may depend DOWNWARD (on `crate::llm`,
//! `crate::content`) but NOT on run/agent/ui/session. The obvious reuse - the
//! `run_command` process-group shell exec (`crate::tools::run_command::spawn`) -
//! is NOT reachable without an upward edge: that helper is `pub(super)` inside
//! `crate::tools::run_command`, whose parent module imports `crate::tool` and
//! `crate::agent::background_shell`, so `hooks -> tools::run_command` would drag
//! in `tools -> agent` and invert the SDP (and cycle once Phase 3 wires
//! `agent -> hooks`). So the shell-exec, http-post, and prompt capabilities are
//! INJECTED behind object-safe traits ([`ShellExec`], [`HttpPost`], and the
//! existing [`crate::llm::Llm`]). The PRODUCTION `ShellExec` (built in Phase 3,
//! at the wiring layer that already sits above both `hooks` and `run_command`)
//! reuses ADR-0023's `setpgid` process-group isolation; the leaf stays testable
//! with fakes and free of the upward edge.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::content::ContentBlock;
use crate::hooks::config::{Hook, HookKind};
use crate::hooks::outcome::HookOutcome;
use crate::llm::model::Model;
use crate::llm::{Llm, LlmRequest};

/// qwen's default hook timeout (`DEFAULT_HOOK_TIMEOUT = 60_000ms`,
/// `hookRunner.ts`): applied when a hook declares no `timeout`.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// The exit code qwen treats as a blocking error (`hookRunner.ts`): on a plain-
/// text (non-JSON) command output, exit 2 -> `deny`, exit 1 -> non-blocking
/// warning, exit 0 -> allow.
const EXIT_BLOCKING: i32 = 2;

/// The HTTP 2xx success range (`[HTTP_OK_MIN, HTTP_OK_MAX)`): a hook response
/// inside it is parsed for a decision, outside it is a non-blocking continue
/// (qwen's `!response.ok` branch, `httpHookRunner.ts`).
const HTTP_OK_MIN: u16 = 200;
const HTTP_OK_MAX: u16 = 300;

/// The env var carrying a skill's directory to a skill-sourced command hook
/// (ADR-0066; qwen's `QWEN_SKILL_ROOT`), so a hook `command` can resolve scripts
/// shipped beside the `SKILL.md`.
pub const SKILL_ROOT_ENV: &str = "SUSPENDERS_SKILL_ROOT";

/// The result of running one command hook: its merged exit code and the text to
/// parse (qwen's `stdout`/`stderr`/`exitCode`). The [`ShellExec`] capability
/// returns this so [`run_hook`] can apply qwen's exit-code decision rules without
/// the leaf owning the process machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellResult {
    /// The process exit code (`-1` if killed by a signal, qwen's fallback).
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// The injected shell-exec capability (the layering seam, see the module doc). A
/// command hook is run through this rather than reaching up into
/// `run_command::spawn`; the production impl (Phase 3) reuses ADR-0023's
/// process-group isolation and honors the timeout, and tests inject a fake.
#[async_trait]
pub trait ShellExec: Send + Sync {
    /// Runs `command`, writing `stdin_json` to the child's stdin, in `cwd`, with
    /// the additional `env` (the `SUSPENDERS_SKILL_ROOT` for a skill hook), bounded
    /// by `timeout_secs`. Returns the captured [`ShellResult`], or an `Err` for a
    /// spawn failure / timeout (a fail-open non-blocking outcome, ADR-0066).
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ShellResult, String>;
}

/// The injected HTTP-POST capability (the layering seam). An http hook POSTs its
/// JSON payload through this; the production impl (Phase 3) rides `reqwest`, and
/// tests inject a fake. Kept minimal - qwen's SSRF guard / header interpolation /
/// `once` gating are the deferred surface (ADR-0066).
#[async_trait]
pub trait HttpPost: Send + Sync {
    /// POSTs `json_body` to `url` bounded by `timeout_secs`. Returns
    /// `(status, body)` on a completed request (any status - a non-2xx is a
    /// non-blocking continue per qwen), or an `Err` for a transport failure.
    async fn post(
        &self,
        url: &str,
        json_body: &str,
        timeout_secs: u64,
    ) -> Result<(u16, String), String>;
}

/// The capabilities a [`run_hook`] call draws on, injected so the runner is a
/// pure leaf unit-testable with fakes (ADR-0066). Borrows, so one bundle serves a
/// whole event's hooks. The `prompt` capability is the existing [`Llm`] boundary
/// (ADR-0002) plus the [`Model`] to evaluate on (the Active Model, ADR-0033),
/// reused rather than reinvented.
pub struct HookCaps<'a> {
    /// Runs command hooks (the injected shell-exec, see the module doc).
    pub shell: &'a dyn ShellExec,
    /// Runs http hooks (the injected HTTP-POST).
    pub http: &'a dyn HttpPost,
    /// Evaluates prompt hooks (the existing LLM boundary).
    pub llm: &'a dyn Llm,
    /// The Model a prompt hook evaluates on (the Active Model unless a per-hook
    /// override is given, ADR-0066; the override is deferred).
    pub prompt_model: &'a Model,
}

/// The runtime context one hook fires in: the working directory the JSON payload
/// reports (and a command hook runs in), and the optional skill root a
/// skill-sourced command hook additionally sees in `SUSPENDERS_SKILL_ROOT`
/// (ADR-0066). A `config.json` hook has no skill root.
pub struct HookRunContext<'a> {
    /// The cwd delivered in the payload and used as a command hook's working dir.
    pub cwd: &'a str,
    /// The skill's directory for a skill-sourced command hook, or `None` for a
    /// standing `config.json` hook.
    pub skill_root: Option<&'a str>,
}

/// The LLM system prompt for a prompt hook, VERBATIM from qwen's
/// `LLM_HOOK_SYSTEM_PROMPT` (`promptHookRunner.ts`) with the product name adapted
/// (Suspenders, not Qwen Code), so a prompt hook's single-turn evaluation asks for
/// the exact `{"ok": ..., "reason"?: ..., "additionalContext"?: ...}` shape qwen
/// expects.
const PROMPT_HOOK_SYSTEM_PROMPT: &str = "You are evaluating a hook in Suspenders.
Your task is to analyze the provided context and make a decision.

You MUST respond with valid JSON in one of these formats:
- {\"ok\": true} - Allow the operation to proceed
- {\"ok\": true, \"additionalContext\": \"...\"} - Allow and provide context
- {\"ok\": false, \"reason\": \"...\"} - Block the operation with a reason
- {\"ok\": false, \"reason\": \"...\", \"additionalContext\": \"...\"} - Block with reason and context

The \"reason\" field is required when blocking and will be shown to the user.
The \"additionalContext\" field is optional and can provide useful information to the main conversation.

Do NOT output anything other than the JSON response. No explanations, no markdown formatting.";

/// Runs one resolved [`Hook`] to a [`HookOutcome`], dispatching on its
/// [`HookKind`] (ADR-0066). `payload` is the JSON event payload (delivered to a
/// command hook on stdin, POSTed by an http hook, spliced into a prompt hook's
/// template at `$ARGUMENTS`). The effective timeout is the hook's `timeout` else
/// qwen's 60s default.
///
/// Fail-open (ADR-0066): a spawn failure, a timeout, an unparseable output, or a
/// transport failure yields `Ok(HookOutcome::default())` (a `continue`-by-default,
/// steers-nothing outcome), NOT an `Err` - a present hook never fails the Run.
pub async fn run_hook(
    hook: &Hook,
    payload: &str,
    ctx: &HookRunContext<'_>,
    caps: &HookCaps<'_>,
) -> HookOutcome {
    let timeout_secs = hook.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
    match &hook.kind {
        HookKind::Command { command } => {
            run_command_hook(command, payload, timeout_secs, ctx, caps.shell).await
        }
        HookKind::Http { url } => run_http_hook(url, payload, timeout_secs, caps.http).await,
        HookKind::Prompt { prompt } => run_prompt_hook(prompt, payload, timeout_secs, caps).await,
    }
}

/// Runs a command hook: hands the command + payload-on-stdin + skill-root env to
/// the injected [`ShellExec`], then maps its [`ShellResult`] to a [`HookOutcome`]
/// by qwen's exit-code rules (`hookRunner.ts`). A spawn failure / timeout
/// fails open to the default outcome.
async fn run_command_hook(
    command: &str,
    payload: &str,
    timeout_secs: u64,
    ctx: &HookRunContext<'_>,
    shell: &dyn ShellExec,
) -> HookOutcome {
    // A skill-sourced command hook additionally sees SUSPENDERS_SKILL_ROOT so its
    // command can resolve scripts beside the SKILL.md (ADR-0066).
    let mut env: HashMap<String, String> = HashMap::new();
    if let Some(root) = ctx.skill_root {
        env.insert(SKILL_ROOT_ENV.to_string(), root.to_string());
    }

    let result = match shell
        .run(command, payload, ctx.cwd, &env, timeout_secs)
        .await
    {
        Ok(result) => result,
        // Spawn failure / timeout: fail open (a present hook never fails the Run).
        Err(_) => return HookOutcome::default(),
    };

    outcome_from_shell(&result)
}

/// Maps a command hook's [`ShellResult`] to a [`HookOutcome`], VERBATIM from
/// qwen's `hookRunner.ts` close handler + `convertPlainTextToHookOutput`:
///
/// - exit 2 is a blocking error: parse STDERR only (ignore stdout).
/// - otherwise parse stdout, falling back to stderr.
/// - JSON output parses into the structured outcome (preserving
///   `hookSpecificOutput`); non-JSON text converts to a structured outcome by
///   exit code (0 -> allow, 1 -> non-blocking warning, 2/other -> deny).
/// - no text at all -> the default (steers-nothing) outcome.
fn outcome_from_shell(result: &ShellResult) -> HookOutcome {
    let is_blocking = result.exit_code == EXIT_BLOCKING;

    // Exit 2 uses stderr only; otherwise stdout with a stderr fallback.
    let text = if is_blocking {
        result.stderr.trim()
    } else {
        let out = result.stdout.trim();
        if out.is_empty() {
            result.stderr.trim()
        } else {
            out
        }
    };

    if text.is_empty() {
        return HookOutcome::default();
    }

    // Structured JSON preserves hookSpecificOutput; else convert plain text.
    match HookOutcome::parse(text) {
        Ok(outcome) => outcome,
        Err(_) => plain_text_outcome(text, result.exit_code),
    }
}

/// Converts a command hook's plain-text (non-JSON) output to a structured
/// [`HookOutcome`], VERBATIM from qwen's `convertPlainTextToHookOutput`: exit 0 is
/// an allow carrying the text as `systemMessage`, exit 1 is a non-blocking allow
/// warning, any other non-zero (including 2) is a `deny` with the text as the
/// reason.
fn plain_text_outcome(text: &str, exit_code: i32) -> HookOutcome {
    use crate::hooks::outcome::Decision;
    match exit_code {
        0 => HookOutcome {
            decision: Some(Decision::Allow),
            reason: Some("Hook executed successfully".to_string()),
            system_message: Some(text.to_string()),
            ..HookOutcome::default()
        },
        1 => HookOutcome {
            decision: Some(Decision::Allow),
            reason: Some(format!("Non-blocking error: {text}")),
            system_message: Some(format!("Warning: {text}")),
            ..HookOutcome::default()
        },
        // Exit 2 and every other non-zero code are blocking.
        _ => HookOutcome {
            decision: Some(Decision::Deny),
            reason: Some(text.to_string()),
            ..HookOutcome::default()
        },
    }
}

/// Runs an http hook: POSTs the JSON payload through the injected [`HttpPost`] and
/// maps the response, VERBATIM from qwen's `httpHookRunner.ts`: a non-2xx status
/// is a non-blocking `continue` (NOT a failure), a JSON body parses into the
/// outcome, a non-JSON body rides as a `systemMessage` continue, and a transport
/// failure fails open to the default outcome.
async fn run_http_hook(
    url: &str,
    payload: &str,
    timeout_secs: u64,
    http: &dyn HttpPost,
) -> HookOutcome {
    let (status, body) = match http.post(url, payload, timeout_secs).await {
        Ok(pair) => pair,
        // Transport failure: fail open.
        Err(_) => return HookOutcome::default(),
    };

    // A non-2xx status is a non-blocking continue (qwen), not a decision.
    if !(HTTP_OK_MIN..HTTP_OK_MAX).contains(&status) {
        return continue_outcome();
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return continue_outcome();
    }

    match HookOutcome::parse(trimmed) {
        Ok(outcome) => outcome,
        // A non-JSON body is surfaced as a systemMessage continue (qwen's
        // plain-text response branch).
        Err(_) => HookOutcome {
            continue_: Some(true),
            system_message: Some(trimmed.to_string()),
            ..HookOutcome::default()
        },
    }
}

/// A `{ continue: true }` outcome (qwen's non-blocking http result). The single
/// place the http non-decision continue is spelled.
fn continue_outcome() -> HookOutcome {
    HookOutcome {
        continue_: Some(true),
        ..HookOutcome::default()
    }
}

/// Runs a prompt hook: splices the payload into the template at `$ARGUMENTS`,
/// sends a single-turn completion on the prompt Model through the injected
/// [`Llm`], and maps the model's `{"ok", "reason"?, "additionalContext"?}` reply
/// to a [`HookOutcome`] by qwen's `promptHookRunner.ts` `processResult`:
///
/// - `ok: false` -> a blocking outcome (`continue: false`, `stopReason` = reason,
///   `decision: block`, `reason`, `additionalContext` when given).
/// - `ok: true` -> an allow (`continue: true`, `decision: allow`, optional
///   `reason` / `additionalContext`).
///
/// An LLM error, an empty reply, or a reply that is not the `{ok}` shape fails
/// open to the default (non-blocking) outcome (qwen's non-blocking-error branch).
async fn run_prompt_hook(
    prompt: &str,
    payload: &str,
    _timeout_secs: u64,
    caps: &HookCaps<'_>,
) -> HookOutcome {
    // Replace every $ARGUMENTS with the payload JSON (qwen's
    // replaceArgumentsPlaceholder). The prompt hook alone PRETTY-prints the input
    // (qwen `promptHookRunner.ts`: `JSON.stringify(input, null, 2)`) - the
    // command/http paths deliver the compact payload verbatim - so a small local
    // model reads a legible 2-space-indented object. A `payload` that does not
    // re-parse (it always does; it is our own serialization) falls back to the
    // compact form.
    let pretty = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| payload.to_string());
    let processed = prompt.replace("$ARGUMENTS", &pretty);

    let request = LlmRequest::new(
        PROMPT_HOOK_SYSTEM_PROMPT,
        vec![crate::content::Message::user(vec![ContentBlock::text(
            processed,
        )])],
        Vec::new(),
    );

    // The single-turn completion; the boundary never Errs (ADR-0002), so a
    // failure rides in the Response's stop_reason and folds into fail-open below.
    let mut sink = |_event: &crate::llm::StreamEvent| {};
    let response = caps
        .llm
        .complete(&request, caps.prompt_model, &mut sink)
        .await;

    // The model's reply text (Text blocks concatenated).
    let reply = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    // Strip a leading/trailing ```json ... ``` markdown fence before parsing
    // (qwen `parseResponse`): a small local model often wraps its JSON reply in a
    // fenced block, and qwen extracts the inner text before `JSON.parse`.
    let stripped = strip_json_fence(reply.trim());
    match LlmHookResponse::parse(stripped) {
        Some(r) => outcome_from_prompt_response(&r),
        // Unparseable / empty reply: qwen's `parseResponse` fail-open returns
        // `{ok:true, reason:"...defaulting to allow"}`, which `processResult` maps
        // to `{continue:true, decision:'allow', reason}`. An EXPLICIT allow (not the
        // steers-nothing default) so a downstream fold sees the same allow qwen
        // produces.
        None => outcome_from_prompt_response(&LlmHookResponse {
            ok: true,
            reason: Some("Failed to parse LLM response, defaulting to allow".to_string()),
            additional_context: None,
        }),
    }
}

/// Strips a single leading/trailing markdown code fence (```json … ``` or ``` …
/// ```) from an LLM reply, VERBATIM from qwen's `parseResponse` regex
/// (`/```(?:json)?\s*([\s\S]*?)```/`): the FIRST fenced block's inner text is
/// returned trimmed, else the input is returned unchanged. A small local model
/// often fences its JSON, so the runner must unwrap it before parsing.
fn strip_json_fence(text: &str) -> &str {
    let Some(open) = text.find("```") else {
        return text;
    };
    // Skip past the opening fence and an optional `json` language tag, then any
    // whitespace, matching qwen's `` ```(?:json)?\s* ``.
    let after_ticks = &text[open + 3..];
    let after_lang = after_ticks
        .strip_prefix("json")
        .unwrap_or(after_ticks)
        .trim_start();
    // The inner block ends at the next closing fence.
    match after_lang.find("```") {
        Some(close) => after_lang[..close].trim(),
        None => text,
    }
}

/// The prompt hook's LLM reply shape (qwen's `LLMHookResponse`): `ok` gates
/// allow/block, `reason` is required on a block, `additionalContext` is optional.
#[derive(Debug, Clone, serde::Deserialize)]
struct LlmHookResponse {
    ok: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, rename = "additionalContext")]
    additional_context: Option<String>,
}

impl LlmHookResponse {
    /// Parses the model's reply into the `{ok}` shape, or `None` when it is not
    /// that shape (an empty or prose reply). The fail-open gate for a prompt hook.
    fn parse(text: &str) -> Option<LlmHookResponse> {
        if text.is_empty() {
            return None;
        }
        serde_json::from_str(text).ok()
    }
}

/// Maps a parsed [`LlmHookResponse`] to a [`HookOutcome`], VERBATIM from qwen's
/// `processResult`. `ok: false` is a block (halt + deny), `ok: true` is an allow.
fn outcome_from_prompt_response(r: &LlmHookResponse) -> HookOutcome {
    use crate::hooks::outcome::Decision;
    let hso = r
        .additional_context
        .as_ref()
        .map(|ctx| serde_json::json!({ "additionalContext": ctx }));

    if !r.ok {
        // Blocking decision.
        let reason = r
            .reason
            .clone()
            .unwrap_or_else(|| "Blocked by prompt hook".to_string());
        HookOutcome {
            continue_: Some(false),
            stop_reason: Some(reason.clone()),
            decision: Some(Decision::Block),
            reason: Some(reason),
            hook_specific_output: hso,
            ..HookOutcome::default()
        }
    } else {
        // Allow.
        HookOutcome {
            continue_: Some(true),
            decision: Some(Decision::Allow),
            reason: r.reason.clone(),
            hook_specific_output: hso,
            ..HookOutcome::default()
        }
    }
}

#[cfg(test)]
#[path = "../../tests/hooks/runner.rs"]
mod tests;
