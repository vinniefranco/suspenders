//! `explore(task)`: dispatches a Scout (CONTEXT.md), a disposable read-only
//! worker that searches the Project Root in its own fresh Conversation and
//! returns a structured findings report as this Tool's result.
//!
//! The Scout is reached through the `scout` capture on the ctx (an effect
//! wired to the Session), so this Tool stays a plain [`Tool`].
//!
//! ## Never crashes the Turn
//!
//! Every Scout failure mode - an LLM error, empty findings, or hitting the
//! Scout's hard Pass cap - comes back as an ordinary Tool Result. Failures are
//! `is_error` and carry a Voice-owned marker plus whatever partial findings the
//! Scout gathered; a clean report is an ok result.

use crate::scout::ScoutOutcome;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::voice;
use serde_json::{json, Value};

pub struct Explore;

#[async_trait::async_trait]
impl Tool for Explore {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".into(),
            description: "Dispatch a read-only Scout to answer one focused question about the codebase \
                - where does X live, how does Y work. The Scout searches the project on \
                its own - grep, list_files, read_file across many steps - and returns a structured \
                findings report: locations as file:line, how it works, what to read next, and open \
                questions. Delegate multi-step searching here instead of exploring inline; it keeps \
                the search out of your own context. The Scout cannot edit, run commands, or explore \
                further."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "What to find, as one focused question, e.g. where the Turn loop is \
                            defined and how it dispatches tools."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let task = input
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: explore requires a string \"task\"".to_string())?;

        match &ctx.scout {
            Some(scout) => dispatch(scout(task.to_string()).await),
            None => Err("explore is unavailable: no Scout is wired for this session".to_string()),
        }
    }
}

// Map the Scout outcome to a Tool Result. A clean report is an ok result;
// every failure mode is an is_error result carrying a Voice-owned marker and
// whatever partial findings the Scout gathered.
fn dispatch(outcome: ScoutOutcome) -> Result<String, String> {
    match outcome {
        ScoutOutcome::Ok(report) => Ok(report),
        ScoutOutcome::LlmError { partial } => {
            Err(with_partial(voice::scout_llm_error(), &partial))
        }
        ScoutOutcome::PassCap { limit, partial } => {
            Err(with_partial(&voice::scout_pass_cap(limit), &partial))
        }
        ScoutOutcome::Empty { partial } => {
            Err(with_partial(voice::scout_empty_findings(), &partial))
        }
    }
}

fn with_partial(marker: &str, partial: &str) -> String {
    if partial.is_empty() {
        marker.to_string()
    } else {
        format!("{marker}\n\n{partial}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scout::ScoutFn;
    use std::sync::Arc;

    // A ctx whose `scout` capture returns a canned outcome.
    fn ctx(scout: ScoutFn) -> ToolCtx {
        ToolCtx {
            root: std::path::PathBuf::from("/nowhere"),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: Some(scout),
        }
    }

    fn canned(outcome: ScoutOutcome) -> ScoutFn {
        Arc::new(move |_task| {
            let outcome = outcome.clone();
            Box::pin(async move { outcome })
        })
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        Explore.run(&input, ctx).await
    }

    #[test]
    fn spec_is_named_explore_with_a_single_required_string_task() {
        let spec = Explore.spec();
        assert_eq!(spec.name, "explore");
        assert_eq!(spec.input_schema["required"], json!(["task"]));
        assert_eq!(spec.input_schema["properties"]["task"]["type"], "string");
    }

    #[test]
    fn spec_tells_the_model_to_delegate() {
        let spec = Explore.spec();
        let d = spec.description.to_lowercase();
        assert!(d.contains("where does") || d.contains("how does") || d.contains("delegate"));
    }

    #[tokio::test]
    async fn returns_the_scouts_findings_report_as_an_ok_result() {
        let report = "Locations: lib/widget.ex:2. How it works: ...";
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        let rep = report.to_string();
        let scout: ScoutFn = Arc::new(move |task| {
            *cap2.lock().unwrap() = task;
            let rep = rep.clone();
            Box::pin(async move { ScoutOutcome::Ok(rep) })
        });

        assert_eq!(
            run(json!({"task": "find widget"}), &ctx(scout)).await,
            Ok(report.into())
        );
        assert_eq!(*captured.lock().unwrap(), "find widget");
    }

    #[tokio::test]
    async fn an_llm_error_becomes_an_is_error_result_with_marker_and_partial() {
        let scout = canned(ScoutOutcome::LlmError {
            partial: "Partway: saw lib/a.ex.".into(),
        });
        let content = run(json!({"task": "t"}), &ctx(scout)).await.unwrap_err();
        assert!(content.contains(voice::scout_llm_error()));
        assert!(content.contains("Partway: saw lib/a.ex."));
    }

    #[tokio::test]
    async fn hitting_the_pass_cap_becomes_an_is_error_result_with_the_cap_marker() {
        let scout = canned(ScoutOutcome::PassCap {
            limit: 8,
            partial: "Partial: found something.".into(),
        });
        let content = run(json!({"task": "t"}), &ctx(scout)).await.unwrap_err();
        assert!(content.contains("Partial: found something."));
        assert!(content.contains("8"));
    }

    #[tokio::test]
    async fn empty_findings_become_an_is_error_result_with_the_empty_marker() {
        let scout = canned(ScoutOutcome::Empty { partial: "".into() });
        let content = run(json!({"task": "t"}), &ctx(scout)).await.unwrap_err();
        assert!(content.contains(voice::scout_empty_findings()));
    }

    #[tokio::test]
    async fn a_non_string_task_is_a_structured_error() {
        let scout = canned(ScoutOutcome::Ok("unused".into()));
        let result = crate::tools::execute("explore", &json!({"task": 123}), &ctx(scout)).await;
        assert!(result.is_error);
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn a_missing_scout_capture_is_a_graceful_error() {
        let ctx = ToolCtx {
            root: std::path::PathBuf::from("/nowhere"),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        };
        let err = run(json!({"task": "t"}), &ctx).await.unwrap_err();
        assert!(!err.is_empty());
    }
}
