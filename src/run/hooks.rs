//! The Run's hook firing facade (Phase 3a, ADR-0066): the wiring layer that
//! sits ABOVE both `crate::hooks` (the leaf) and `crate::tools::run_command`, so
//! it may reach into both. It owns the two production capability impls the leaf
//! injects ([`ProcessGroupShell`] over ADR-0023's process-group isolation and
//! [`ReqwestHttp`] over `reqwest`), wires the captured [`Llm`] + [`Model`] as the
//! prompt capability, and exposes [`Hooks`] - the per-Run handle the tool-dispatch
//! seam ([`crate::run::batch`]) fires the four tool events through.
//!
//! ## Why here, not in the leaf
//!
//! `crate::hooks` is a LEAF (ADR-0066): it may not reach up into `run_command`'s
//! shell exec (that would drag `tools -> agent` in and invert the SDP). So the
//! shell/http capabilities are INJECTED behind [`ShellExec`]/[`HttpPost`], and
//! their PRODUCTION impls live HERE, at the layer that already sits above both.
//! [`ProcessGroupShell`] mirrors `run_command`'s `setpgid` spawn but adds the
//! stdin-payload + `SUSPENDERS_SKILL_ROOT` env a hook command needs; the leaf
//! stays a pure, fake-testable unit.
//!
//! ## The four events wired this phase
//!
//! Phase 3a wires the tool-dispatch + permission events only (the other twelve
//! are Phase 3b): [`Hooks::pre_tool_use`], [`Hooks::permission_request`],
//! [`Hooks::post_tool_use`], [`Hooks::post_tool_use_failure`]. Each fires every
//! [`HookManager::hooks_for`] hook for its event through [`run_hook`] and folds
//! the outcomes into the one decision the batch acts on (block / permission /
//! injected context / stop). A firing error is fail-open (ADR-0018): the tool
//! proceeds as if no hook fired, and every deciding fire is surfaced visibly.

use std::collections::HashMap;

use crate::hooks::{
    HookCaps, HookEvent, HookManager, HookRunContext, HttpPost, PermissionDecision, ShellExec,
    ShellResult, run_hook,
};
use crate::llm::Llm;
use crate::llm::model::Model;

/// The production [`ShellExec`] (ADR-0066, ADR-0023): runs a command hook in its
/// own process group (`setpgid`) so a timeout killpg's the whole subtree, writing
/// the JSON payload to the child's stdin and adding the hook's `env` (the
/// `SUSPENDERS_SKILL_ROOT` a skill hook carries). It mirrors
/// [`crate::tools::run_command`]'s foreground spawn but adds stdin + env, which
/// that helper does not expose; living here (above both `hooks` and
/// `run_command`) keeps the `hooks` leaf free of the upward edge.
pub struct ProcessGroupShell;

#[async_trait::async_trait]
impl ShellExec for ProcessGroupShell {
    #[cfg(unix)]
    async fn run(
        &self,
        command: &str,
        stdin_json: &str,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Own process group so a timeout killpg's the whole subtree (ADR-0023).
        // SAFETY: `pre_exec` runs in the child after `fork`, before `exec`, where
        // only async-signal-safe calls are permitted. The sole call is
        // `setpgid(0, 0)`, which POSIX lists as async-signal-safe; it neither
        // allocates nor touches shared parent state, so the invariant holds. There
        // is no safe stdlib API for setting the child's process group, so the
        // `unsafe` is irreducible - the same pattern `run_command::spawn` uses.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::from)?;
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|err| format!("could not run hook command: {err}"))?;
        let pid = child.id().map(|p| p as i32);

        // Write the JSON payload to stdin, then close it so a hook reading stdin to
        // EOF unblocks. A broken pipe (a hook that never reads stdin) is not fatal.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_json.as_bytes()).await;
            drop(stdin);
        }

        let wait = child.wait_with_output();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(output)) => Ok(ShellResult {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
            Ok(Err(err)) => Err(format!("hook runner exited: {err}")),
            Err(_elapsed) => {
                // Timeout: signal the whole process group, not just the child.
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                Err(format!("hook command timed out after {timeout_secs}s"))
            }
        }
    }

    #[cfg(not(unix))]
    async fn run(
        &self,
        _command: &str,
        _stdin_json: &str,
        _cwd: &str,
        _env: &HashMap<String, String>,
        _timeout_secs: u64,
    ) -> Result<ShellResult, String> {
        Err("command hooks are only supported on unix".into())
    }
}

