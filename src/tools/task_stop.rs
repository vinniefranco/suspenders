//! `task_stop(task_id)`: stops a background subagent by its task id (faithful to
//! qwen v0.16.0's `tools/task-stop.ts`, ADR-0063).
//!
//! The model reaches for this to cancel a background agent or shell it launched
//! (via `agent` with `run_in_background: true`, or `run_shell_command` with
//! `is_background: true`) - the id came back in the launch string or a
//! `<task-notification>`. The tool reaches the host through the
//! [`SubagentSpawner`](crate::tool::caps::SubagentSpawner) capability's
//! `stop_background`, which relays to the Agent that owns the background
//! registries. The Agent resolves the id across BOTH registries (ADR-0064): it
//! tries the subagent registry, then falls through to the background-shell
//! registry, aborting the running child/process, queuing the `was cancelled`
//! notification, and returning the VERBATIM qwen wording. Suspenders ports the
//! AGENT and SHELL registry legs of qwen's `task_stop` (the monitor/memory
//! registries are not ported), so the three outcomes are the running stop
//! confirmation, the not-running error, and the not-found error - all VERBATIM.
//!
//! Deferred (`should_defer: true`, qwen's `shouldDefer` - stopping tasks is
//! infrequent), so the model discovers it via `tool_search`; excluded from every
//! subagent (`EXCLUDED_TOOLS_FOR_SUBAGENTS`), so a child Run can never reach for
//! it - the recursion guard.

use serde_json::{Value, json};

use crate::tool::{Tool, ToolCtx, ToolSpec};

pub struct TaskStop;

/// The tool description, VERBATIM from qwen `tools/task-stop.ts`.
const DESCRIPTION: &str = "Stop a background task by its ID. Running agents and shells are cancelled; paused recovered agents are abandoned without resuming them.";

#[async_trait::async_trait]
impl Tool for TaskStop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_stop".into(),
            description: DESCRIPTION.into(),
            // Schema VERBATIM from qwen `tools/task-stop.ts`: `task_id` required.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The ID of the background task to stop (from the launch response or notification).",
                    },
                },
                "required": ["task_id"],
                "additionalProperties": false,
            }),
        }
    }

    /// Deferred (qwen `shouldDefer: true` - stopping tasks is infrequent): the
    /// model discovers it via `tool_search` rather than seeing it on the wire at
    /// Run start.
    fn should_defer(&self) -> bool {
        true
    }

    fn always_load(&self) -> bool {
        false
    }

    /// qwen `searchHint` VERBATIM: the keywords `tool_search` folds in alongside
    /// the name/description so a "cancel the background agent" query surfaces it.
    fn search_hint(&self) -> Option<&str> {
        Some("task stop cancel kill background")
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // Schema validation guarantees `task_id` is present and a string; the
        // non-empty check mirrors qwen's own guard.
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Parameter \"task_id\" must be a non-empty string.".to_string())?
            .to_string();

        // Reach the host through the SubagentSpawner capability's stop_background
        // (ADR-0063): the Agent that owns the registry returns the VERBATIM qwen
        // wording (found/not-running/not-found). It is never an `Err` - the whole
        // result is the wording, which the tool returns as its content either way,
        // so an `Err` from the capability (a degraded host) folds the same string.
        match ctx.caps.subagents.stop_background(task_id).await {
            Ok(wording) => Ok(wording),
            Err(wording) => Ok(wording),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/tools/task_stop.rs"]
mod tests;
