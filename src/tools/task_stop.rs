//! `task_stop(task_id)`: stops a background subagent by its task id (faithful to
//! qwen v0.16.0's `tools/task-stop.ts`, ADR-0063).
//!
//! The model reaches for this to cancel a background agent it launched (via
//! `agent` with `run_in_background: true`) - the task id came back in the launch
//! string or a `<task-notification>`. The tool reaches the host through the
//! [`SubagentSpawner`](crate::tool::caps::SubagentSpawner) capability's
//! `stop_background`, which relays to the Agent that owns the background registry:
//! the Agent aborts the running child, sets the entry `Stopped`, queues the `was
//! cancelled` notification, and returns the VERBATIM qwen wording. Suspenders
//! ports only the AGENT registry leg of qwen's `task_stop` (the shell/monitor/
//! memory registries are not ported), so the three outcomes are the running-agent
//! stop confirmation, the not-running error, and the not-found error - all
//! VERBATIM.
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
mod tests {
    use super::*;
    use crate::tool::caps::{Capabilities, SubagentRequest, SubagentResult, SubagentSpawner};
    use std::sync::Arc;

    #[test]
    fn spec_is_the_verbatim_schema_and_deferred() {
        let spec = TaskStop.spec();
        assert_eq!(spec.name, "task_stop");
        assert_eq!(spec.description, DESCRIPTION);
        assert_eq!(spec.input_schema["properties"]["task_id"]["type"], "string");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "task_id");
        // Deferred, never always-load, with the verbatim search hint.
        assert!(TaskStop.should_defer());
        assert!(!TaskStop.always_load());
        assert_eq!(
            TaskStop.search_hint(),
            Some("task stop cancel kill background")
        );
    }

    // A spawner that returns a scripted `stop_background` wording, so the tool's
    // three-outcome plumbing is exercisable without an Agent.
    struct ScriptedStopSpawner {
        wording: String,
    }

    #[async_trait::async_trait]
    impl SubagentSpawner for ScriptedStopSpawner {
        async fn spawn(&self, _request: SubagentRequest) -> Result<SubagentResult, String> {
            Err("unused".into())
        }
        async fn spawn_background(
            &self,
            _request: SubagentRequest,
            _description: String,
        ) -> Result<String, String> {
            Err("unused".into())
        }
        async fn stop_background(&self, _id: String) -> Result<String, String> {
            Ok(self.wording.clone())
        }
    }

    fn ctx_with(spawner: Arc<dyn SubagentSpawner>) -> ToolCtx {
        ToolCtx {
            caps: Capabilities::for_test_with_subagents(spawner),
            ..ToolCtx::for_test(std::env::temp_dir(), 100_000)
        }
    }

    #[tokio::test]
    async fn run_returns_the_running_stop_confirmation() {
        let wording = "Cancellation requested for background agent \"scout-1\". A final \
             task-notification carrying the agent's last result will follow.\n\
             Description: explore api"
            .to_string();
        let out = TaskStop
            .run(
                &json!({"task_id": "scout-1"}),
                &ctx_with(Arc::new(ScriptedStopSpawner {
                    wording: wording.clone(),
                })),
            )
            .await
            .unwrap();
        assert_eq!(out, wording);
    }

    #[tokio::test]
    async fn run_returns_the_not_running_error() {
        let wording =
            "Error: Background agent \"scout-1\" is not running (status: completed).".to_string();
        let out = TaskStop
            .run(
                &json!({"task_id": "scout-1"}),
                &ctx_with(Arc::new(ScriptedStopSpawner {
                    wording: wording.clone(),
                })),
            )
            .await
            .unwrap();
        assert_eq!(out, wording);
    }

    #[tokio::test]
    async fn run_returns_the_not_found_error_from_a_degraded_host() {
        // The default for_test ctx carries an UnavailableSubagentSpawner, whose
        // stop_background is the VERBATIM not-found wording.
        let out = TaskStop
            .run(
                &json!({"task_id": "ghost-9"}),
                &ToolCtx::for_test(std::env::temp_dir(), 100_000),
            )
            .await
            .unwrap();
        assert_eq!(out, "Error: No background task found with ID \"ghost-9\".");
    }
}
