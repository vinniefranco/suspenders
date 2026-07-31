//! `todo_write(todos)`: records the model's task list (CONTEXT.md: Plan) - the
//! steps for the current goal, each with a stable `id`, its `content`, and its
//! `status`.
//!
//! Faithful port of qwen-code's `todo_write` (tools/todoWrite.ts): the schema
//! (`todoWriteToolSchemaData.parametersJsonSchema`) and the long description
//! (`todoWriteToolDescription`) are verbatim, and the returned `llmContent`
//! carries qwen's exact `<system-reminder>` payload - the normal "Todos have
//! been modified successfully" wrapper embedding the todos JSON, or the "Todo
//! list has been cleared." message when the list is empty. Per-item validation
//! (non-empty `id`, non-empty `content`, valid `status`, unique ids) returns
//! qwen's verbatim messages.
//!
//! Storage is the harness's concern, not this tool's: qwen's disk persistence
//! (`todos/<sessionId>.json`) and its TodoCreated/TodoCompleted hook system are
//! qwen-internal infra, out of the model-facing contract, and not ported. The
//! suspenders UI renders the list from a first-class `TranscriptItem::Todo` that
//! the Todo extension (`src/extensions/todo.rs`) derives from the call input,
//! independent of the string this tool returns.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct TodoWriteTool;

/// qwen's `todoWriteToolDescription` (tools/todoWrite.ts:64-240), verbatim. The
/// `${...}` interpolations in qwen's source resolve to literal tool names in the
/// runtime string; there are none in this description (the examples name tools
/// like "Edit tool" / "grep" as prose, not via interpolation).
const DESCRIPTION: &str = r#"
Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool
Use this tool proactively in these scenarios:

1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations
3. User explicitly requests todo list - When the user directly asks you to use the todo list
4. User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)
5. After receiving new instructions - Immediately capture user requirements as todos
6. When you start working on a task - Mark it as in_progress BEFORE beginning work. Ideally you should only have one todo as in_progress at a time
7. After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
1. There is only a single, straightforward task
2. The task is trivial and tracking it provides no organizational benefit
3. The task can be completed in less than 3 trivial steps
4. The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Examples of When to Use the Todo List

<example>
User: I want to add a dark mode toggle to the application settings. Make sure you run the tests and build when you're done!
Assistant: I'll help add a dark mode toggle to your application settings. Let me create a todo list to track this implementation.
*Creates todo list with the following items:*
1. Create dark mode toggle component in Settings page
2. Add dark mode state management (context/store)
3. Implement CSS-in-JS styles for dark theme
4. Update existing components to support theme switching
5. Run tests and build process, addressing any failures or errors that occur
*Begins working on the first task*

<reasoning>
The assistant used the todo list because:
1. Adding dark mode is a multi-step feature requiring UI, state management, and styling changes
2. The user explicitly requested tests and build be run afterward
3. The assistant inferred that tests and build need to pass by adding "Ensure tests and build succeed" as the final task
</reasoning>
</example>

<example>
User: Help me rename the function getCwd to getCurrentWorkingDirectory across my project
Assistant: Let me first search through your codebase to find all occurrences of 'getCwd'.
*Uses grep or search tools to locate all instances of getCwd in the codebase*
Assistant: I've found 15 instances of 'getCwd' across 8 different files. Let me create a todo list to track these changes.
*Creates todo list with specific items for each file that needs updating*

<reasoning>
The assistant used the todo list because:
1. First, the assistant searched to understand the scope of the task
2. Upon finding multiple occurrences across different files, it determined this was a complex task with multiple steps
3. The todo list helps ensure every instance is tracked and updated systematically
4. This approach prevents missing any occurrences and maintains code consistency
</reasoning>
</example>


<example>
User: I need to implement these features for my e-commerce site: user registration, product catalog, shopping cart, and checkout flow.
Assistant: I'll help implement these features. First, let's add all the features to the todo list.
*Creates a todo list breaking down each feature into specific tasks based on the project architecture*
Assistant: Let's start with user registration. This will involve creating database models, API endpoints, and frontend forms that integrate with your existing authentication system.

