//! Registry of Suspenders tools.
//!
//! Registry order is prompt order: the order the model sees the tool specs.
//! [`execute`] runs every outcome into a `{content, is_error}` Tool Result, so
//! a tool can never crash the Run. [`run`] adds Shaping on top - the
//! extension-free dispatch path.

pub mod agent;
pub mod ask_user_question;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod notebook_edit;
pub mod read_file;
pub mod run_command;
pub mod shaping;
pub mod skill;
pub mod task_stop;
pub mod todo_write;
pub mod tool_search;
pub mod web_fetch;
pub mod write_file;

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::Value;

/// Re-exported so existing `crate::tools::ToolResult` references keep resolving;
/// the type itself now lives with the Tool contract in [`crate::tool`].
pub use crate::tool::ToolResult;

// The registry builder, in prompt order. The todo_write Tool leads so a small
// model sees it first and records its task list early (CONTEXT.md: Plan). The
// tool_search Tool trails: it is the on-demand discovery seam and is always on
// the wire list (`always_load`). Boxed trait objects so the async `run` stays
// object-safe (async-trait). `pub(crate)` because the ToolRegistry is the one
// that owns this set at runtime; only the Run builds it.
pub(crate) fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(todo_write::TodoWriteTool),
        Box::new(read_file::ReadFile),
        Box::new(list_files::ListFiles),
        Box::new(glob::Glob),
        Box::new(grep::Grep),
        Box::new(edit_file::EditFile),
        Box::new(write_file::WriteFile),
        Box::new(notebook_edit::NotebookEdit),
        Box::new(run_command::RunCommand),
        Box::new(web_fetch::WebFetch),
        Box::new(ask_user_question::AskUserQuestion),
        // task_stop trails with tool_search: both are deferred (discovered via
        // `tool_search`), so neither rides the base wire list (P4b, ADR-0063).
        Box::new(task_stop::TaskStop),
        Box::new(tool_search::ToolSearch),
    ]
}

/// The BASE (non-revealed) wire list, in prompt order: every tool EXCEPT
/// deferred, non-always-load tools (which the model discovers on demand via
/// `tool_search`). Used for the Agent's one-time tool-spec overhead estimate,
/// where zero tools are revealed - the wire list a request would carry at Run
/// start. The Run's live, reveal-aware list comes off the `ToolRegistry`.
pub fn specs() -> Vec<ToolSpec> {
    tools()
        .iter()
        .filter(|t| !(t.should_defer() && !t.always_load()))
        .map(|t| t.spec())
        .collect()
}

/// The `{name, description}` summary of every built-in tool the model must
/// discover on demand (deferred, non-always-load), sorted by name. Feeds the
/// "Deferred Tools" system-prompt section, computed once at Run launch. Empty
/// until a later phase flips `should_defer`.
///
/// Sources the built-in set only - the built-in deferred floor. F8 (MCP) landed
/// (ADR-0056): the Agent now sources its live Deferred Tools section from a
/// per-session [`crate::tool_registry::ToolRegistry::with_shared`] registry over
/// the built-ins PLUS the discovered `mcp__*` tools
/// ([`crate::agent`]'s `init_agent`), so the section the model actually sees
/// includes MCP tools. This free fn stays as the built-in floor these tests
/// document; production reads the live registry instead.
pub fn deferred_summary() -> Vec<(String, String)> {
    crate::tool_registry::ToolRegistry::new(tools()).deferred_summary()
}

/// Runs the named tool with the raw decoded input and the ctx, then Shapes the
/// result to the Result Cap: the extension-free dispatch path. Delegates
/// validation + dispatch to the Run's [`ToolRegistry`] (on the ctx).
pub async fn run(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    let mut result = execute(name, input, ctx).await;
    result.content = shaping::shape(name, input, result.content, ctx.result_cap);
    result
}

/// Runs the named tool WITHOUT Shaping - the raw result. A thin delegator to
/// the ctx's [`ToolRegistry::execute`], which validates the input against the
/// tool's JSON Schema before dispatch; an unknown tool name and an `Err` return
/// both come back as `is_error` results.
pub async fn execute(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    ctx.registry().execute(name, input, ctx).await
}

#[cfg(test)]
#[path = "../tests/tools.rs"]
mod tests;
