//! [`HookEvent`] - the sixteen lifecycle events a hook can fire on (ADR-0066),
//! a faithful port of qwen v0.16.0's `HookEventName` (`hooks/types.ts`).
//!
//! Each variant serde-maps to qwen's exact wire name (the PascalCase string that
//! appears as a `hooks` key in `config.json` and a `SKILL.md` frontmatter
//! `hooks:` block, and as the `hook_event_name` in the JSON payload delivered to
//! a hook). This is a pure LEAF: `serde` only, no upward deps.

use serde::{Deserialize, Serialize};

/// The lifecycle event a hook is fired on. Ported VERBATIM from qwen's
/// `HookEventName` (`hooks/types.ts`) - all sixteen variants, each serialized to
/// qwen's exact PascalCase wire name so a `config.json` or `SKILL.md` authored
/// against qwen loads unchanged, and the payload's `hook_event_name` matches.
///
/// The events group by their anchor in the Suspenders loop (ADR-0066): the
/// tool-dispatch seam ([`PreToolUse`](HookEvent::PreToolUse) /
/// [`PostToolUse`](HookEvent::PostToolUse) /
/// [`PostToolUseFailure`](HookEvent::PostToolUseFailure) /
/// [`PermissionRequest`](HookEvent::PermissionRequest)), the Run boundary
/// ([`Stop`](HookEvent::Stop) / [`StopFailure`](HookEvent::StopFailure)), the
/// session boundary, compaction, the todo tool, and the subagent boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Before a tool runs (the tool-dispatch seam): a guard or formatter site.
    PreToolUse,
    /// After a tool succeeds: an audit sink or context-injecting site.
    PostToolUse,
    /// After a tool execution fails.
    PostToolUseFailure,
    /// When the user submits from the composer, before the prompt reaches the
    /// model: a hook can inject context or veto.
    UserPromptSubmit,
    /// At session startup / resume / clear.
    SessionStart,
    /// When a session is ending (clear / logout / exit).
    SessionEnd,
    /// As a Run concludes cleanly.
    Stop,
    /// When the turn ends due to an API error (instead of [`Stop`](HookEvent::Stop)).
    StopFailure,
    /// When a subagent Run (ADR-0061) is spawned.
    SubagentStart,
    /// When a subagent Run concludes.
    SubagentStop,
    /// When a terminal notification is sent.
    Notification,
    /// When an approval would be shown (feeds the ADR-0050 approval flow).
    PermissionRequest,
    /// Before conversation compaction (ADR-0012).
    PreCompact,
    /// After conversation compaction (observe-only, matching qwen).
    PostCompact,
    /// When a new todo item is added (ADR-0048).
    TodoCreated,
    /// When a todo item's status changes to completed (ADR-0048).
    TodoCompleted,
}

impl HookEvent {
    /// The exact qwen wire name (the `config.json` / `SKILL.md` key, and the
    /// payload's `hook_event_name`). Single-sourced from the serde mapping so the
    /// string form and the parse cannot drift.
    pub fn wire_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::Notification => "Notification",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::TodoCreated => "TodoCreated",
            HookEvent::TodoCompleted => "TodoCompleted",
        }
    }

    /// Parses a wire name (a `config.json` / `SKILL.md` `hooks` key) into a
    /// [`HookEvent`], or `None` for an unrecognized key. Config parsing uses this
    /// to skip qwen's non-event `hooks` fields (`enabled`/`disabled`/
    /// `notifications`) and to record an unknown event name as a fail-open
    /// failure rather than crashing (ADR-0066).
    pub fn parse(name: &str) -> Option<HookEvent> {
        // The serde untagged map from the wire string is the single source; a
        // manual match here would be a second mapping that could drift. Deserialize
        // the name as a JSON string into the enum.
        serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
    }

    /// Whether this event scopes its hooks by a tool-name matcher (ADR-0066). A
    /// matcher is meaningful only on the tool-dispatch events; on every other
    /// event a matcher is inert (a present matcher matches all). Mirrors qwen's
    /// `getHookMatcherTarget` tool-kind arm.
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionRequest
        )
    }
}

#[cfg(test)]
#[path = "../../tests/hooks/event.rs"]
mod tests;
