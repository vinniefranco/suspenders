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
//! ## The events wired through this facade
//!
//! Phase 3a wired the tool-dispatch + permission events: [`Hooks::pre_tool_use`],
//! [`Hooks::permission_request`], [`Hooks::post_tool_use`],
//! [`Hooks::post_tool_use_failure`]. Each fires every [`HookManager::hooks_for`]
//! hook for its event through [`run_hook`] and folds the outcomes into the one
//! decision the batch acts on (block / permission / injected context / stop).
//!
//! Phase 3b adds the twelve lifecycle events through the SAME facade (fired off a
//! `None` tool name, [`Hooks::fire_lifecycle`]): the DECIDING ones -
//! [`Hooks::user_prompt_submit`] (veto / inject a prompt) and [`Hooks::stop`]
//! (qwen's Stop-hook feedback INVERSION - a block FORCES the Run to continue) -
//! and the observational ones ([`Hooks::stop_failure`], [`Hooks::session_start`]
//! (which injects initial context), [`Hooks::session_end`],
//! [`Hooks::pre_compact`] (which injects a compaction instruction),
//! [`Hooks::post_compact`], [`Hooks::todo_created`], [`Hooks::todo_completed`],
//! [`Hooks::subagent_start`], [`Hooks::subagent_stop`], [`Hooks::notification`]).
//! The Run-layer events fire from the loop; the Agent builds this SAME facade to
//! fire the session/notification events (agent.rs). A firing error is fail-open
//! (ADR-0018): the surrounding operation proceeds as if no hook fired, and every
//! deciding fire is surfaced visibly.

use std::collections::HashMap;

use crate::hooks::{
    HookCaps, HookEvent, HookManager, HookOutcome, HookRunContext, HttpPost, PermissionDecision,
    ShellExec, ShellResult, run_hook,
};
use crate::llm::Llm;
use crate::llm::model::Model;

/// qwen's `DEFAULT_STOP_HOOK_BLOCK_CAP` (stopHookCap.ts): a Stop hook may force at
/// most this many consecutive continuations before the Run ends regardless (A2).
pub const DEFAULT_STOP_HOOK_BLOCK_CAP: u64 = 8;

/// qwen's `MAX_STOP_HOOK_BLOCK_CAP` (stopHookCap.ts): the resolved cap is clamped
/// to this ceiling so an env override cannot ask for an unbounded loop (A2).
pub const MAX_STOP_HOOK_BLOCK_CAP: u64 = 100;

/// The env var that overrides the Stop-hook cap (A2, qwen's
/// `QWEN_CODE_STOP_HOOK_BLOCK_CAP`, adapted to the product name).
pub const STOP_HOOK_BLOCK_CAP_ENV: &str = "SUSPENDERS_STOP_HOOK_BLOCK_CAP";

/// Resolves the Stop-hook continuation cap (A2, qwen's `resolveStopHookBlockingCap`
/// and `normalizeStopHookBlockingCap`): the `SUSPENDERS_STOP_HOOK_BLOCK_CAP` env
/// value when set to a parseable positive integer (clamped to the `MAX` ceiling),
/// else the default 8. A non-numeric, empty, or non-positive value falls back to the
/// default, matching qwen's normalizer.
pub fn resolve_stop_hook_cap() -> u64 {
    match std::env::var(STOP_HOOK_BLOCK_CAP_ENV) {
        Ok(raw) if !raw.trim().is_empty() => {
            normalize_stop_hook_cap(raw.trim().parse::<i64>().ok())
        }
        _ => DEFAULT_STOP_HOOK_BLOCK_CAP,
    }
}

/// qwen's `normalizeStopHookBlockingCap`: a finite integer `>= 1` is clamped to the
/// `MAX` ceiling; anything else (missing, `< 1`) is the default 8 (A2).
fn normalize_stop_hook_cap(value: Option<i64>) -> u64 {
    match value {
        Some(n) if n >= 1 => (n as u64).min(MAX_STOP_HOOK_BLOCK_CAP),
        _ => DEFAULT_STOP_HOOK_BLOCK_CAP,
    }
}

