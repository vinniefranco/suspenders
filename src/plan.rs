//! The Plan and the original task as one value (CONTEXT.md: Plan).
//!
//! The Plan is the model-maintained task list for the current goal, held by the
//! harness outside the Conversation; the original task is the user's verbatim
//! first prompt. This module owns the value and its composition; the Run loop
//! keeps the *when* (when to fire `set_plan`), the Agent keeps the storage, and
//! `crate::voice` keeps the tool's confirmation framing.
//!
//! The task list is the model's voice: it arrives as a `todo_write` Tool Call
//! carrying a `todos` array (each item a `content` string and a `status`), and
//! this value stores it verbatim - never rewritten, never interpreted. A
//! malformed or errored call stores nothing.
//!
//! ## Where the original task comes from
//!
//! Captured once per Run from the Conversation's first user text
//! ([`crate::conversation::Conversation::original_task`]) - unless the caller
//! already holds a durable copy. After a Compaction the Conversation's head is
//! the summary message, whose first block is also user text: a fresh capture
//! there would carry the summary blob, not the task. The durable copy lives in
//! the Compaction state (captured at the first Compaction), and the Agent
//! threads it into every later Run, so compaction keeps re-appending the
//! verbatim task statement per CONTEXT.md.

use crate::conversation::Conversation;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One item in the model's task list: a `content` line in the model's voice and
/// its `status`. Held verbatim; the harness never edits the content.
///
/// `Serialize`/`Deserialize` so the Todo display extension can attach the parsed
/// list as an Artifact (ADR-0048): the same value the Run-loop's Plan fold reads
/// rides the Tool Result to Presentment, so the committed render never re-parses
/// the raw JSON args.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// A task item's status, the `todo_write` vocabulary: not started, the single
/// item currently being worked, or finished. `snake_case` on the wire so the
/// serialized form matches the `todo_write` tokens (`pending`/`in_progress`/
/// `completed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// The wire token for this status (the `todo_write` vocabulary and the
    /// serde snake_case representation): `"pending"`, `"in_progress"`, or
    /// `"completed"`. The single canonical string mapping; `parse` delegates
    /// to it to keep the two directions in sync.
    fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    /// Parses the `todo_write` status enum, returning `None` for any token
    /// outside the vocabulary (a malformed item the caller drops).
    /// Delegates to `as_str` so the string mapping lives once.
    fn parse(s: &str) -> Option<Self> {
        [
            TodoStatus::Pending,
            TodoStatus::InProgress,
            TodoStatus::Completed,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }

    /// The checklist glyph for this status (qwen `STATUS_ICONS`, TodoDisplay.tsx):
    /// `○` U+25CB pending, `◐` U+25D0 in progress, `●` U+25CF completed. Plain
    /// `&str` so `plan.rs` stays ratatui-free (ADR-0019); the in_progress-green /
    /// completed-strikethrough treatment lives in `ui/components`.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            TodoStatus::Pending => "○",
            TodoStatus::InProgress => "◐",
            TodoStatus::Completed => "●",
        }
    }
}

/// The Plan value: the model's current task list (`todos`) and the user's
/// verbatim first prompt (`original_task`). The `restored` render is the plan
/// string carried in from a previous Run (the Agent holds it), used only as the
/// rendered form until a fresh `todo_write` replaces the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub todos: Vec<TodoItem>,
    pub original_task: Option<String>,
    restored: Option<String>,
}

/// The outcome of folding one Tool Call into the Plan: `Updated` carries the
/// new Plan (the caller persists it, firing the `set_plan` Dep); `Unchanged`
/// leaves the Plan alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Updated(Plan),
    Unchanged,
}

impl Plan {
    /// Builds the Run's Plan value: `restored` is the plan string from the
    /// previous Run (the Agent holds it), `original_task` from a durable copy
    /// when one exists (the Compaction state after the first Compaction). The
    /// restored string is a rendered checklist, not structured todos, so it is
    /// kept only as the render fallback until a fresh `todo_write` replaces it.
    pub fn new(restored: Option<String>, original_task: Option<String>) -> Self {
        Plan {
            todos: Vec::new(),
            original_task,
            restored,
        }
    }

    /// Captures the verbatim original task from the Conversation, once: a Plan
    /// that already carries one is returned unchanged. Called at Run start,
    /// before any Compaction can summarize the head away.
    pub fn capture_task(mut self, conv: &Conversation) -> Self {
        if self.original_task.is_none() {
            self.original_task = conv.original_task().map(|t| t.to_string());
        }
        self
    }

    /// Folds one executed Tool Call into the Plan: a successful `todo_write`
    /// call carrying a non-empty, well-formed `todos` array replaces the list;
    /// anything else leaves the Plan alone. The items are the model's voice,
    /// verbatim - never rewritten (a malformed input sentinel, an errored call,
    /// or a list with no valid item stores nothing).
    pub fn update(&self, name: &str, input: &Value, is_error: bool) -> Update {
        if name == "todo_write"
            && !is_error
            && let Some(todos) = parse_todos(input)
            && !todos.is_empty()
        {
            let mut updated = self.clone();
            updated.todos = todos;
            updated.restored = None;
            return Update::Updated(updated);
        }
        Update::Unchanged
    }

    /// Renders the task list as a checklist string for the log and the UI: one
    /// line per item, its status glyph then its content. When the list is empty
    /// (a resumed Run before any fresh `todo_write`), the restored render is
    /// returned as-is.
    pub fn render(&self) -> String {
        if self.todos.is_empty() {
            return self.restored.clone().unwrap_or_default();
        }
        self.todos
            .iter()
            .map(|t| format!("{} {}", t.status.glyph(), t.content))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parses the `todos` array from a `todo_write` input, returning the well-formed
/// items. An item needs a non-empty string `content` and a known `status`;
/// malformed items are dropped. A missing or non-array `todos` yields `None`.
///
/// `pub(crate)` so the Todo display extension parses the SAME vocabulary the
/// Run-loop's Plan fold does (ADR-0048: `plan.rs` owns the todo vocabulary; the
/// three consumers - Plan fold, committed render, sticky box - never re-derive
/// it).
pub(crate) fn parse_todos(input: &Value) -> Option<Vec<TodoItem>> {
    let items = input.get("todos")?.as_array()?;
    let todos = items
        .iter()
        .filter_map(|item| {
            let content = item.get("content")?.as_str()?;
            if content.is_empty() {
                return None;
            }
            let status = TodoStatus::parse(item.get("status")?.as_str()?)?;
            Some(TodoItem {
                content: content.to_string(),
                status,
            })
        })
        .collect();
    Some(todos)
}

#[cfg(test)]
#[path = "../tests/unit/plan.rs"]
mod tests;
