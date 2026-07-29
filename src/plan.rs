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
use serde_json::Value;

/// One item in the model's task list: a `content` line in the model's voice and
/// its `status`. Held verbatim; the harness never edits the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// A task item's status, the `todo_write` vocabulary: not started, the single
/// item currently being worked, or finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Parses the `todo_write` status enum, returning `None` for any token
    /// outside the vocabulary (a malformed item the caller drops).
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            _ => None,
        }
    }

    /// The checklist glyph for this status: `[ ]` pending, `[~]` in progress,
    /// `[x]` completed. Used to render the list for the log and the UI.
    fn glyph(self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
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
fn parse_todos(input: &Value) -> Option<Vec<TodoItem>> {
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
mod tests {
    use super::*;
    use crate::conversation::{Conversation, ConversationOpts};
    use serde_json::json;

    fn conversation() -> Conversation {
        Conversation::new("sys", ConversationOpts::new(10_000, 0))
    }

    fn todos(input: Value) -> Vec<TodoItem> {
        match Plan::default().update("todo_write", &input, false) {
            Update::Updated(plan) => plan.todos,
            Update::Unchanged => panic!("expected Updated"),
        }
    }

    // ---- capture_task/2 ----

    #[test]
    fn captures_the_conversations_first_user_text_once() {
        let mut conv = conversation();
        conv.add_user_text("fix the flaky test");

        let plan = Plan::default().capture_task(&conv);
        assert_eq!(plan.original_task.as_deref(), Some("fix the flaky test"));
    }

    #[test]
    fn a_plan_already_carrying_a_task_is_unchanged() {
        let mut conv = conversation();
        conv.add_user_text("second turn prompt");

        let plan = Plan::new(None, Some("the real task".to_string())).capture_task(&conv);
        assert_eq!(plan.original_task.as_deref(), Some("the real task"));
    }

    #[test]
    fn the_durable_copy_wins_over_a_summary_head_post_compaction() {
        // After a Compaction the Conversation's head is the summary message -
        // also user text. A fresh capture would carry the summary blob; the
        // carried copy from the Compaction state keeps the verbatim task.
        let mut base = conversation();
        base.add_user_text("original task");
        let conv = base.apply_compaction("what happened so far", 1);

        let fresh = Plan::default().capture_task(&conv);
        assert!(
            fresh
                .original_task
                .unwrap()
                .contains("what happened so far")
        );

        let durable = Plan::new(None, Some("original task".to_string())).capture_task(&conv);
        assert_eq!(durable.original_task.as_deref(), Some("original task"));
    }

    // ---- update/4 ----

    #[test]
    fn a_successful_todo_write_call_replaces_the_list() {
        let items = todos(json!({
            "todos": [
                { "content": "read the file", "status": "completed" },
                { "content": "edit the file", "status": "in_progress" },
                { "content": "build", "status": "pending" },
            ]
        }));
        assert_eq!(
            items,
            vec![
                TodoItem {
                    content: "read the file".to_string(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "edit the file".to_string(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "build".to_string(),
                    status: TodoStatus::Pending,
                },
            ]
        );
    }

    #[test]
    fn other_tools_errored_calls_and_malformed_lists_do_not_update() {
        let plan = Plan::new(Some("keep me".to_string()), Some("task".to_string()));
        let good = json!({ "todos": [{ "content": "x", "status": "pending" }] });

        // A different tool leaves the Plan alone.
        assert_eq!(
            plan.update("read_file", &json!({ "path": "x" }), false),
            Update::Unchanged
        );
        // An errored todo_write call stores nothing.
        assert_eq!(plan.update("todo_write", &good, true), Update::Unchanged);
        // A missing or non-array todos stores nothing.
        assert_eq!(
            plan.update("todo_write", &json!({}), false),
            Update::Unchanged
        );
        assert_eq!(
            plan.update("todo_write", &json!({ "todos": "nope" }), false),
            Update::Unchanged
        );
        // An empty list stores nothing.
        assert_eq!(
            plan.update("todo_write", &json!({ "todos": [] }), false),
            Update::Unchanged
        );
        // A malformed input sentinel stores nothing.
        assert_eq!(
            plan.update(
                "todo_write",
                &crate::llm::malformed_input_marker("raw"),
                false
            ),
            Update::Unchanged
        );
    }

    #[test]
    fn malformed_items_are_dropped_but_the_valid_ones_survive() {
        let items = todos(json!({
            "todos": [
                { "content": "kept", "status": "pending" },
                { "content": "", "status": "pending" },
                { "content": "bad status", "status": "blocked" },
                { "status": "pending" },
                { "content": "also kept", "status": "completed" },
            ]
        }));
        assert_eq!(
            items,
            vec![
                TodoItem {
                    content: "kept".to_string(),
                    status: TodoStatus::Pending,
                },
                TodoItem {
                    content: "also kept".to_string(),
                    status: TodoStatus::Completed,
                },
            ]
        );
    }

    #[test]
    fn a_list_of_only_malformed_items_stores_nothing() {
        let plan = Plan::default();
        assert_eq!(
            plan.update(
                "todo_write",
                &json!({ "todos": [{ "content": "", "status": "pending" }] }),
                false
            ),
            Update::Unchanged
        );
    }

    // ---- render/0 ----

    #[test]
    fn render_writes_a_status_glyph_checklist() {
        let plan = match Plan::default().update(
            "todo_write",
            &json!({
                "todos": [
                    { "content": "read", "status": "completed" },
                    { "content": "edit", "status": "in_progress" },
                    { "content": "build", "status": "pending" },
                ]
            }),
            false,
        ) {
            Update::Updated(plan) => plan,
            Update::Unchanged => panic!("expected Updated"),
        };
        assert_eq!(plan.render(), "[x] read\n[~] edit\n[ ] build");
    }

    #[test]
    fn an_empty_list_renders_the_restored_string() {
        let plan = Plan::new(Some("[ ] restored step".to_string()), None);
        assert_eq!(plan.render(), "[ ] restored step");
    }

    #[test]
    fn an_empty_list_with_no_restore_renders_empty() {
        assert_eq!(Plan::default().render(), "");
    }

    #[test]
    fn a_fresh_todo_write_supersedes_the_restored_render() {
        let plan = Plan::new(Some("[ ] old".to_string()), None);
        let updated = match plan.update(
            "todo_write",
            &json!({ "todos": [{ "content": "new", "status": "pending" }] }),
            false,
        ) {
            Update::Updated(plan) => plan,
            Update::Unchanged => panic!("expected Updated"),
        };
        assert_eq!(updated.render(), "[ ] new");
    }
}