/// The production [`HttpPost`] (ADR-0066): POSTs a hook's JSON payload over
/// `reqwest` with the hook's timeout, reading the status + body back. A transport
/// failure is an `Err` the leaf turns into a fail-open non-blocking outcome. The
/// SSRF guard / header interpolation qwen adds are the deferred surface (ADR-0066).
pub struct ReqwestHttp;

#[async_trait::async_trait]
impl HttpPost for ReqwestHttp {
    async fn post(
        &self,
        url: &str,
        json_body: &str,
        timeout_secs: u64,
    ) -> Result<(u16, String), String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| format!("hook http client build failed: {e}"))?;
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(json_body.to_string())
            .send()
            .await
            .map_err(|e| format!("hook http post failed: {e}"))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| format!("hook http body read failed: {e}"))?;
        Ok((status, body))
    }
}

/// The Run's hook firing handle (Phase 3a, ADR-0066): the manager to resolve
/// hooks from plus the assembled capability set + cwd the runner needs. Built
/// once per Run at the wiring layer ([`crate::run::run`]) and carried on the
/// [`crate::run::loop_::RunEnv`], so [`crate::run::batch`] can fire the four tool
/// events without reaching into either the leaf's runner or the host's channels.
///
/// The prompt capability is the captured [`Llm`] + [`Model`] (the Active Model,
/// ADR-0033), reused rather than reinvented. The shell/http capabilities are the
/// production impls above. A [`Hooks`] holds owned production impls so its borrow
/// of the manager is the only lifetime the [`RunEnv`] carries.
pub struct Hooks<'a> {
    manager: &'a HookManager,
    shell: Box<dyn ShellExec>,
    http: Box<dyn HttpPost>,
    llm: &'a dyn Llm,
    model: &'a Model,
    /// The working directory the payload reports and a command hook runs in (the
    /// Session's Project Root).
    cwd: String,
}

/// A hook's decision at the PreToolUse seam, folded from every fired hook's
/// outcome (ADR-0066). The batch acts on exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolDecision {
    /// A hook blocked the call (`decision` block/deny, or a prompt hook's
    /// `ok:false`): the tool does NOT run and `reason` is fed back to the model as
    /// the tool result. Carries any `additionalContext` a blocking hook still
    /// injected.
    Block {
        reason: String,
        context: Option<String>,
    },
    /// No hook blocked: the call proceeds. `permission` is the permission decision
    /// a PreToolUse hook contributed (feeding the Approval seam), `context` is the
    /// `additionalContext` to append to the eventual tool result, and `stop`
    /// carries a `continue:false` halt reason if a hook requested one.
    Proceed {
        permission: Option<PermissionDecision>,
        context: Option<String>,
        stop: Option<String>,
    },
}

/// The permission verdict a PermissionRequest hook produced, composed with the
/// approval mode per ADR-0050's revised section (ADR-0066). The Approval gate acts
/// on exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// A hook allowed: auto-approve this call with no modal (scoped to the call,
    /// not a Standing entry). Overrides the mode's own gate.
    Allow,
    /// A hook denied: reject the call outright with `reason`, the gate never opens.
    /// Overrides even Yolo (an operator-installed guard).
    Deny { reason: String },
    /// No hook decided (or a hook returned `ask`): fall through to the normal gate,
    /// so the mode + any Standing Approval decide.
    Ask,
}

/// A hook's decision at the PostToolUse seam (ADR-0066): the `additionalContext`
/// to append to the successful result the model sees, and whether a hook requested
/// the loop stop (`continue:false`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostToolDecision {
    pub context: Option<String>,
    pub stop: Option<String>,
}

impl<'a> Hooks<'a> {
    /// Builds the Run's firing handle from the manager + the captured Llm/Model +
    /// the Session's Project Root, wiring the production shell/http capabilities.
    pub fn new(manager: &'a HookManager, llm: &'a dyn Llm, model: &'a Model, cwd: String) -> Self {
        Hooks {
            manager,
            shell: Box::new(ProcessGroupShell),
            http: Box::new(ReqwestHttp),
            llm,
            model,
            cwd,
        }
    }

