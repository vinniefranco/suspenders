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
