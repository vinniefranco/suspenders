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
    // Fail-open (ADR-0007): a Todo artifact always serializes, but should that
    // ever break, attach nothing rather than panic - the Presenter reads `None`
    // and simply shows no todo box.
    let value = serde_json::to_value(artifact).unwrap_or(Value::Null);
    token.put_artifact(keys::TODOS, value)
}

fn read_todos_artifact(artifacts: &HashMap<String, Value>) -> Option<TodoArtifact> {
    let value = artifacts.get(keys::TODOS)?;
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
#[path = "../../tests/extensions/todo.rs"]
mod tests;