    /// Builds a firing handle over an INJECTED shell/http pair (the test seam):
    /// the same wiring the production [`Hooks::new`] does, but with the caller's
    /// fakes so a test can craft exact [`crate::hooks::HookOutcome`]s. Kept beside
    /// the production constructor so both assemble the [`HookCaps`] identically.
    #[cfg(test)]
    pub fn with_caps(
        manager: &'a HookManager,
        shell: Box<dyn ShellExec>,
        http: Box<dyn HttpPost>,
        llm: &'a dyn Llm,
        model: &'a Model,
        cwd: String,
    ) -> Self {
        Hooks {
            manager,
            shell,
            http,
            llm,
            model,
            cwd,
        }
    }

    // The runner's capability bundle over this handle's owned impls + injected Llm.
    fn caps(&self) -> HookCaps<'_> {
        HookCaps {
            shell: self.shell.as_ref(),
            http: self.http.as_ref(),
            llm: self.llm,
            prompt_model: self.model,
        }
    }

    // Fires every hook for `event`/`tool_name` in source order, yielding each
    // `(outcome, skill_root)` so a caller can fold them. The payload is the qwen
    // event JSON. A firing that resolves to the default (steers-nothing) outcome
    // is fail-open (the runner already turns a spawn/timeout/parse failure into
    // one), so this never errs.
    async fn fire(
        &self,
        event: HookEvent,
        tool_name: &str,
        payload: &str,
    ) -> Vec<crate::hooks::HookOutcome> {
        let selected = self.manager.hooks_for(event, Some(tool_name));
        let caps = self.caps();
        let mut outcomes = Vec::with_capacity(selected.len());
        for sel in selected {
            let ctx = HookRunContext {
                cwd: &self.cwd,
                skill_root: sel.skill_root,
            };
            outcomes.push(run_hook(sel.hook, payload, &ctx, &caps).await);
        }
        outcomes
    }

    /// Fires the PreToolUse hooks and folds them into a [`PreToolDecision`]
    /// (ADR-0066). The FIRST blocking outcome (a `block`/`deny` decision, or a
    /// prompt hook's `ok:false`) blocks the call and its reason is fed back to the
    /// model; otherwise the call proceeds carrying the first permission decision a
    /// hook contributed, the concatenated additionalContext, and the first
    /// `continue:false` stop reason. The model-sent `input` rides in the payload.
    pub async fn pre_tool_use(&self, tool_name: &str, input: &serde_json::Value) -> PreToolDecision {
        let payload = payload_json(HookEvent::PreToolUse, tool_name, &self.cwd, Some(input), None);
        let outcomes = self.fire(HookEvent::PreToolUse, tool_name, &payload).await;

        // A blocking outcome stops the call; its reason (plus any context it still
        // carried) is fed back to the model as the tool result.
        for outcome in &outcomes {
            if outcome.is_blocking() {
                let (_, reason) = outcome.blocking_error();
                return PreToolDecision::Block {
                    reason,
                    context: outcome.additional_context(),
                };
            }
        }

        // No block: fold the permission decision (first wins), the injected
        // context (concatenated), and the halt reason (first wins).
        let permission = outcomes.iter().find_map(|o| o.permission_decision());
        let context = join_context(&outcomes);
        let stop = outcomes
            .iter()
            .find(|o| o.should_stop())
            .map(|o| o.effective_reason());
        PreToolDecision::Proceed {
            permission,
            context,
            stop,
        }
    }

    /// Fires the PermissionRequest hooks and composes their decision with the
    /// approval mode per ADR-0050's revised section (ADR-0066): a `deny` (first
    /// wins) rejects outright and overrides even Yolo; else an `allow` auto-approves
    /// with no modal; else (no decision or `ask`) falls through to the normal gate.
    /// `pre_permission` is the permission decision a PreToolUse hook already
    /// contributed, consulted with the same precedence so a PreToolUse allow/deny
    /// and a PermissionRequest allow/deny compose into one verdict.
    pub async fn permission_request(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        pre_permission: Option<PermissionDecision>,
    ) -> PermissionVerdict {
        let payload = payload_json(
            HookEvent::PermissionRequest,
            tool_name,
            &self.cwd,
            Some(input),
            None,
        );
        let outcomes = self
            .fire(HookEvent::PermissionRequest, tool_name, &payload)
            .await;

        // The PreToolUse permission decision joins the PermissionRequest ones, so
        // one deny/allow precedence spans both events (ADR-0050 revised). A
        // PreToolUse decision carries no reason of its own here (its blocking form
        // was already handled at PreToolUse); a bare deny reads the generic denial.
        let decisions = pre_permission
            .map(|pd| (pd, "Denied by a PreToolUse hook".to_string()))
            .into_iter()
            .chain(outcomes.iter().filter_map(|o| {
                o.permission_decision().map(|pd| {
                    (
                        pd,
                        o.permission_decision_reason()
                            .unwrap_or_else(|| o.effective_reason()),
                    )
                })
            }));
        // Deny overrides everything (even Yolo). Collect decisions with reasons.
        let mut allow = false;
        for (pd, reason) in decisions {
            match pd {
                PermissionDecision::Deny => return PermissionVerdict::Deny { reason },
                PermissionDecision::Allow => allow = true,
                PermissionDecision::Ask => {}
            }
        }
        if allow {
            PermissionVerdict::Allow
        } else {
            PermissionVerdict::Ask
        }
    }

    /// Fires the PostToolUse hooks over a successful result and folds them into a
    /// [`PostToolDecision`] (ADR-0066): the concatenated additionalContext to
    /// append to the result the model sees, and the first `continue:false` stop
    /// reason a hook requested. A PostToolUse hook that returns no decision defaults
    /// to allow, so only its context/stop matter here (the batch does not re-gate a
    /// completed call). `output` is the tool's result text, delivered in the
    /// payload.
    pub async fn post_tool_use(&self, tool_name: &str, output: &str) -> PostToolDecision {
        let payload = payload_json(
            HookEvent::PostToolUse,
            tool_name,
            &self.cwd,
            None,
            Some(output),
        );
        let outcomes = self.fire(HookEvent::PostToolUse, tool_name, &payload).await;
        PostToolDecision {
            context: join_context(&outcomes),
            stop: outcomes
                .iter()
                .find(|o| o.should_stop())
                .map(|o| o.effective_reason()),
        }
    }

    /// Fires the PostToolUseFailure hooks over a failed result: additionalContext
    /// ONLY (a failure hook cannot stop the loop, ADR-0066). Returns the
    /// concatenated context to append to the error result the model sees.
    pub async fn post_tool_use_failure(&self, tool_name: &str, output: &str) -> Option<String> {
        let payload = payload_json(
            HookEvent::PostToolUseFailure,
            tool_name,
            &self.cwd,
            None,
            Some(output),
        );
        let outcomes = self
            .fire(HookEvent::PostToolUseFailure, tool_name, &payload)
            .await;
        join_context(&outcomes)
    }
}

