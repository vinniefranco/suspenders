//! `todo_write(todos)`: records the model's task list (CONTEXT.md: Plan) - the
//! steps for the current goal, each with its status.
//!
//! The schema mirrors qwen-code's `todo_write`: a `todos` array of `{content,
//! status}` items. The list is the model's voice: this tool never rewrites,
//! parses, or interprets it. Executing the tool returns a short Voice-neutral
//! confirmation; the storage is the harness's concern, not this tool's.
//!
//! qwen-code's item also carries an `id`; we drop it. Nothing in the harness
//! keys on item identity - the list is stored and rendered whole - so the flat
//! `{content, status}` shape is one fewer field for a small model to fill.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::voice;
use serde_json::{Value, json};

pub struct TodoWriteTool;

const DESCRIPTION: &str = "\
Use this tool to create and manage a structured task list for your current coding session. This \
helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\
It also helps the user understand the progress of the task and overall progress of their requests.\n\
\n\
## When to Use This Tool\n\
Use this tool proactively in these scenarios:\n\
\n\
1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions\n\
2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations\n\
3. User explicitly requests todo list - When the user directly asks you to use the todo list\n\
4. User provides multiple tasks - When users provide a list of things to be done (numbered or \
comma-separated)\n\
5. After receiving new instructions - Immediately capture user requirements as todos\n\
6. When you start working on a task - Mark it as in_progress BEFORE beginning work. Ideally you \
should only have one todo as in_progress at a time\n\
7. After completing a task - Mark it as completed and add any new follow-up tasks discovered during \
implementation\n\
\n\
## When NOT to Use This Tool\n\
\n\
Skip using this tool when:\n\
1. There is only a single, straightforward task\n\
2. The task is trivial and tracking it provides no organizational benefit\n\
3. The task can be completed in less than 3 trivial steps\n\
4. The task is purely conversational or informational\n\
\n\
NOTE that you should not use this tool if there is only one trivial task to do. In this case you \
are better off just doing the task directly.\n\
\n\
## Examples of When to Use the Todo List\n\
\n\
<example>\n\
User: I want to add a dark mode toggle to the application settings. Make sure you run the tests \
and build when you're done!\n\
Assistant: I'll help add a dark mode toggle to your application settings. Let me create a todo \
list to track this implementation.\n\
*Creates todo list with the following items:*\n\
1. Create dark mode toggle component in Settings page\n\
2. Add dark mode state management (context/store)\n\
3. Implement the styles for the dark theme\n\
4. Update existing components to support theme switching\n\
5. Run tests and build process, addressing any failures or errors that occur\n\
*Begins working on the first task*\n\
\n\
<reasoning>\n\
The assistant used the todo list because:\n\
1. Adding dark mode is a multi-step feature requiring UI, state management, and styling changes\n\
2. The user explicitly requested tests and build be run afterward\n\
3. The assistant inferred that tests and build need to pass by adding \"Run tests and build\" as \
the final task\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: Help me rename the function getCwd to getCurrentWorkingDirectory across my project\n\
Assistant: Let me first search through your codebase to find all occurrences of 'getCwd'.\n\
*Uses grep to locate all instances of getCwd in the codebase*\n\
Assistant: I've found 15 instances of 'getCwd' across 8 different files. Let me create a todo list \
to track these changes.\n\
*Creates todo list with specific items for each file that needs updating*\n\
\n\
<reasoning>\n\
The assistant used the todo list because:\n\
1. First, the assistant searched to understand the scope of the task\n\
2. Upon finding multiple occurrences across different files, it determined this was a complex task \
with multiple steps\n\
3. The todo list helps ensure every instance is tracked and updated systematically\n\
4. This approach prevents missing any occurrences and maintains code consistency\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: I need to implement these features for my e-commerce site: user registration, product \
catalog, shopping cart, and checkout flow.\n\
Assistant: I'll help implement these features. First, let's add all the features to the todo list.\n\
*Creates a todo list breaking down each feature into specific tasks based on the project \
architecture*\n\
Assistant: Let's start with user registration. This will involve creating database models, API \
endpoints, and frontend forms that integrate with your existing authentication system.\n\
\n\
<reasoning>\n\
The assistant used the todo list because:\n\
1. The user provided multiple complex features to implement in a comma separated list\n\
2. The todo list helps organize these large features into manageable tasks\n\
3. This approach allows for tracking progress across the entire implementation\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: Can you help optimize my application? It's rendering slowly and has performance issues.\n\
Assistant: I'll help optimize your application. First, let me examine your codebase to identify \
potential performance bottlenecks.\n\
*Reviews module structure, render patterns, state management, and data fetching*\n\
Assistant: After analyzing your codebase, I've identified several performance issues. Let me create \
a todo list to track our optimization efforts.\n\
*Creates todo list with items like: 1) Implement memoization for expensive calculations in \
ProductList, 2) Add virtualization for long lists in Dashboard, 3) Optimize image loading in \
Gallery module, 4) Fix state update loops in ShoppingCart, 5) Review bundle size and implement \
code splitting*\n\
Let's start by implementing memoization for the expensive calculations in your ProductList module.\n\
\n\
<reasoning>\n\
The assistant used the todo list because:\n\
1. First, the assistant examined the codebase to identify specific performance issues\n\
2. Based on this analysis, it identified multiple optimization opportunities\n\
3. Performance optimization is a non-trivial task requiring multiple steps\n\
4. The todo list helps methodically track improvements across different modules\n\
5. This systematic approach ensures all performance bottlenecks are addressed\n\
</reasoning>\n\
</example>\n\
\n\
## Examples of When NOT to Use the Todo List\n\
\n\
<example>\n\
User: How do I print 'Hello World' in Python?\n\
Assistant: In Python, you can print \"Hello World\" with this simple code:\n\
\n\
print(\"Hello World\")\n\
\n\
This will output the text \"Hello World\" to the console when executed.\n\
\n\
<reasoning>\n\
The assistant did not use the todo list because this is a single, trivial task that can be \
completed in one step. There's no need to track multiple tasks or steps for such a straightforward \
request.\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: What does the git status command do?\n\
Assistant: The git status command shows the current state of your working directory and staging \
area. It displays which changes have been staged, which haven't, and which files aren't being \
tracked by Git. This is a fundamental command for understanding the current state of your \
repository before making commits.\n\
\n\
<reasoning>\n\
The assistant did not use the todo list because this is an informational request with no actual \
coding task to complete. The user is simply asking for an explanation, not for the assistant to \
perform multiple steps or tasks.\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: Can you add a comment to the calculateTotal function to explain what it does?\n\
Assistant: Sure, let me add a comment to the calculateTotal function to explain what it does.\n\
*Uses edit_file to add a comment to the calculateTotal function*\n\
\n\
<reasoning>\n\
The assistant did not use the todo list because this is a single, straightforward task confined to \
one location in the code. Adding a comment doesn't require tracking multiple steps or systematic \
organization.\n\
</reasoning>\n\
</example>\n\
\n\
<example>\n\
User: Run cargo build for me and tell me what happens.\n\
Assistant: I'll run the cargo build command for you.\n\
\n\
*Executes: cargo build*\n\
\n\
The command completed successfully. Here's the output:\n\
[Output of cargo build command]\n\
\n\
All crates compiled without errors.\n\
\n\
<reasoning>\n\
The assistant did not use the todo list because this is a single command execution with immediate \
results. There are no multiple steps to track or organize, making the todo list unnecessary for \
this straightforward task.\n\
</reasoning>\n\
</example>\n\
\n\
## Task States and Management\n\
\n\
1. **Task States**: Use these states to track progress:\n\
   - pending: Task not yet started\n\
   - in_progress: Currently working on (limit to ONE task at a time)\n\
   - completed: Task finished successfully\n\
\n\
2. **Task Management**:\n\
   - Update task status in real-time as you work\n\
   - Mark tasks complete IMMEDIATELY after finishing (don't batch completions)\n\
   - Only have ONE task in_progress at any time\n\
   - Complete current tasks before starting new ones\n\
   - Remove tasks that are no longer relevant from the list entirely\n\
\n\
3. **Task Completion Requirements**:\n\
   - ONLY mark a task as completed when you have FULLY accomplished it\n\
   - If you encounter errors, blockers, or cannot finish, keep the task as in_progress\n\
   - When blocked, create a new task describing what needs to be resolved\n\
   - Never mark a task as completed if:\n\
     - Tests are failing\n\
     - Implementation is partial\n\
     - You encountered unresolved errors\n\
     - You couldn't find necessary files or dependencies\n\
\n\
4. **Task Breakdown**:\n\
   - Create specific, actionable items\n\
   - Break complex tasks into smaller, manageable steps\n\
   - Use clear, descriptive task names\n\
\n\
When in doubt, use this tool. Being proactive with task management demonstrates attentiveness and \
ensures you complete all requirements successfully.\n\
\n\
Each item is just content (the step) and status (pending, in_progress, or completed) - there is no \
id field. Send the whole list every time; this replaces the previous list. The list is your voice: \
it is stored and shown to the user verbatim, not parsed or rewritten.";

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The updated todo list: every step and its status. Send the \
                            whole list on each call; it replaces the previous one.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The task, in your own words.",
                                    "minLength": 1
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "pending (not started), in_progress (exactly \
                                        one at a time), or completed (fully finished)."
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        match input.get("todos") {
            Some(Value::Array(todos)) if !todos.is_empty() => {
                Ok(voice::todos_confirmation().to_string())
            }
            _ => Err("invalid input: todo_write requires a non-empty \"todos\" array".to_string()),
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
        }
    }

    async fn run(input: Value) -> Result<String, String> {
        TodoWriteTool.run(&input, &ctx()).await
    }

    #[test]
    fn spec_requires_a_todos_array_of_content_status_items() {
        let spec = TodoWriteTool.spec();
        assert_eq!(spec.name, "todo_write");
        let schema = &spec.input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["todos"]));
        let todos = &schema["properties"]["todos"];
        assert_eq!(todos["type"], "array");
        let item = &todos["items"];
        assert_eq!(item["required"], json!(["content", "status"]));
        assert_eq!(item["properties"]["content"]["type"], "string");
        assert_eq!(
            item["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
        assert!(spec.description.contains("todo_write") || spec.description.contains("task list"));
    }

    #[tokio::test]
    async fn run_with_a_todos_array_returns_a_short_confirmation() {
        let confirmation = run(json!({
            "todos": [
                { "content": "read the failing test", "status": "in_progress" },
                { "content": "fix it", "status": "pending" },
            ]
        }))
        .await
        .unwrap();
        assert!(confirmation.chars().count() < 120);
        assert!(!confirmation.contains("failing test"));
    }

    #[tokio::test]
    async fn run_rejects_a_missing_or_empty_todos_array() {
        let err = run(json!({})).await.unwrap_err();
        assert!(err.contains("todos"));
        assert!(run(json!({ "todos": [] })).await.is_err());
        assert!(run(json!({ "todos": "nope" })).await.is_err());
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