/// Derives the hook payload's `session_id` from the Session Log's JSONL path (H1,
/// ADR-0010): the file stem (`<utc-stamp>-<unique>`) is the unique per-session token
/// suspenders mints - the closest analog to qwen's `getSessionId()`. Empty when no
/// log opened (a test Run, or a log-open failure), matching qwen's empty-when-absent.
pub fn session_id_from_log_path(transcript_path: &str) -> String {
    if transcript_path.is_empty() {
        return String::new();
    }
    std::path::Path::new(transcript_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// qwen's `formatStopHookBlockingCapWarning` for the Stop hook (A2): the user-facing
/// line emitted when the cap is hit and the Run ends despite a still-blocking Stop
/// hook. Verbatim wording, with the correct singular/plural of "time(s)".
pub fn format_stop_hook_cap_warning(cap: u64) -> String {
    let times = if cap == 1 { "time" } else { "times" };
    format!(
        "Stop hook blocked continuation {cap} consecutive {times}; overriding and ending the turn."
    )
}

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

        // The stdin write must run CONCURRENTLY with draining stdout/stderr (C-F1):
        // a PostToolUse payload embeds the full tool output and can exceed the pipe
        // buffer (~64KB), so a hook that echoes stdin to stdout deadlocks if we write
        // the whole payload before starting to read (the child blocks on a full
        // stdout pipe while we block on a full stdin pipe). Spawning the write as a
        // task lets `wait_with_output` drain both pipes while the payload streams in;
        // the task drops stdin at the end so a hook reading to EOF unblocks. A broken
        // pipe (a hook that never reads stdin) is swallowed - it is not fatal.
        let stdin = child.stdin.take();
        let payload = stdin_json.as_bytes().to_vec();
        let writer = tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(&payload).await;
                // Explicit drop closes the pipe (EOF) before the task ends.
                drop(stdin);
            }
        });

        // Wrap the ENTIRE write + wait in ONE timeout (C-F1): a stuck write (a hook
        // that fills stdout without reading stdin, so the write never completes) is
        // now inside the timeout, so it is killpg'd on elapse instead of hanging
        // run() forever. Draining stdout/stderr concurrently means the write always
        // makes progress once the child reads, so the normal path never trips it.
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let combined = async {
            let output = child.wait_with_output().await;
            // The writer has finished (stdin drained) by the time the child exits;
            // join it so no task is left detached. Its result is ignored (a broken
            // pipe is expected for a hook that ignores stdin).
            let _ = writer.await;
            output
        };
        match tokio::time::timeout(timeout, combined).await {
            Ok(Ok(output)) => Ok(ShellResult {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
            Ok(Err(err)) => Err(format!("hook runner exited: {err}")),
            Err(_elapsed) => {
                // Timeout: signal the whole process group, not just the child. This
                // now also fires for a stuck stdin write (it is inside the timeout).
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
    /// The Session identifier every payload carries (H1, qwen's
    /// `createBaseInput.session_id` from `config.getSessionId()`). Sourced from the
    /// Session Log's per-session file stem (ADR-0010) - the unique token that names
    /// this Session's JSONL - so a hook keys the same identity across events. Empty
    /// when no log opened (a test Run, or a log-open failure).
    session_id: String,
    /// The Session Log's JSONL path every payload carries (H1, qwen's
    /// `createBaseInput.transcript_path` from `config.getTranscriptPath()`), so a
    /// hook can tail the running transcript (ADR-0010). Empty when no log opened.
    transcript_path: String,
}

/// The five base fields every hook payload carries (H1, qwen's `createBaseInput`):
/// `session_id`, `transcript_path`, `cwd`, `hook_event_name`, and a `timestamp`.
/// Built once per fire from the [`Hooks`] handle + the event, then merged into the
/// per-event map by [`payload_json`] / [`lifecycle_payload`]. qwen's `timestamp` is
/// an ISO-8601 string (`new Date().toISOString()`), so this stamps the same shape.
struct BaseInput<'a> {
    session_id: &'a str,
    transcript_path: &'a str,
    cwd: &'a str,
    event: HookEvent,
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
        /// A hook's visible `systemMessage` (A5), gated on `suppressOutput`.
        system_message: Option<String>,
    },
    /// No hook blocked: the call proceeds. `permission` is the permission decision
    /// a PreToolUse hook contributed (feeding the Approval seam), `context` is the
    /// `additionalContext` to append to the eventual tool result, and `stop`
    /// carries a `continue:false` halt reason if a hook requested one.
    Proceed {
        permission: Option<PermissionDecision>,
        context: Option<String>,
        stop: Option<String>,
        /// A hook's visible `systemMessage` (A5), gated on `suppressOutput`.
        system_message: Option<String>,
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
    /// A hook's visible `systemMessage` (A5), gated on `suppressOutput`.
    pub system_message: Option<String>,
}

/// A hook's decision at the UserPromptSubmit seam (Phase 3b, ADR-0066): fired
/// before the prompt reaches the model. The FIRST blocking outcome vetoes the
/// prompt (it never reaches the model, and `reason` is surfaced); otherwise the
/// prompt proceeds carrying the concatenated `additionalContext` a hook injected
/// as a leading conversation turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPromptDecision {
    /// A hook blocked the prompt: it does NOT reach the model; `reason` is
    /// surfaced (and any `additionalContext` the blocking hook still carried).
    Reject {
        reason: String,
        context: Option<String>,
    },
    /// No hook blocked: the prompt proceeds. `context` is the injected
    /// additionalContext (concatenated) to hand the model ahead of the prompt.
    Proceed { context: Option<String> },
}

/// A hook's decision at the Stop seam (Phase 3b, ADR-0066): fired when the model
/// would end its Run. Faithful to qwen's Stop-hook feedback INVERSION: a Stop
/// hook that blocks (`continue:false`) FORCES the Run to continue rather than
/// stop, feeding its reason back as guidance. `Stop` lets the Run end; `Continue`
/// carries the "Stop hook feedback:\n<reason>" the loop injects to keep going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// No Stop hook forced a continuation: the Run ends as it would have.
    Stop,
    /// A Stop hook blocked the end (`continue:false`): the Run must continue, and
    /// `feedback` is the "Stop hook feedback:\n<reason>" injected as guidance.
    Continue { feedback: String },
}

