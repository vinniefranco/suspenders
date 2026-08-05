//! [`HookOutcome`] - a hook's parsed JSON decision (ADR-0066), a pure LEAF.
//!
//! A hook is not observability-only: its whole value is that its JSON output can
//! steer the Run (ADR-0066). This module ports qwen's decision protocol
//! (`hooks/types.ts` `HookOutput` + `DefaultHookOutput` and its per-event
//! subclasses) VERBATIM: the field set, the helper methods (is-blocking,
//! should-stop, effective-reason, additional-context escape, blocking-error), and
//! the PreToolUse permission-decision mapping with its base-`decision` fallback.
//!
//! The field NAMES are qwen's wire names (`continue`, `stopReason`,
//! `suppressOutput`, `decision`, `reason`, `hookSpecificOutput` with
//! `additionalContext` / `permissionDecision` / `permissionDecisionReason`), so a
//! hook authored against qwen produces an outcome that parses here unchanged.

use serde::Deserialize;
use serde_json::Value;

/// The `decision` a hook can return (qwen's `HookDecision`): whether a tool call
/// proceeds. `block`/`deny` stop it (feeding `reason` back as the failure),
/// `approve`/`allow` let it through, `ask` routes it to the user (ADR-0066).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Ask,
    Block,
    Deny,
    Approve,
    Allow,
}

/// The permission decision that flows into the ADR-0050 approval flow (qwen's
/// `permissionDecision`): a hook can auto-allow, auto-deny, or force an ask on a
/// call that would otherwise prompt (ADR-0066).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

/// A hook's parsed JSON decision (qwen's `HookOutput`). Every steering field is
/// optional, matching qwen's `Partial<HookOutput>`: a hook that returns `{}` (or
/// nothing) is an all-`None` outcome that steers nothing. The `#[serde]` names are
/// qwen's exact wire keys.
///
/// `hook_specific_output` stays a raw [`Value`] (qwen's
/// `hookSpecificOutput: Record<string, unknown>`), the open edge the typed
/// accessors ([`additional_context`](HookOutcome::additional_context),
/// [`permission_decision`](HookOutcome::permission_decision)) read - so a future
/// per-event field needs no new struct field, only a new accessor.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HookOutcome {
    /// `continue: false` halts the surrounding loop (ADR-0066). `None` / `true`
    /// let it run on.
    #[serde(rename = "continue")]
    pub continue_: Option<bool>,
    /// The explanation surfaced when the loop halts; becomes a Suspenders
    /// `StopReason` in Phase 3.
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<String>,
    /// Keep the hook's stdout/stderr out of the visible transcript.
    #[serde(rename = "suppressOutput")]
    pub suppress_output: Option<bool>,
    /// A system-facing message (qwen's `systemMessage`); set by the plain-text
    /// command-output conversion path.
    #[serde(rename = "systemMessage")]
    pub system_message: Option<String>,
    /// Governs whether a tool call proceeds (ADR-0066).
    pub decision: Option<Decision>,
    /// The reason fed back to the model on a block/deny, or shown to the user.
    pub reason: Option<String>,
    /// The open-edge per-event bag: `additionalContext`, `permissionDecision`,
    /// `permissionDecisionReason`, and any future per-event field.
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<Value>,
    /// NOT part of qwen's wire shape (never parsed): the runner's own note that
    /// the hook could not fire at all - a command spawn failure / timeout, or an
    /// http transport failure - and failed open to this steers-nothing outcome.
    /// The firing layer surfaces it visibly (ADR-0018: a Hook failure never
    /// fails the Run and is recorded visibly in the Transcript).
    #[serde(skip)]
    pub firing_error: Option<String>,
}

