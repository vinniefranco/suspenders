use super::*;
use serde_json::json;

fn ctx() -> ToolCtx {
    ToolCtx::for_test(std::path::PathBuf::from("/nowhere"), 10_000)
}

async fn run(input: Value) -> Result<String, String> {
    TodoWriteTool.run(&input, &ctx()).await
}

#[test]
fn spec_matches_qwens_wire_schema() {
    let spec = TodoWriteTool.spec();
    assert_eq!(spec.name, "todo_write");
    let schema = &spec.input_schema;
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["todos"]));
    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");

    let todos = &schema["properties"]["todos"];
    assert_eq!(todos["type"], "array");
    assert_eq!(todos["description"], "The updated todo list");

    let item = &todos["items"];
    // qwen: item required carries the id, item is additionalProperties:false.
    assert_eq!(item["required"], json!(["content", "status", "id"]));
    assert_eq!(item["additionalProperties"], json!(false));
    assert_eq!(item["properties"]["content"]["type"], "string");
    assert_eq!(item["properties"]["content"]["minLength"], 1);
    assert_eq!(item["properties"]["id"]["type"], "string");
    // qwen v0.21.4 caps the id at 500 chars.
    assert_eq!(item["properties"]["id"]["maxLength"], 500);
    assert_eq!(
        item["properties"]["status"]["enum"],
        json!(["pending", "in_progress", "completed"])
    );
    // qwen v0.21.4 adds a `blockedBy` array of unique, 500-char-capped Todo IDs.
    let blocked = &item["properties"]["blockedBy"];
    assert_eq!(blocked["type"], "array");
    assert_eq!(blocked["items"]["type"], "string");
    assert_eq!(blocked["items"]["maxLength"], 500);
    assert_eq!(blocked["uniqueItems"], json!(true));
    assert_eq!(
        blocked["description"],
        "Todo IDs that must be completed before this item"
    );
    // qwen's wire item content/status properties carry NO description.
    assert!(item["properties"]["content"].get("description").is_none());
    assert!(item["properties"]["status"].get("description").is_none());
    assert!(item["properties"]["id"].get("description").is_none());
}

#[test]
fn description_is_qwens_short_v0_21_4_block_verbatim() {
    let d = TodoWriteTool.spec().description;
    // qwen v0.21.4 replaced the long guide with a short outcome-oriented block.
    assert!(d.contains(
        "Use this tool to create and manage a user-visible task list when explicit progress tracking improves clarity."
    ));
    assert!(d.contains("## When to Use This Tool"));
    assert!(d.contains("## Planning with Todos"));
    assert!(d.contains(
        "Use blockedBy only when the work has real dependencies. Reference Todo IDs from the same list and keep independent work unblocked."
    ));
    // The old long-guide scaffolding and worked examples are gone.
    assert!(!d.contains("## When NOT to Use This Tool"));
    assert!(!d.contains("## Task States and Management"));
    assert!(!d.contains("Implement CSS-in-JS styles for dark theme"));
    assert!(!d.contains("Run npm install for me and tell me what happens."));
    // No suspenders rewrites survive: the "no id field" trailer is gone.
    assert!(!d.contains("there is no id field"));
    assert!(!d.contains("cargo build"));
}

#[tokio::test]
async fn a_written_list_returns_qwens_system_reminder_payload() {
    let out = run(json!({
        "todos": [
            { "id": "1", "content": "read the failing test", "status": "in_progress" },
            { "id": "2", "content": "fix it", "status": "pending" },
        ]
    }))
    .await
    .unwrap();

    assert!(out.starts_with("Todos have been modified successfully."));
    assert!(out.contains("<system-reminder>"));
    assert!(out.contains("Your todo list has changed. DO NOT mention this explicitly"));
    // The embedded JSON is the verbatim todos array.
    assert!(out.contains(
            r#"[{"id":"1","content":"read the failing test","status":"in_progress"},{"id":"2","content":"fix it","status":"pending"}]. Continue on with the tasks at hand if applicable."#
        ));
}

#[tokio::test]
async fn an_empty_list_clears_and_returns_the_cleared_message() {
    let out = run(json!({ "todos": [] })).await.unwrap();
    assert_eq!(
        out,
        "Todo list has been cleared.\n\n<system-reminder>\nYour todo list is now empty. DO NOT \
mention this explicitly to the user. You have no pending tasks in your todo list.\n\
</system-reminder>"
    );
}