/// The qwen event payload JSON (ADR-0066): the `hook_event_name`, the `cwd`, and -
/// on the tool events - the `tool_name` plus the `tool_input` (Pre/Permission) or
/// `tool_response` (Post). Delivered to a command hook on stdin, POSTed by an http
/// hook, spliced into a prompt hook's template.
fn payload_json(
    event: HookEvent,
    tool_name: &str,
    cwd: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "hook_event_name".to_string(),
        serde_json::Value::String(event.wire_name().to_string()),
    );
    map.insert(
        "cwd".to_string(),
        serde_json::Value::String(cwd.to_string()),
    );
    map.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    if let Some(input) = tool_input {
        map.insert("tool_input".to_string(), input.clone());
    }
    if let Some(output) = tool_output {
        map.insert(
            "tool_response".to_string(),
            serde_json::Value::String(output.to_string()),
        );
    }
    serde_json::Value::Object(map).to_string()
}

/// Concatenates the sanitized `additionalContext` of every outcome that carries
/// one (ADR-0066), newline-joined in source order, or `None` when none did. The
/// escape (`<`/`>` -> entities) already happened in the leaf's accessor.
fn join_context(outcomes: &[crate::hooks::HookOutcome]) -> Option<String> {
    let parts: Vec<String> = outcomes
        .iter()
        .filter_map(|o| o.additional_context())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
#[path = "../../tests/run/hooks.rs"]
mod tests;
