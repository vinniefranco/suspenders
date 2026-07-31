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
    assert_eq!(plan.render(), "● read\n◐ edit\n○ build");
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

// ---- serde round-trip (ADR-0048: the artifact wire form) ----

#[test]
fn todo_item_serializes_status_as_snake_case_and_round_trips() {
    let items = vec![
        TodoItem {
            content: "read".to_string(),
            status: TodoStatus::Pending,
        },
        TodoItem {
            content: "edit".to_string(),
            status: TodoStatus::InProgress,
        },
        TodoItem {
            content: "build".to_string(),
            status: TodoStatus::Completed,
        },
    ];

    let value = serde_json::to_value(&items).unwrap();
    // The status tokens on the wire match the `todo_write` vocabulary.
    assert_eq!(
        value,
        json!([
            { "content": "read", "status": "pending" },
            { "content": "edit", "status": "in_progress" },
            { "content": "build", "status": "completed" },
        ])
    );

    let back: Vec<TodoItem> = serde_json::from_value(value).unwrap();
    assert_eq!(back, items);
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
    assert_eq!(updated.render(), "○ new");
}