#[tokio::test]
async fn a_non_array_todos_is_rejected_with_qwens_message() {
    let err = run(json!({ "todos": "nope" })).await.unwrap_err();
    assert_eq!(err, r#"Parameter "todos" must be an array."#);
}

#[tokio::test]
async fn a_missing_or_empty_id_is_rejected_with_qwens_message() {
    let missing = run(json!({ "todos": [{ "content": "x", "status": "pending" }] }))
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        r#"Each todo must have a non-empty "id" string of at most 500 characters."#
    );

    let blank = run(json!({
        "todos": [{ "id": "  ", "content": "x", "status": "pending" }]
    }))
    .await
    .unwrap_err();
    assert_eq!(
        blank,
        r#"Each todo must have a non-empty "id" string of at most 500 characters."#
    );
}

#[tokio::test]
async fn an_id_over_500_chars_is_rejected_with_qwens_message() {
    let long_id = "x".repeat(501);
    let err = run(json!({
        "todos": [{ "id": long_id, "content": "x", "status": "pending" }]
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        r#"Each todo must have a non-empty "id" string of at most 500 characters."#
    );
}

#[tokio::test]
async fn a_missing_content_is_rejected_with_qwens_message() {
    let err = run(json!({ "todos": [{ "id": "1", "status": "pending" }] }))
        .await
        .unwrap_err();
    assert_eq!(err, r#"Each todo must have a non-empty "content" string."#);
}

#[tokio::test]
async fn an_invalid_status_is_rejected_with_qwens_message() {
    let err = run(json!({
        "todos": [{ "id": "1", "content": "x", "status": "blocked" }]
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        r#"Each todo must have a valid "status" (pending, in_progress, completed)."#
    );
}

#[tokio::test]
async fn duplicate_ids_are_rejected_with_qwens_message() {
    let err = run(json!({
        "todos": [
            { "id": "1", "content": "a", "status": "pending" },
            { "id": "1", "content": "b", "status": "pending" },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(err, "Todo IDs must be unique within the array.");
}

#[tokio::test]
async fn a_valid_blocked_by_reference_is_accepted() {
    // qwen v0.21.4: `blockedBy` may reference other Todo IDs in the same list.
    let out = run(json!({
        "todos": [
            { "id": "1", "content": "lay foundation", "status": "completed" },
            { "id": "2", "content": "build wall", "status": "pending", "blockedBy": ["1"] },
        ]
    }))
    .await
    .unwrap();
    assert!(out.starts_with("Todos have been modified successfully."));
}

#[tokio::test]
async fn a_blocked_by_unknown_id_is_rejected() {
    // A dependency on an ID absent from the list is rejected (qwen parity).
    let err = run(json!({
        "todos": [
            { "id": "2", "content": "build wall", "status": "pending", "blockedBy": ["99"] },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(err, r#"Todo "2" references unknown dependency "99"."#);
}

#[tokio::test]
async fn a_self_blocked_by_reference_is_rejected() {
    let err = run(json!({
        "todos": [
            { "id": "1", "content": "x", "status": "pending", "blockedBy": ["1"] },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(err, r#"Todo "1" must not depend on itself."#);
}

#[tokio::test]
async fn a_duplicate_blocked_by_reference_is_rejected() {
    let err = run(json!({
        "todos": [
            { "id": "1", "content": "a", "status": "completed" },
            { "id": "2", "content": "b", "status": "pending", "blockedBy": ["1", "1"] },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        r#"Todo "2" must not contain duplicate blockedBy references."#
    );
}

#[tokio::test]
async fn a_malformed_blocked_by_value_is_rejected_with_qwens_message() {
    // A non-array `blockedBy` is a shape violation.
    let err = run(json!({
        "todos": [
            { "id": "1", "content": "a", "status": "pending", "blockedBy": "nope" },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(
        err,
        r#"Each todo "blockedBy" value must be an array of non-empty Todo IDs of at most 500 characters."#
    );
}

#[tokio::test]
async fn a_blocked_by_dependency_cycle_is_rejected() {
    // 1 depends on 2, 2 depends on 1: a cycle, rejected (qwen parity).
    let err = run(json!({
        "todos": [
            { "id": "1", "content": "a", "status": "pending", "blockedBy": ["2"] },
            { "id": "2", "content": "b", "status": "pending", "blockedBy": ["1"] },
        ]
    }))
    .await
    .unwrap_err();
    assert_eq!(err, "Todo dependencies must not contain a cycle.");
}

#[tokio::test]
async fn the_todo_write_tool_is_registered() {
    let names: Vec<String> = crate::tools::specs()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert!(names.contains(&"todo_write".to_string()));
}

#[test]
fn the_todo_write_tool_never_requires_approval() {
    assert_eq!(crate::approvals::gate_text("todo_write", &json!({})), None);
}
