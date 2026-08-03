use super::*;
use crate::subagents::builtins;
use crate::tool::caps::{Capabilities, SubagentRequest, SubagentResult, SubagentSpawner};
use std::sync::Mutex;

fn tool() -> AgentTool {
    AgentTool::new(Arc::new(SubagentRegistry::new(builtins())))
}

#[test]
fn spec_schema_enum_is_the_registry_names() {
    let spec = tool().spec();
    assert_eq!(spec.name, "agent");
    let enum_names = spec.input_schema["properties"]["subagent_type"]["enum"]
        .as_array()
        .expect("enum present with defs")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(enum_names, vec!["general-purpose", "Explore"]);
    // The required set is description + prompt (subagent_type optional).
    let required = spec.input_schema["required"].as_array().unwrap();
    assert_eq!(required.len(), 2);
}

#[test]
fn spec_description_lists_the_subagents_and_is_not_deferred() {
    let t = tool();
    let spec = t.spec();
    assert!(spec.description.contains("- **general-purpose**:"));
    assert!(spec.description.contains("- **Explore**:"));
    // P4b restores the run_in_background bullet AND the schema property.
    assert!(spec.description.contains("run_in_background: true"));
    assert!(spec.input_schema["properties"]["run_in_background"]["type"] == "boolean");
    // The worktree/isolation note stays trimmed (deferred).
    assert!(!spec.description.contains("worktree"));
    assert!(spec.input_schema["properties"]["isolation"].is_null());
    assert!(!t.should_defer());
}

#[test]
fn spec_description_uses_v0_21_4_wording_and_omits_fork_team() {
    let spec = tool().spec();
    let d = &spec.description;
    // v0.21.4 re-ported phrasing that applies to the trimmed surface.
    assert!(d.contains("If omitted, the general-purpose agent is used."));
    assert!(d.contains("Delegate only concrete, bounded tasks that can run independently."));
    assert!(d.contains("Treat the agent's output as evidence, not as automatically correct."));
    assert!(d.contains("## Writing the prompt"));
    assert!(d.contains("**Never delegate understanding.**"));
    // The single test-runner example survives; the v0.16 greeting-responder
    // second example is gone.
    assert!(
        d.contains("\"test-runner\": use this agent after you are done writing code to run tests")
    );
    assert!(!d.contains("greeting-responder"));
    // Fork/team/isolation surface stays OUT (ADR-0061/0063).
    assert!(!d.contains("fork"));
    assert!(!d.contains("## When to fork"));
    assert!(!d.contains("teammate"));
    assert!(!d.contains("isolation"));
    // subagent_type keeps suspenders' non-fork wording.
    assert_eq!(
        spec.input_schema["properties"]["subagent_type"]["description"],
        "The type of specialized agent to use for this task"
    );
}

#[test]
fn empty_registry_omits_the_enum_and_shows_the_empty_wording() {
    let t = AgentTool::new(Arc::new(SubagentRegistry::default()));
    let spec = t.spec();
    assert!(
        spec.input_schema["properties"]["subagent_type"]
            .get("enum")
            .is_none()
    );
    assert!(
        spec.description
            .contains("No subagents are currently configured.")
    );
}

// A spawner that records the request it received and returns a fixed result.
// `bg_seen` records background launches as `(subagent_type, prompt,
// description)` and `bg_id` is the task id `spawn_background` returns.
struct RecordingSpawner {
    seen: Arc<Mutex<Vec<(String, String)>>>,
    result: SubagentResult,
    bg_seen: Arc<Mutex<Vec<(String, String, String)>>>,
    bg_id: String,
}

impl RecordingSpawner {
    fn new(seen: Arc<Mutex<Vec<(String, String)>>>, result: SubagentResult) -> Self {
        RecordingSpawner {
            seen,
            result,
            bg_seen: Arc::new(Mutex::new(Vec::new())),
            bg_id: "general-purpose-1".into(),
        }
    }
}

#[async_trait::async_trait]
impl SubagentSpawner for RecordingSpawner {
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String> {
        self.seen
            .lock()
            .unwrap()
            .push((request.subagent_type, request.prompt));
        Ok(SubagentResult {
            terminate_reason: self.result.terminate_reason.clone(),
            result: self.result.result.clone(),
        })
    }

    async fn spawn_background(
        &self,
        request: SubagentRequest,
        description: String,
    ) -> Result<String, String> {
        self.bg_seen
            .lock()
            .unwrap()
            .push((request.subagent_type, request.prompt, description));
        Ok(self.bg_id.clone())
    }

    async fn stop_background(&self, id: String) -> Result<String, String> {
        Ok(format!("Error: No background task found with ID \"{id}\"."))
    }
}

fn ctx_with(spawner: Arc<dyn SubagentSpawner>) -> ToolCtx {
    ToolCtx {
        caps: Capabilities::for_test_with_subagents(spawner),
        ..ToolCtx::for_test(std::env::temp_dir(), 100_000)
    }
}