impl<'a> Hooks<'a> {
    /// Builds the Run's firing handle from the manager + the captured Llm/Model +
    /// the Session's Project Root, wiring the production shell/http capabilities.
    pub fn new(
        manager: &'a HookManager,
        llm: &'a dyn Llm,
        model: &'a Model,
        cwd: String,
        session_id: String,
        transcript_path: String,
    ) -> Self {
        Hooks {
            manager,
            shell: Box::new(ProcessGroupShell),
            http: Box::new(ReqwestHttp),
            llm,
            model,
            cwd,
            session_id,
            transcript_path,
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
            // A test Run carries synthetic base identity so a payload assertion has
            // stable values to check (H1); production sources these from the log.
            session_id: "test-session".to_string(),
            transcript_path: "/tmp/test-session.jsonl".to_string(),
        }
    }

    // The five base fields (H1) this handle stamps on every payload for `event`.
    fn base(&self, event: HookEvent) -> BaseInput<'_> {
        BaseInput {
            session_id: &self.session_id,
            transcript_path: &self.transcript_path,
            cwd: &self.cwd,
            event,
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
        for sel in &selected {
            let ctx = HookRunContext {
                cwd: &self.cwd,
                skill_root: sel.skill_root.as_deref(),
            };
            outcomes.push(run_hook(&sel.hook, payload, &ctx, &caps).await);
        }
        outcomes
    }

    /// Registers a skill's session-scoped hooks into the manager (Phase 4c,
    /// ADR-0066): delegates to [`HookManager::register_skill`] through the handle's
    /// shared manager, so the run layer (`crate::run::batch`) can register on skill
    /// invocation without depending on the hook subsystem directly. Idempotent (a
    /// skill invoked twice registers once). `skill_root` is the skill's base
    /// directory, carried so a registered command hook sees `SUSPENDERS_SKILL_ROOT`
    /// when it fires.
    pub fn register_skill(&self, name: &str, skill_root: &str, hooks: &serde_json::Value) {
        self.manager.register_skill(name, skill_root, hooks);
    }