impl HookOutcome {
    /// Parses a hook's raw JSON stdout/response body into a [`HookOutcome`]
    /// (qwen's `JSON.parse` into `HookOutput`). Faithful to qwen's command-hook
    /// double-parse: a JSON STRING whose content is itself JSON is unwrapped once
    /// (`JSON.parse(JSON.parse(text))`), so a hook that double-encodes still
    /// yields its object. A non-object, non-string JSON value (a bare number,
    /// array, or the unwrapped-to-non-object case) is an `Err`, as is unparseable
    /// text - the runner turns either into a fail-open non-blocking outcome.
    pub fn parse(text: &str) -> Result<HookOutcome, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("hook output is not JSON: {e}"))?;

        // qwen: a JSON string is re-parsed once, so a double-encoded object still
        // lands as an object.
        let value = match value {
            Value::String(inner) => serde_json::from_str::<Value>(&inner)
                .map_err(|e| format!("hook output string is not JSON: {e}"))?,
            other => other,
        };

        if !value.is_object() {
            return Err("hook output is not a JSON object".to_string());
        }

        serde_json::from_value(value).map_err(|e| format!("hook output shape is invalid: {e}"))
    }

    /// Whether this outcome blocks the operation (qwen's `isBlockingDecision`):
    /// `decision` is `block` or `deny`.
    pub fn is_blocking(&self) -> bool {
        matches!(self.decision, Some(Decision::Block | Decision::Deny))
    }

    /// Whether this outcome requests the loop stop (qwen's
    /// `shouldStopExecution`): `continue: false`.
    pub fn should_stop(&self) -> bool {
        self.continue_ == Some(false)
    }

    /// The effective reason for blocking or stopping (qwen's `getEffectiveReason`):
    /// `stopReason`, else `reason`, else the `"No reason provided"` sentinel.
    pub fn effective_reason(&self) -> String {
        self.stop_reason
            .clone()
            .or_else(|| self.reason.clone())
            .unwrap_or_else(|| "No reason provided".to_string())
    }

    /// The sanitized `additionalContext` to inject as a conversation turn (qwen's
    /// `getAdditionalContext`), or `None` when absent or non-string. Per qwen,
    /// `<`/`>` are escaped to `&lt;`/`&gt;` so a hook cannot smuggle tags into the
    /// transcript (ADR-0066).
    pub fn additional_context(&self) -> Option<String> {
        let raw = self
            .hook_specific_output
            .as_ref()?
            .get("additionalContext")?
            .as_str()?;
        Some(raw.replace('<', "&lt;").replace('>', "&gt;"))
    }

    /// The blocking verdict + reason (qwen's `getBlockingError`): `(true, reason)`
    /// when [`is_blocking`](HookOutcome::is_blocking), else `(false, "")`.
    pub fn blocking_error(&self) -> (bool, String) {
        if self.is_blocking() {
            (true, self.effective_reason())
        } else {
            (false, String::new())
        }
    }

    /// The permission decision for the ADR-0050 approval seam (qwen's
    /// PreToolUse `getPermissionDecision`): reads
    /// `hookSpecificOutput.permissionDecision` first, then falls back to mapping
    /// the base `decision` field - `allow`/`approve` -> allow, `deny`/`block` ->
    /// deny, `ask` -> ask. `None` when neither yields one.
    pub fn permission_decision(&self) -> Option<PermissionDecision> {
        // hookSpecificOutput.permissionDecision wins when a valid one is present.
        if let Some(hso) = self.hook_specific_output.as_ref()
            && let Some(pd) = hso.get("permissionDecision").and_then(Value::as_str)
        {
            match pd {
                "allow" => return Some(PermissionDecision::Allow),
                "deny" => return Some(PermissionDecision::Deny),
                "ask" => return Some(PermissionDecision::Ask),
                // An invalid permissionDecision string falls through to the base
                // `decision` mapping (qwen ignores it and continues).
                _ => {}
            }
        }
        // Fall back to the base `decision` field.
        match self.decision {
            Some(Decision::Allow | Decision::Approve) => Some(PermissionDecision::Allow),
            Some(Decision::Deny | Decision::Block) => Some(PermissionDecision::Deny),
            Some(Decision::Ask) => Some(PermissionDecision::Ask),
            None => None,
        }
    }

    /// The permission-decision reason (qwen's PreToolUse
    /// `getPermissionDecisionReason`): `hookSpecificOutput.permissionDecisionReason`
    /// when a string, else the base `reason` field.
    pub fn permission_decision_reason(&self) -> Option<String> {
        if let Some(hso) = self.hook_specific_output.as_ref()
            && let Some(r) = hso.get("permissionDecisionReason").and_then(Value::as_str)
        {
            return Some(r.to_string());
        }
        self.reason.clone()
    }

    /// The Stop-hook feedback string (qwen's `StopHookOutput.getStopReason`):
    /// `stopReason` wrapped as `"Stop hook feedback:\n<reason>"`, or `None` when
    /// no `stopReason` is set. The Stop / SubagentStop surfacing (ADR-0066).
    pub fn stop_hook_feedback(&self) -> Option<String> {
        self.stop_reason
            .as_ref()
            .map(|r| format!("Stop hook feedback:\n{r}"))
    }
}

#[cfg(test)]
#[path = "../../tests/hooks/outcome.rs"]
mod tests;
