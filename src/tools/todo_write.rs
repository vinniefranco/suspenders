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
//! suspenders UI renders the list from a first-class `TranscriptItem::Todo`: on a
//! successful call this tool parses `todos` with the SAME `plan::parse_todos` the
//! Run-loop's Plan fold uses (ADR-0048: `plan.rs` owns the todo vocabulary; no
//! consumer re-derives it) and attaches a `todos` display Artifact (via
//! [`Tool::run_rich`]), which the Transcript store swaps in for the raw JSON
//! args - independent of the string this tool returns to the model.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::plan::{self, TodoItem};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub struct TodoWriteTool;

/// The wire name of this tool, shared with the Transcript store's Todo swap so
/// the two never drift.
pub const TOOL: &str = "todo_write";

/// The Artifact key the Todo display reserves, declared in one place: a producer
/// (this tool) and consumer (the Transcript store) that disagree fail to
/// *compile*, and a rename touches this module alone.
pub const TODOS: &str = "todos";

/// The Artifact carried to the Transcript store: the parsed task list. Serialized
/// into the `todos` slot; `TodoItem`'s `snake_case` status matches the
/// `todo_write` vocabulary on the wire (ADR-0048, [`plan::TodoItem`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoArtifact {
    pub items: Vec<TodoItem>,
}

/// qwen's `todoWriteToolDescription` (tools/todoWrite.ts:77-92), verbatim. qwen
/// v0.21.4 REPLACED the long v0.16 "When to Use This Tool" guide (numbered
/// scenarios + worked examples) with this short, outcome-oriented block; there
/// are no `${...}` interpolations in it.
const DESCRIPTION: &str = r#"
Use this tool to create and manage a user-visible task list when explicit progress tracking improves clarity.

## When to Use This Tool
Use this tool for work that is complex, ambiguous, or multi-phase; has multiple independent outcomes or important dependencies; benefits from checkpoints; or when the user explicitly asks for a todo list.

Do not use it for simple or single-step work, purely conversational or informational requests, or tasks that can be answered or completed directly unless the user explicitly requests a todo list.

## Planning with Todos

Keep the list short and outcome-oriented. Use a small number of meaningful, logically ordered, verifiable steps. Do not create a separate todo for every error, file, command, or minor edit.

Use blockedBy only when the work has real dependencies. Reference Todo IDs from the same list and keep independent work unblocked.

Keep at most one task in_progress. When a plan exists, keep its statuses current, mark finished work completed, revise the plan when the scope or approach changes, and remove items that are no longer relevant. Do not mark incomplete or blocked work completed.
"#;

/// qwen's `todoWriteToolSchemaData.description` (tools/todoWrite.ts:37-38): the
/// short schema-level summary. suspenders' [`ToolSpec`] carries a single
/// `description` slot (wired to the long [`DESCRIPTION`] above, matching qwen's
/// `todoWriteToolDescription` which is what qwen actually passes to the model),
/// so this schema-level string has no distinct slot on the wire; it is kept here
/// for parity documentation only.
#[allow(dead_code)]
const SCHEMA_DESCRIPTION: &str =
    "Creates and manages a concise, user-visible task list for complex or multi-step work.";

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

/// qwen caps ids and `blockedBy` entries at 500 characters
/// (tools/todoWrite.ts:57,61,167,188).
const MAX_ID_LEN: usize = 500;

/// The `blockedBy` dependency IDs of a todo item, in wire order, or `None` when
/// the field is absent. `Some(Err(_))` is a shape violation qwen rejects: a
/// non-array `blockedBy`, or an entry that is not a non-empty string of at most
/// 500 characters (tools/todoWrite.ts:181-192).
fn blocked_by(item: &Value) -> Option<Result<Vec<&str>, ()>> {
    let value = item.get("blockedBy")?;
    let Value::Array(entries) = value else {
        return Some(Err(()));
    };
    let mut ids = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.as_str() {
            Some(s) if !s.trim().is_empty() && s.len() <= MAX_ID_LEN => ids.push(s),
            _ => return Some(Err(())),
        }
    }
    Some(Ok(ids))
}