    /// Fires the PreToolUse hooks and folds them into a [`PreToolDecision`]
    /// (ADR-0066). The FIRST blocking outcome (a `block`/`deny` decision, or a
    /// prompt hook's `ok:false`) blocks the call and its reason is fed back to the
    /// model; otherwise the call proceeds carrying the first permission decision a
    /// hook contributed, the concatenated additionalContext, and the first
    /// `continue:false` stop reason. The model-sent `input` rides in the payload.
    pub async fn pre_tool_use(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> PreToolDecision {
        let payload = payload_json(
            self.base(HookEvent::PreToolUse),
            tool_name,
            Some(input),
            None,
        );
        let outcomes = self.fire(HookEvent::PreToolUse, tool_name, &payload).await;

        // The permission decision folded across every hook (deny wins, else ask,
        // else allow), matching qwen's toolHookTriggers precedence
        // (`isDenied()`/`isAsk()`). This drives the UNIVERSAL block: qwen blocks on
        // an `isDenied()` regardless of tool gating (a PreToolUse permissionDecision
        // `deny` on an ungated tool must still stop the call), so a folded Deny is a
        // Block here, not merely a permission hint the gated path might drop.
        let permission = fold_permission(&outcomes);

        // A blocking outcome stops the call universally. qwen mergeWithOrLogic joins
        // ALL matched hooks' reasons + additionalContexts and blocks if ANY blocks
        // (A4), so the fold spans every outcome, not just the first blocker. A base
        // `decision` block/deny OR a folded permission Deny (A1) both block.
        let blocks = outcomes.iter().any(|o| o.is_blocking())
            || permission == Some(PermissionDecision::Deny);
        let system_message = fold_system_message(&outcomes);
        if blocks {
            return PreToolDecision::Block {
                reason: fold_block_reason(&outcomes, permission),
                context: join_context(&outcomes),
                system_message,
            };
        }

        // No block: proceed carrying the folded permission (an `Ask` forces the
        // confirmation gate even on an ungated tool, qwen's `isAsk()`), the injected
        // context (concatenated), and the halt reason (first wins).
        let context = join_context(&outcomes);
        let stop = outcomes
            .iter()
            .find(|o| o.should_stop())
            .map(|o| o.effective_reason());
        PreToolDecision::Proceed {
            permission,
            context,
            stop,
            system_message,
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
            self.base(HookEvent::PermissionRequest),
            tool_name,
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
            self.base(HookEvent::PostToolUse),
            tool_name,
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
            system_message: fold_system_message(&outcomes),
        }
    }

    /// Fires the PostToolUseFailure hooks over a failed result: additionalContext
    /// ONLY (a failure hook cannot stop the loop, ADR-0066). Returns the
    /// concatenated context to append to the error result the model sees.
    pub async fn post_tool_use_failure(&self, tool_name: &str, output: &str) -> Option<String> {
        let payload = payload_json(
            self.base(HookEvent::PostToolUseFailure),
            tool_name,
            None,
            Some(output),
        );
        let outcomes = self
            .fire(HookEvent::PostToolUseFailure, tool_name, &payload)
            .await;
        join_context(&outcomes)
    }

    // Fires every hook for a NON-tool `event` (no matcher, no tool name), yielding
    // each outcome to fold. Mirrors [`fire`] but passes `None` as the tool name so
    // `hooks_for` selects every hook for the event (a matcher is inert off a tool
    // event, ADR-0066). The lifecycle events (Phase 3b) fire through this.
    async fn fire_lifecycle(
        &self,
        event: HookEvent,
        payload: &str,
    ) -> Vec<crate::hooks::HookOutcome> {
        let selected = self.manager.hooks_for(event, None);
        let caps = self.caps();
        let mut outcomes = Vec::with_capacity(selected.len());
        for sel in &selected {
            let ctx = HookRunContext {
                cwd: &self.cwd,
                skill_root: sel.skill_root.as_deref(),
            };
            outcomes.push(run_hook(&sel.hook, payload, &ctx, &caps).await);
        }
        outcomes
    }

    /// Fires the UserPromptSubmit hooks and folds them into a
    /// [`UserPromptDecision`] (Phase 3b, ADR-0066): fired before the prompt reaches
    /// the model. The FIRST blocking outcome vetoes the prompt; otherwise it
    /// proceeds carrying the concatenated `additionalContext` a hook injected. The
    /// prompt text rides the payload's `prompt` field (qwen's UserPromptSubmit
    /// payload key).
    pub async fn user_prompt_submit(&self, prompt: &str) -> UserPromptDecision {
        let payload = lifecycle_payload(
            self.base(HookEvent::UserPromptSubmit),
            &[("prompt", prompt)],
        );
        let outcomes = self
            .fire_lifecycle(HookEvent::UserPromptSubmit, &payload)
            .await;

        for outcome in &outcomes {
            if outcome.is_blocking() {
                let (_, reason) = outcome.blocking_error();
                return UserPromptDecision::Reject {
                    reason,
                    context: outcome.additional_context(),
                };
            }
        }
        UserPromptDecision::Proceed {
            context: join_context(&outcomes),
        }
    }

    /// Fires the Stop hooks and folds them into a [`StopDecision`] (Phase 3b,
    /// ADR-0066), porting qwen's Stop-hook feedback INVERSION: a Stop hook that
    /// blocks (`continue:false` or a `block`/`deny` decision) FORCES the Run to
    /// continue, feeding its `stopReason` back as "Stop hook feedback:\n<reason>".
    /// The payload's `stop_hook_active` is HARDCODED `true` (A3, qwen client.ts:1720
    /// sends `stop_hook_active: true` on every Stop fire), so a Stop hook always sees
    /// the "you are already in a stop-hook loop" flag - the iteration bound is the
    /// CALLER's counter+cap (A2, `dispatch::finish_or_stop_hook`), not this flag. The
    /// FIRST forcing outcome wins.
    pub async fn stop(&self) -> StopDecision {
        let payload =
            lifecycle_payload(self.base(HookEvent::Stop), &[("stop_hook_active", "true")]);
        let outcomes = self.fire_lifecycle(HookEvent::Stop, &payload).await;

        // A Stop hook forces continuation when it halts (continue:false) or blocks:
        // the INVERSION - the "block" means "do not stop". Its feedback is the
        // qwen-wrapped stopReason.
        for outcome in &outcomes {
            if outcome.should_stop() || outcome.is_blocking() {
                let feedback = outcome.stop_hook_feedback().unwrap_or_else(|| {
                    format!("Stop hook feedback:\n{}", outcome.effective_reason())
                });
                return StopDecision::Continue { feedback };
            }
        }
        StopDecision::Stop
    }

    /// Fires the StopFailure hooks (Phase 3b, ADR-0066): observational, fired when
    /// a Run ends due to an API error instead of a clean Stop. The `error` rides the
    /// payload; a StopFailure hook cannot steer (it fires on an already-failed Run),
    /// so this returns nothing.
    pub async fn stop_failure(&self, error: &str) {
        let payload = lifecycle_payload(self.base(HookEvent::StopFailure), &[("error", error)]);
        let _ = self.fire_lifecycle(HookEvent::StopFailure, &payload).await;
    }

    /// Fires the SessionStart hooks and returns the injected initial context
    /// (Phase 3b, ADR-0066): fired once at Agent launch after subsystem discovery.
    /// `source` names the launch kind (qwen's `startup`/`resume`/`clear`). The
    /// concatenated `additionalContext` is injected as initial context; a
    /// SessionStart hook is otherwise observational (it cannot veto a session).
    pub async fn session_start(&self, source: &str) -> Option<String> {
        let payload = lifecycle_payload(self.base(HookEvent::SessionStart), &[("source", source)]);
        let outcomes = self.fire_lifecycle(HookEvent::SessionStart, &payload).await;
        join_context(&outcomes)
    }

    /// Fires the SessionEnd hooks (Phase 3b, ADR-0066): observational, fired once at
    /// session shutdown. `reason` names the end kind (qwen's `clear`/`logout`/
    /// `exit`).
    pub async fn session_end(&self, reason: &str) {
        let payload = lifecycle_payload(self.base(HookEvent::SessionEnd), &[("reason", reason)]);
        let _ = self.fire_lifecycle(HookEvent::SessionEnd, &payload).await;
    }

    /// Fires the PreCompact hooks and returns any injected custom instruction
    /// (Phase 3b, ADR-0066): fired before the compaction service runs, so a hook can
    /// adjust the summarization. The concatenated `additionalContext` is the
    /// instruction to fold in; `None` when no hook contributed one.
    pub async fn pre_compact(&self) -> Option<String> {
        let payload = lifecycle_payload(self.base(HookEvent::PreCompact), &[]);
        let outcomes = self.fire_lifecycle(HookEvent::PreCompact, &payload).await;
        join_context(&outcomes)
    }

    /// Fires the PostCompact hooks (Phase 3b, ADR-0066): observe-only, matching qwen
    /// (its returned JSON produces no control effect). Fired after the compaction
    /// service produces its summary.
    pub async fn post_compact(&self) {
        let payload = lifecycle_payload(self.base(HookEvent::PostCompact), &[]);
        let _ = self.fire_lifecycle(HookEvent::PostCompact, &payload).await;
    }

    /// Fires the TodoCreated hooks (Phase 3b, ADR-0066): observational, fired from
    /// the RUN layer when a `todo_write` adds a new todo item. The `content` of the
    /// created item rides the payload.
    pub async fn todo_created(&self, content: &str) {
        let payload = lifecycle_payload(self.base(HookEvent::TodoCreated), &[("content", content)]);
        let _ = self.fire_lifecycle(HookEvent::TodoCreated, &payload).await;
    }

    /// Fires the TodoCompleted hooks (Phase 3b, ADR-0066): observational, fired from
    /// the RUN layer when a `todo_write` marks a todo item completed. The `content`
    /// of the completed item rides the payload.
    pub async fn todo_completed(&self, content: &str) {
        let payload =
            lifecycle_payload(self.base(HookEvent::TodoCompleted), &[("content", content)]);
        let _ = self
            .fire_lifecycle(HookEvent::TodoCompleted, &payload)
            .await;
    }

    /// Fires the SubagentStart hooks (Phase 3b, ADR-0066): observational, fired at
    /// the PARENT run layer when the `agent` tool spawns a child Run. The
    /// `subagent_type` rides the payload.
    pub async fn subagent_start(&self, subagent_type: &str) {
        let payload = lifecycle_payload(
            self.base(HookEvent::SubagentStart),
            &[("subagent_type", subagent_type)],
        );
        let _ = self
            .fire_lifecycle(HookEvent::SubagentStart, &payload)
            .await;
    }

    /// Fires the SubagentStop hooks (Phase 3b, ADR-0066): observational, fired at
    /// the PARENT run layer when the spawned child Run concludes. The
    /// `subagent_type` rides the payload (qwen's SubagentStop is the child-Run
    /// analog of Stop, but the parent's Stop force-continue does not apply to a
    /// completed subagent, so this stays observational).
    pub async fn subagent_stop(&self, subagent_type: &str) {
        let payload = lifecycle_payload(
            self.base(HookEvent::SubagentStop),
            &[("subagent_type", subagent_type)],
        );
        let _ = self.fire_lifecycle(HookEvent::SubagentStop, &payload).await;
    }

    /// Fires the Notification hooks (Phase 3b, ADR-0066): observational, fired at
    /// the terminal-notification point (the "agent is waiting" alert). The `message`
    /// rides the payload.
    pub async fn notification(&self, message: &str) {
        let payload =
            lifecycle_payload(self.base(HookEvent::Notification), &[("message", message)]);
        let _ = self.fire_lifecycle(HookEvent::Notification, &payload).await;
    }
}

/// The qwen event payload JSON (ADR-0066): the `hook_event_name`, the `cwd`, and -
/// on the tool events - the `tool_name` plus the `tool_input` (Pre/Permission) or
/// `tool_response` (Post). Delivered to a command hook on stdin, POSTed by an http
/// hook, spliced into a prompt hook's template.
fn payload_json(
    base: BaseInput<'_>,
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
) -> String {
    let mut map = base_map(&base);
    map.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    if let Some(input) = tool_input {
        map.insert("tool_input".to_string(), input.clone());
    }
    if let Some(output) = tool_output {
        map.insert("tool_response".to_string(), tool_response_value(output));
    }
    serde_json::Value::Object(map).to_string()
}

/// The base map every payload starts from (H1, qwen's `createBaseInput`):
/// `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `timestamp`. qwen
/// ALWAYS includes all five (types.ts marks them required), so a hook authored
/// against qwen reads the same fields here.
fn base_map(base: &BaseInput<'_>) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "session_id".to_string(),
        serde_json::Value::String(base.session_id.to_string()),
    );
    map.insert(
        "transcript_path".to_string(),
        serde_json::Value::String(base.transcript_path.to_string()),
    );
    map.insert(
        "cwd".to_string(),
        serde_json::Value::String(base.cwd.to_string()),
    );
    map.insert(
        "hook_event_name".to_string(),
        serde_json::Value::String(base.event.wire_name().to_string()),
    );
    map.insert(
        "timestamp".to_string(),
        serde_json::Value::String(iso8601_now()),
    );
    map
}

