//! `plan(plan)`: records the model's Plan (CONTEXT.md: Plan) — its statement
//! of the current goal, the steps with their status, and the next step.
//!
//! The schema is deliberately tiny and flat (one required string) so a small
//! model can fill it reliably. The Plan itself is the model's voice: this tool
//! never rewrites, parses, or interprets it. Executing the tool returns a
//! short Voice-neutral confirmation; the storage and Anchor injection are the
//! harness's concern, not this tool's.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::voice;
use serde_json::{Value, json};

pub struct Plan;

#[async_trait::async_trait]
impl Tool for Plan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plan".into(),
            description: "Record or update your plan for this task. Set the plan early in a task, \
                before exploring, and update it as you finish each step. State the goal, \
                the steps with their status (e.g. [x] done, [ ] not done), and the next \
                step. This keeps you oriented; the plan is kept in view for you as you work."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The full plan text: the goal, the steps with their status, and the next step."
                    }
                },
                "required": ["plan"]
            }),
        }
    }

    async fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        match input.get("plan") {
            Some(Value::String(s)) if !s.is_empty() => Ok(voice::plan_confirmation().to_string()),
            _ => Err("invalid input: plan requires a non-empty string \"plan\"".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx {
            root: std::path::PathBuf::from("/nowhere"),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
    }

    async fn run(input: Value) -> Result<String, String> {
        Plan.run(&input, &ctx()).await
    }

    #[test]
    fn spec_is_a_flat_one_string_schema_requiring_plan() {
        let spec = Plan.spec();
        assert_eq!(spec.name, "plan");
        let schema = &spec.input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["plan"]));
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props.keys().collect::<Vec<_>>(), vec!["plan"]);
        assert_eq!(schema["properties"]["plan"]["type"], "string");
        assert!(spec.description.contains("plan"));
    }

    #[tokio::test]
    async fn run_with_a_plan_string_returns_a_short_confirmation() {
        let confirmation = run(json!({"plan": "Goal: fix bug. 1. read [ ] 2. edit [ ]"}))
            .await
            .unwrap();
        assert!(confirmation.chars().count() < 120);
        assert!(!confirmation.contains("fix bug"));
    }

    #[tokio::test]
    async fn run_rejects_a_missing_or_non_string_plan() {
        let err = run(json!({})).await.unwrap_err();
        assert!(err.contains("plan"));
        assert!(run(json!({"plan": 42})).await.is_err());
        assert!(run(json!({"plan": ""})).await.is_err());
    }

    #[tokio::test]
    async fn the_plan_tool_is_registered() {
        let names: Vec<String> = crate::tools::specs()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"plan".to_string()));
    }

    #[test]
    fn the_plan_tool_never_requires_approval() {
        assert_eq!(crate::approvals::gate_text("plan", &json!({})), None);
    }
}
