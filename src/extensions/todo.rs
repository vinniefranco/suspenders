//! The Todo display Extension (ADR-0048): the model's `todo_write` task list as
//! a first-class [`TranscriptItem::Todo`] instead of the raw JSON args.
//!
//! Mirrors [`super::diff`]. A Middleware [`post_run`](Todo::post_run) on a
//! successful `todo_write` parses the call input `todos` with the SAME
//! `plan::parse_todos` the Run-loop's Plan fold uses (ADR-0048: `plan.rs` owns
//! the todo vocabulary; no consumer re-derives it) and attaches a `todos`
//! Artifact. The Presenter [`present`](Todo::present) swaps a successful
//! `todo_write` Tool Result for a [`TranscriptItem::Todo`] carrying the parsed
//! items, so the committed render draws the circle list (`○ ◐ ●`) rather than
//! the raw `{"todos": ...}` args (the live-vet defect). A malformed, empty,
//! errored, or other-tool call passes through untouched.
//!
//! Pure like `diff.rs`: no ratatui (ADR-0019). The glyph/colour treatment lives
//! in `ui/components`; this module only carries the parsed items to Presentment.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::middleware::{Middleware, Token};
use crate::plan::{self, TodoItem};
use crate::presenter::Presenter;
use crate::view_model::TranscriptItem;

/// The Token keys the Todo extension reserves, declared in one place (the
/// [`super::diff::keys`] pattern): a producer and consumer that disagree fail to
/// *compile*, and a rename touches this module alone.
mod keys {
    /// `artifacts`: the serialized [`super::TodoArtifact`] that rides the Tool
    /// Result to Presentment, read back by [`super::Todo::present`].
    pub const TODOS: &str = "todos";
}

/// The tool the Todo extension acts on.
const TOOL: &str = "todo_write";

/// The Artifact carried to Presentment: the parsed task list. Serialized into
/// the `todos` slot; `TodoItem`'s `snake_case` status matches the `todo_write`
/// vocabulary on the wire (ADR-0048, [`plan::TodoItem`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TodoArtifact {
    items: Vec<TodoItem>,
}

/// The Todo display extension (ADR-0048).
pub struct Todo;

impl Middleware for Todo {
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        let is_error = token.result.as_ref().map(|r| r.is_error).unwrap_or(true);
        if token.tool != TOOL || is_error {
            return token;
        }
        // The SAME parse the Plan fold runs: a missing/non-array `todos` or an
        // all-malformed list yields nothing, so no artifact attaches and the
        // Presenter passes the raw result through.
        match plan::parse_todos(&token.input) {
            Some(items) if !items.is_empty() => put_todos(token, &TodoArtifact { items }),
            _ => token,
        }
    }
}

impl Presenter for Todo {
    fn present(
        &self,
        item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        // Replace a successful todo_write Tool Result summary with a first-class
        // Todo item; everything else passes through.
        if let TranscriptItem::ToolResult {
            name,
            is_error: false,
            ..
        } = &item
            && name == TOOL
            && let Some(artifact) = read_todos_artifact(artifacts)
        {
            return TranscriptItem::Todo {
                items: artifact.items,
            };
        }
        item
    }
}

// ---- artifact (de)serialization ----

fn put_todos(token: Token, artifact: &TodoArtifact) -> Token {
    let value = serde_json::to_value(artifact).expect("Todo artifact serializes");
    token.put_artifact(keys::TODOS, value)
}

