
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
    assert_eq!(
        item["properties"]["status"]["enum"],
        json!(["pending", "in_progress", "completed"])
    );
    // qwen's wire item properties carry NO description.
    assert!(item["properties"]["content"].get("description").is_none());
    assert!(item["properties"]["status"].get("description").is_none());
    assert!(item["properties"]["id"].get("description").is_none());
}

#[test]
fn description_is_qwens_long_guide_verbatim() {
    let d = TodoWriteTool.spec().description;
    assert!(d.contains("## When to Use This Tool"));
    assert!(d.contains("## When NOT to Use This Tool"));
    assert!(d.contains("## Task States and Management"));
    assert!(d.contains("Implement CSS-in-JS styles for dark theme"));
    assert!(d.contains("Run npm install for me and tell me what happens."));
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
    assert_eq!(missing, r#"Each todo must have a non-empty "id" string."#);

    let blank = run(json!({
        "todos": [{ "id": "  ", "content": "x", "status": "pending" }]
    }))
    .await
    .unwrap_err();
    assert_eq!(blank, r#"Each todo must have a non-empty "id" string."#);
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