<reasoning>
The assistant used the todo list because:
1. The user provided multiple complex features to implement in a comma separated list
2. The todo list helps organize these large features into manageable tasks
3. This approach allows for tracking progress across the entire implementation
</reasoning>
</example>

<example>
User: Can you help optimize my React application? It's rendering slowly and has performance issues.</user>
Assistant: I'll help optimize your React application. First, let me examine your codebase to identify potential performance bottlenecks.
*Reviews component structure, render patterns, state management, and data fetching*
Assistant: After analyzing your codebase, I've identified several performance issues. Let me create a todo list to track our optimization efforts.
*Creates todo list with items like: 1) Implement memoization for expensive calculations in ProductList, 2) Add virtualization for long lists in Dashboard, 3) Optimize image loading in Gallery component, 4) Fix state update loops in ShoppingCart, 5) Review bundle size and implement code splitting*
Let's start by implementing memoization for the expensive calculations in your ProductList component.</assistant>

<reasoning>
The assistant used the todo list because:
1. First, the assistant examined the codebase to identify specific performance issues
2. Based on this analysis, it identified multiple optimization opportunities
3. Performance optimization is a non-trivial task requiring multiple steps
4. The todo list helps methodically track improvements across different components
5. This systematic approach ensures all performance bottlenecks are addressed
</reasoning>
</example>

## Examples of When NOT to Use the Todo List

<example>
User: How do I print 'Hello World' in Python?
Assistant: In Python, you can print "Hello World" with this simple code:

python
print("Hello World")

This will output the text "Hello World" to the console when executed.</assistant>

<reasoning>
The assistant did not use the todo list because this is a single, trivial task that can be completed in one step. There's no need to track multiple tasks or steps for such a straightforward request.
</reasoning>
</example>

<example>
User: What does the git status command do?
Assistant: The git status command shows the current state of your working directory and staging area. It displays which changes have been staged, which haven't, and which files aren't being tracked by Git. This is a fundamental command for understanding the current state of your repository before making commits.

<reasoning>
The assistant did not use the todo list because this is an informational request with no actual coding task to complete. The user is simply asking for an explanation, not for the assistant to perform multiple steps or tasks.
</reasoning>
</example>

<example>
User: Can you add a comment to the calculateTotal function to explain what it does?
Assistant: Sure, let me add a comment to the calculateTotal function to explain what it does.
* Uses the Edit tool to add a comment to the calculateTotal function *

<reasoning>
The assistant did not use the todo list because this is a single, straightforward task confined to one location in the code. Adding a comment doesn't require tracking multiple steps or systematic organization.
</reasoning>
</example>

<example>
User: Run npm install for me and tell me what happens.
Assistant: I'll run the npm install command for you.

*Executes: npm install*

The command completed successfully. Here's the output:
[Output of npm install command]

All dependencies have been installed according to your package.json file.

<reasoning>
The assistant did not use the todo list because this is a single command execution with immediate results. There are no multiple steps to track or organize, making the todo list unnecessary for this straightforward task.
</reasoning>
</example>

## Task States and Management

1. **Task States**: Use these states to track progress:
   - pending: Task not yet started
   - in_progress: Currently working on (limit to ONE task at a time)
   - completed: Task finished successfully

2. **Task Management**:
   - Update task status in real-time as you work
   - Mark tasks complete IMMEDIATELY after finishing (don't batch completions)
   - Only have ONE task in_progress at any time
   - Complete current tasks before starting new ones
   - Remove tasks that are no longer relevant from the list entirely

3. **Task Completion Requirements**:
   - ONLY mark a task as completed when you have FULLY accomplished it
   - If you encounter errors, blockers, or cannot finish, keep the task as in_progress
   - When blocked, create a new task describing what needs to be resolved
   - Never mark a task as completed if:
     - Tests are failing
     - Implementation is partial
     - You encountered unresolved errors
     - You couldn't find necessary files or dependencies

4. **Task Breakdown**:
   - Create specific, actionable items
   - Break complex tasks into smaller, manageable steps
   - Use clear, descriptive task names

When in doubt, use this tool. Being proactive with task management demonstrates attentiveness and ensures you complete all requirements successfully.
"#;

/// One validated todo item's fields, in the order qwen validates them.
struct ItemFields<'a> {
    id: Option<&'a str>,
    content: Option<&'a str>,
    status: Option<&'a str>,
}