fn read_todos_artifact(artifacts: &HashMap<String, Value>) -> Option<TodoArtifact> {
    let value = artifacts.get(keys::TODOS)?;
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::{self, Registered};
    use crate::plan::TodoStatus;
    use crate::tool::ToolCtx;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
        }
    }

    fn extensions() -> Vec<Registered> {
        vec![
            Registered::new("todo", json!({}))
                .with_middleware(Box::new(Todo))
                .with_presenter(Box::new(Todo)),
        ]
    }

    // The lifecycle exactly as the Run runs it: pre_run, then execution with
    // post_run and Shaping inside extensions::execute.
    async fn run(name: &str, input: Value, ctx: &ToolCtx) -> extensions::PipelineResult {
        let regs = extensions();
        let (token, failures) = extensions::pre_run(&regs, Token::new(name, input, ctx.clone()));
        assert!(failures.is_empty());
        let (result, failures) = extensions::execute(&regs, token).await;
        assert!(failures.is_empty());
        result
    }

    fn items(overrides: impl FnOnce(&mut Vec<TodoItem>)) -> Vec<TodoItem> {
        let mut items = vec![
            TodoItem {
                content: "read".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "edit".into(),
                status: TodoStatus::InProgress,
            },
        ];
        overrides(&mut items);
        items
    }

    // ---- post_run: artifact attach / skip ----

    #[tokio::test]
    async fn a_successful_todo_write_attaches_the_parsed_items_artifact() {
        // todo_write is not a shipped tool in the test harness; the Shaping still
        // runs post_run, and a non-erroring result (the default fake) attaches.
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let input = json!({
            "todos": [
                { "content": "read", "status": "completed" },
                { "content": "edit", "status": "in_progress" },
            ]
        });
        let result = run(TOOL, input, &ctx).await;

        let artifact = read_todos_artifact(&result.artifacts).expect("todos artifact present");
        assert_eq!(artifact.items, items(|_| {}));
    }

    #[tokio::test]
    async fn a_malformed_or_empty_list_attaches_no_artifact() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());

        // Empty list: nothing to show.
        let result = run(TOOL, json!({ "todos": [] }), &ctx).await;
        assert!(result.artifacts.is_empty());

        // Non-array todos: parse yields None.
        let result = run(TOOL, json!({ "todos": "nope" }), &ctx).await;
        assert!(result.artifacts.is_empty());

        // A list of only malformed items collapses to empty -> no artifact.
        let result = run(TOOL, json!({ "todos": [{ "content": "" }] }), &ctx).await;
        assert!(result.artifacts.is_empty());
    }

    #[tokio::test]
    async fn other_tools_pass_through_untouched() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        std::fs::write(tmp.path().join("r.txt"), "hello").unwrap();

        let result = run("read_file", json!({ "path": "r.txt" }), &ctx).await;
        assert!(result.artifacts.is_empty());
    }

    // ---- present: swap / passthrough ----

    fn todos_artifact(items: Vec<TodoItem>) -> HashMap<String, Value> {
        let mut map = HashMap::new();
        map.insert(
            keys::TODOS.to_string(),
            serde_json::to_value(TodoArtifact { items }).unwrap(),
        );
        map
    }

    #[test]
    fn present_swaps_a_successful_todo_write_result_for_a_todo_item() {
        let item = TranscriptItem::ToolResult {
            name: TOOL.to_string(),
            summary: "todos=...".to_string(),
            is_error: false,
            key_arg: None,
        };
        let presented = Todo.present(item, &todos_artifact(items(|_| {})), &json!({}));
        assert_eq!(
            presented,
            TranscriptItem::Todo {
                items: items(|_| {})
            }
        );
    }

    #[test]
    fn present_passes_errors_missing_artifacts_and_other_tools_through() {
        let error_item = TranscriptItem::ToolResult {
            name: TOOL.to_string(),
            summary: "boom".to_string(),
            is_error: true,
            key_arg: None,
        };
        let other = TranscriptItem::ToolResult {
            name: "read_file".to_string(),
            summary: "340 lines".to_string(),
            is_error: false,
            key_arg: None,
        };
        let no_artifact = TranscriptItem::ToolResult {
            name: TOOL.to_string(),
            summary: "todos=...".to_string(),
            is_error: false,
            key_arg: None,
        };

        assert_eq!(
            Todo.present(
                error_item.clone(),
                &todos_artifact(items(|_| {})),
                &json!({})
            ),
            error_item
        );
        assert_eq!(
            Todo.present(other.clone(), &todos_artifact(items(|_| {})), &json!({})),
            other
        );
        // A successful todo_write with NO artifact (malformed input upstream)
        // keeps its default summary rather than an empty Todo.
        assert_eq!(
            Todo.present(no_artifact.clone(), &HashMap::new(), &json!({})),
            no_artifact
        );
    }

    // ---- serde round-trip (the artifact wire form) ----

    #[test]
    fn the_todo_artifact_round_trips_with_snake_case_status() {
        let artifact = TodoArtifact {
            items: items(|v| {
                v.push(TodoItem {
                    content: "ship".into(),
                    status: TodoStatus::Pending,
                })
            }),
        };
        let value = serde_json::to_value(&artifact).unwrap();
        assert_eq!(
            value["items"][2]["status"], "pending",
            "status tokens match the todo_write vocabulary"
        );
        let back: TodoArtifact = serde_json::from_value(value).unwrap();
        assert_eq!(back, artifact);
    }

    // ---- config wiring (risk #5: a missing registration silently no-ops) ----

    #[test]
    fn configured_resolves_the_todo_name_to_one_todo_extension() {
        let extensions = extensions::configured(&["todo".to_string()]);
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].name, "todo");
    }

    // End-to-end through the SHIPPED DEFAULT extension list (not a hand-built
    // one): the true silent-regression guard for the user's bug. Drive a
    // `todo_write` through `configured(base().extensions)` → post_run (artifact)
    // → present, and assert the committed item is a first-class `Todo`, not a raw
    // `ToolResult`. If `"todo"` ever falls out of the default config (risk #5),
    // the present fold leaves the ToolResult and this fails.
    #[tokio::test]
    async fn the_default_config_renders_todo_write_as_a_todo_item_not_raw_json() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let regs = extensions::configured(&crate::session::SessionConfig::base().extensions);

        let input = json!({
            "todos": [
                { "content": "read", "status": "completed" },
                { "content": "edit", "status": "in_progress" },
            ]
        });
        let (token, failures) =
            extensions::pre_run(&regs, Token::new(TOOL, input.clone(), ctx.clone()));
        assert!(failures.is_empty());
        let (result, failures) = extensions::execute(&regs, token).await;
        assert!(failures.is_empty());

        // The committed ToolResult carries the parsed items via the artifact; the
        // present fold swaps it for a Todo.
        let tool_result = TranscriptItem::ToolResult {
            name: TOOL.to_string(),
            summary: String::new(),
            is_error: false,
            key_arg: None,
        };
        let (presented, failures) = extensions::present(&regs, tool_result, &result.artifacts);
        assert!(failures.is_empty());
        assert_eq!(
            presented,
            TranscriptItem::Todo {
                items: items(|_| {})
            },
            "the default config must render todo_write as a Todo, not a raw ToolResult"
        );
    }
}