/// The `tool_response` field for a PostToolUse payload as a JSON OBJECT (H2, qwen's
/// `PostToolUseInput.tool_response: Record<string, unknown>`). A qwen PostToolUse
/// hook indexes `tool_response` as an object (e.g. `tool_response.output`), so a
/// tool output that already IS a JSON object rides through directly, and a plain
/// string is wrapped as `{"output": <string>}` - the stable key a hook can read for
/// either shape. A non-object JSON value (a bare array/number) is also wrapped, so
/// the field is invariably an object.
fn tool_response_value(output: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(output) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => serde_json::json!({ "output": output }),
    }
}

/// An ISO-8601 UTC timestamp (H1, qwen's `new Date().toISOString()`), the shape a
/// qwen hook expects in `timestamp`. Falls back to the epoch on the (unreachable)
/// format error so a payload always carries a well-formed stamp.
fn iso8601_now() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// The qwen event payload JSON for a NON-tool lifecycle event (Phase 3b,
/// ADR-0066): the `hook_event_name` and `cwd` every payload carries, plus each
/// event's own string fields (`prompt` for UserPromptSubmit, `source` for
/// SessionStart, `stop_hook_active` for Stop, and so on). Kept beside
/// [`payload_json`] so the two payload shapes read together; the lifecycle events
/// carry no `tool_name`/`tool_input`/`tool_response`, so they build the smaller
/// map here rather than threading `None`s through the tool builder.
fn lifecycle_payload(base: BaseInput<'_>, fields: &[(&str, &str)]) -> String {
    let mut map = base_map(&base);
    for (key, value) in fields {
        map.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    serde_json::Value::Object(map).to_string()
}

/// Concatenates the sanitized `additionalContext` of every outcome that carries
/// one (ADR-0066), newline-joined in source order, or `None` when none did. The
/// escape (`<`/`>` -> entities) already happened in the leaf's accessor.
fn join_context(outcomes: &[HookOutcome]) -> Option<String> {
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

/// The visible `systemMessage` folded across a fired event's outcomes (A5, qwen's
/// `processCommonHookOutputFields`): a hook's `systemMessage` is surfaced to the
/// user UNLESS that same outcome set `suppressOutput: true` (the gate). qwen's
/// merge keeps the LAST systemMessage across hooks, so this returns the last
/// non-suppressed one, or `None` when none qualified.
fn fold_system_message(outcomes: &[HookOutcome]) -> Option<String> {
    outcomes
        .iter()
        .filter(|o| o.suppress_output != Some(true))
        .filter_map(|o| o.system_message.clone())
        .next_back()
}

/// The permission decision folded across every PreToolUse outcome (A1, qwen's
/// toolHookTriggers `isDenied()`/`isAsk()` precedence): a `deny` from ANY hook
/// wins (most restrictive), else an `ask` forces the confirmation gate, else an
/// `allow` auto-approves. `None` when no hook contributed a permission decision.
/// Folds the whole batch (not first-wins) so a later hook's deny is never dropped.
fn fold_permission(outcomes: &[HookOutcome]) -> Option<PermissionDecision> {
    let mut result: Option<PermissionDecision> = None;
    for pd in outcomes.iter().filter_map(|o| o.permission_decision()) {
        result = Some(match (result, pd) {
            // A deny is absolute (security wins), regardless of order.
            (_, PermissionDecision::Deny) | (Some(PermissionDecision::Deny), _) => {
                PermissionDecision::Deny
            }
            // An ask outranks an allow (a confirmation is more restrictive).
            (_, PermissionDecision::Ask) | (Some(PermissionDecision::Ask), _) => {
                PermissionDecision::Ask
            }
            (_, PermissionDecision::Allow) => PermissionDecision::Allow,
        });
    }
    result
}

/// The reason for a folded PreToolUse block (A1 + A4, qwen mergeWithOrLogic joins
/// ALL reasons with "\n"): every blocking/deny outcome's reason, newline-joined in
/// source order. A permission-deny with no reason of its own reads its
/// `permissionDecisionReason`, then `reason`; a base block/deny reads its effective
/// reason. Falls back to the leaf's "No reason provided" sentinel when none carried
/// one, so the model always gets a reason.
fn fold_block_reason(outcomes: &[HookOutcome], permission: Option<PermissionDecision>) -> String {
    let mut reasons: Vec<String> = Vec::new();
    for o in outcomes {
        if o.is_blocking() {
            reasons.push(o.blocking_error().1);
        } else if permission == Some(PermissionDecision::Deny)
            && o.permission_decision() == Some(PermissionDecision::Deny)
        {
            reasons.push(
                o.permission_decision_reason()
                    .unwrap_or_else(|| o.effective_reason()),
            );
        }
    }
    if reasons.is_empty() {
        "No reason provided".to_string()
    } else {
        reasons.join("\n")
    }
}

#[cfg(test)]
#[path = "../../tests/run/hooks.rs"]
mod tests;