#[tokio::test]
async fn run_spawns_the_resolved_request_and_returns_the_goal_result() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::clone(&seen),
        SubagentResult {
            terminate_reason: "GOAL".into(),
            result: "the findings".into(),
        },
    ));
    let out = tool()
        .run(
            &json!({
                "description": "find the bug",
                "prompt": "investigate the panic",
                "subagent_type": "general-purpose",
            }),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    assert_eq!(out, "the findings");
    // The spawner received the resolved request.
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0],
        (
            "general-purpose".to_string(),
            "investigate the panic".to_string()
        )
    );
}

#[tokio::test]
async fn run_defaults_the_subagent_type_to_general_purpose() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::clone(&seen),
        SubagentResult {
            terminate_reason: "GOAL".into(),
            result: "ok".into(),
        },
    ));
    tool()
        .run(
            &json!({"description": "do it", "prompt": "the task"}),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap()[0].0, "general-purpose");
}

#[tokio::test]
async fn run_error_empty_result_is_the_verbatim_execution_failed_message() {
    // qwen foreground: ERROR + empty finalText -> "Subagent execution failed."
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::new(Mutex::new(Vec::new())),
        SubagentResult {
            terminate_reason: "ERROR".into(),
            result: String::new(),
        },
    ));
    let out = tool()
        .run(
            &json!({"description": "do it", "prompt": "the task"}),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    assert_eq!(out, "Subagent execution failed.");
}

#[tokio::test]
async fn run_error_non_empty_result_surfaces_the_bare_partial_text() {
    // qwen foreground: ERROR + non-empty finalText -> the raw finalText, no
    // synthetic message.
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::new(Mutex::new(Vec::new())),
        SubagentResult {
            terminate_reason: "ERROR".into(),
            result: "got this far".into(),
        },
    ));
    let out = tool()
        .run(
            &json!({"description": "do it", "prompt": "the task"}),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    assert_eq!(out, "got this far");
}

#[tokio::test]
async fn run_max_turns_empty_result_is_the_bare_empty_string() {
    // qwen foreground: a non-ERROR terminate (MAX_TURNS) with an empty
    // finalText surfaces the bare (empty) text, NOT the background
    // `Agent terminated with mode: ...` string.
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::new(Mutex::new(Vec::new())),
        SubagentResult {
            terminate_reason: "MAX_TURNS".into(),
            result: String::new(),
        },
    ));
    let out = tool()
        .run(
            &json!({"description": "do it", "prompt": "the task"}),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    assert_eq!(out, "");
}

#[tokio::test]
async fn run_unknown_subagent_type_is_the_verbatim_not_found_error() {
    let err = tool()
        .run(
            &json!({"description": "d", "prompt": "p", "subagent_type": "nope"}),
            &ctx_with(Arc::new(RecordingSpawner::new(
                Arc::new(Mutex::new(Vec::new())),
                SubagentResult {
                    terminate_reason: "GOAL".into(),
                    result: String::new(),
                },
            ))),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        "Subagent \"nope\" not found. Available subagents: general-purpose, Explore"
    );
}

#[tokio::test]
async fn run_degraded_spawner_folds_the_verbatim_unavailable_string() {
    // The default for_test ctx carries an UnavailableSubagentSpawner.
    let err = tool()
        .run(
            &json!({"description": "d", "prompt": "p", "subagent_type": "general-purpose"}),
            &ToolCtx::for_test(std::env::temp_dir(), 100_000),
        )
        .await
        .unwrap_err();
    assert_eq!(err, "subagents are unavailable in this environment");
}

#[tokio::test]
async fn run_in_background_true_calls_spawn_background_and_returns_the_launch_string() {
    // 4c: run_in_background: true routes to spawn_background (NOT spawn), and
    // the tool returns the VERBATIM launch string carrying the minted id.
    let spawner = Arc::new(RecordingSpawner::new(
        Arc::new(Mutex::new(Vec::new())),
        SubagentResult {
            terminate_reason: "GOAL".into(),
            result: "unused".into(),
        },
    ));
    let bg_seen = Arc::clone(&spawner.bg_seen);
    let out = tool()
        .run(
            &json!({
                "description": "find the bug",
                "prompt": "investigate the panic",
                "subagent_type": "general-purpose",
                "run_in_background": true,
            }),
            &ctx_with(spawner),
        )
        .await
        .unwrap();
    // The launch string names the minted id and does NOT carry the subagent's
    // result (the parent parks on nothing - it carries on).
    assert!(out.starts_with("Background agent launched successfully."));
    assert!(out.contains("agentId: general-purpose-1"));
    assert!(out.contains("Use task_stop to cancel."));
    assert!(!out.contains("unused"));
    // The background launch received the resolved request + description; the
    // foreground `spawn` was never called.
    let bg = bg_seen.lock().unwrap();
    assert_eq!(
        bg[0],
        (
            "general-purpose".to_string(),
            "investigate the panic".to_string(),
            "find the bug".to_string()
        )
    );
}

#[test]
fn spec_schema_has_run_in_background_boolean() {
    let spec = tool().spec();
    assert_eq!(
        spec.input_schema["properties"]["run_in_background"]["type"],
        "boolean"
    );
    // run_in_background is optional (not required) and worktree/isolation
    // stays absent.
    let required = spec.input_schema["required"].as_array().unwrap();
    assert!(!required.iter().any(|r| r == "run_in_background"));
    assert!(spec.input_schema["properties"]["isolation"].is_null());
}