fn item_fields(item: &Value) -> ItemFields<'_> {
    ItemFields {
        id: item.get("id").and_then(Value::as_str),
        content: item.get("content").and_then(Value::as_str),
        status: item.get("status").and_then(Value::as_str),
    }
}

/// qwen's `validateToolParams` (tools/todoWrite.ts:565-595), returning the
/// verbatim first failing message or `Ok(())`. The wire `validate` only checks
/// that `todos` is present, so the array-shape and per-item checks live here.
fn validate(todos: &[Value]) -> Result<(), String> {
    for item in todos {
        let fields = item_fields(item);
        match fields.id {
            Some(id) if !id.trim().is_empty() => {}
            _ => return Err(r#"Each todo must have a non-empty "id" string."#.to_string()),
        }
        match fields.content {
            Some(content) if !content.trim().is_empty() => {}
            _ => return Err(r#"Each todo must have a non-empty "content" string."#.to_string()),
        }
        match fields.status {
            Some("pending" | "in_progress" | "completed") => {}
            _ => {
                return Err(
                    r#"Each todo must have a valid "status" (pending, in_progress, completed)."#
                        .to_string(),
                );
            }
        }
    }

    let ids: Vec<&str> = todos.iter().filter_map(|t| t.get("id")?.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    if ids.len() != unique.len() {
        return Err("Todo IDs must be unique within the array.".to_string());
    }

    Ok(())
}

/// Renders qwen's `llmContent` for a written list (tools/todoWrite.ts:477-492):
/// the cleared message when empty, otherwise the "modified successfully" wrapper
/// embedding `JSON.stringify(finalTodos)`. The suspenders posture drops qwen's
/// post-write-hook reminder (hooks are unported harness infra), so the payload
/// is the base case with no trailing reminder.
fn llm_content(todos: &[Value]) -> String {
    if todos.is_empty() {
        return "Todo list has been cleared.\n\n<system-reminder>\nYour todo list is now empty. \
DO NOT mention this explicitly to the user. You have no pending tasks in your todo list.\n\
</system-reminder>"
            .to_string();
    }
    let todos_json = Value::Array(todos.to_vec()).to_string();
    format!(
        "Todos have been modified successfully. Ensure that you continue to use the todo list to \
track your progress. Please proceed with the current tasks if applicable\n\n<system-reminder>\n\
Your todo list has changed. DO NOT mention this explicitly to the user. Here are the latest \
contents of your todo list:\n\n{todos_json}. Continue on with the tasks at hand if applicable.\n\
</system-reminder>"
    )
}

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: DESCRIPTION.into(),
            // qwen's `todoWriteToolSchemaData.parametersJsonSchema`
            // (tools/todoWrite.ts:33-61), verbatim: item properties carry no
            // description, `todos` is "The updated todo list", the item is
            // `additionalProperties: false`, and item required is
            // ["content","status","id"].
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "minLength": 1
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                },
                                "id": {
                                    "type": "string"
                                }
                            },
                            "required": ["content", "status", "id"],
                            "additionalProperties": false
                        },
                        "description": "The updated todo list"
                    }
                },
                "required": ["todos"],
                "$schema": "http://json-schema.org/draft-07/schema#"
            }),
        }
    }

    async fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        // qwen accepts an empty array (clears the list) and requires `todos` to
        // be an array; the wire `validate` already guaranteed `todos` is present.
        let todos = match input.get("todos") {
            Some(Value::Array(todos)) => todos.as_slice(),
            _ => return Err(r#"Parameter "todos" must be an array."#.to_string()),
        };
        validate(todos)?;
        Ok(llm_content(todos))
    }
}

#[cfg(test)]
mod tests {
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
}