/// qwen's `validateTodos` (tools/todoWrite.ts:153-242), returning the verbatim
/// first failing message or `Ok(())`. The wire `validate` only checks that
/// `todos` is present, so the array-shape and per-item checks live here.
fn validate(todos: &[Value]) -> Result<(), String> {
    for item in todos {
        let fields = item_fields(item);
        match fields.id {
            Some(id) if !id.trim().is_empty() && id.len() <= MAX_ID_LEN => {}
            _ => {
                return Err(
                    r#"Each todo must have a non-empty "id" string of at most 500 characters."#
                        .to_string(),
                );
            }
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
        if let Some(Err(())) = blocked_by(item) {
            return Err(
                r#"Each todo "blockedBy" value must be an array of non-empty Todo IDs of at most 500 characters."#
                    .to_string(),
            );
        }
    }

    let ids: Vec<&str> = todos.iter().filter_map(|t| t.get("id")?.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    if ids.len() != unique.len() {
        return Err("Todo IDs must be unique within the array.".to_string());
    }

    // Per-item dependency checks: no duplicate/self references, and every
    // referenced ID must exist (tools/todoWrite.ts:201-215). Every item here has
    // a valid, present id and a well-formed `blockedBy` (checked above).
    for item in todos {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        let deps: Vec<&str> = match blocked_by(item) {
            Some(Ok(deps)) => deps,
            _ => Vec::new(),
        };
        let distinct: std::collections::HashSet<&str> = deps.iter().copied().collect();
        if distinct.len() != deps.len() {
            return Err(format!(
                r#"Todo "{id}" must not contain duplicate blockedBy references."#
            ));
        }
        if deps.contains(&id) {
            return Err(format!(r#"Todo "{id}" must not depend on itself."#));
        }
        if let Some(missing) = deps.iter().find(|d| !unique.contains(*d)) {
            return Err(format!(
                r#"Todo "{id}" references unknown dependency "{missing}"."#
            ));
        }
    }

    // Cycle detection via Kahn's algorithm (tools/todoWrite.ts:217-239): if a
    // topological sort cannot place every node, a dependency cycle exists.
    if has_dependency_cycle(todos) {
        return Err("Todo dependencies must not contain a cycle.".to_string());
    }

    Ok(())
}

/// `true` when the todos' `blockedBy` edges contain a cycle, decided by Kahn's
/// topological sort (tools/todoWrite.ts:217-239): count how many nodes can be
/// ordered when a node is ready only once all its dependencies are placed; a
/// leftover means a cycle. Assumes each item has a valid, present id and
/// well-formed dependencies (already checked by `validate`).
fn has_dependency_cycle(todos: &[Value]) -> bool {
    use std::collections::HashMap;

    let dep_count = |item: &Value| match blocked_by(item) {
        Some(Ok(deps)) => deps.len(),
        _ => 0,
    };

    let mut remaining: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in todos {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        remaining.insert(id, dep_count(item));
        if let Some(Ok(deps)) = blocked_by(item) {
            for dep in deps {
                dependents.entry(dep).or_default().push(id);
            }
        }
    }

    let mut queue: Vec<&str> = todos
        .iter()
        .map(|t| t.get("id").and_then(Value::as_str).unwrap_or_default())
        .filter(|id| remaining.get(id).copied() == Some(0))
        .collect();
    let mut index = 0;
    while index < queue.len() {
        let node = queue[index];
        index += 1;
        if let Some(children) = dependents.get(node) {
            for &child in children {
                let left = remaining.get(child).copied().unwrap_or(1).saturating_sub(1);
                remaining.insert(child, left);
                if left == 0 {
                    queue.push(child);
                }
            }
        }
    }

    queue.len() != todos.len()
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
    // Think (qwen todoWrite.ts:559 `Kind.Think`): ALLOWED in plan mode. It
    // writes, but only to the model's own task record, so qwen keeps it available
    // while planning.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Think
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: DESCRIPTION.into(),
            // qwen's `todoWriteToolSchemaData.parametersJsonSchema`
            // (tools/todoWrite.ts:39-74), verbatim: item content/status carry no
            // description, `id` is capped at `maxLength: 500`, each item may
            // carry a `blockedBy` array of unique `maxLength: 500` Todo IDs,
            // `todos` is "The updated todo list", the item is
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
                                    "type": "string",
                                    "maxLength": 500
                                },
                                "blockedBy": {
                                    "type": "array",
                                    "items": { "type": "string", "maxLength": 500 },
                                    "uniqueItems": true,
                                    "description": "Todo IDs that must be completed before this item"
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

    async fn run_rich(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        // A successful todo_write attaches the parsed task list as the `todos`
        // display Artifact, which the Transcript store swaps for a first-class
        // Todo item (the circle list) rather than the raw `{"todos": ...}` args.
        // The SAME parse the Plan fold runs: a missing/non-array `todos` or an
        // all-malformed list yields nothing, so no Artifact attaches and the raw
        // result passes through.
        let output = ToolOutput::text(self.run(input, ctx).await?);
        Ok(match plan::parse_todos(input) {
            Some(items) if !items.is_empty() => {
                // Fail-open (ADR-0007): a Todo Artifact always serializes, but
                // should that ever break, attach nothing rather than panic - the
                // Transcript store reads `None` and simply shows no todo box.
                match serde_json::to_value(TodoArtifact { items }) {
                    Ok(value) => output.with_artifact(TODOS, value),
                    Err(_) => output,
                }
            }
            _ => output,
        })
    }
}

/// Reads the `todos` display Artifact back out of a Tool Result's Artifacts, or
/// `None` when it is absent or malformed. Read by the Transcript store to decide
/// whether to swap the summary for a [`crate::view_model::TranscriptItem::Todo`].
pub fn read_todos_artifact(artifacts: &HashMap<String, Value>) -> Option<TodoArtifact> {
    let value = artifacts.get(TODOS)?;
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
#[path = "../../tests/tools/todo_write.rs"]
mod tests;
