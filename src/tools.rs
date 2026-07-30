//! Registry of Suspenders tools.
//!
//! Registry order is prompt order: the order the model sees the tool specs.
//! [`execute`] runs every outcome into a `{content, is_error}` Tool Result, so
//! a tool can never crash the Run. [`run`] adds Shaping on top - the
//! extension-free dispatch path.

pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod run_command;
pub mod shaping;
pub mod todo_write;
pub mod tool_search;
pub mod web_fetch;
pub mod write_file;

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::Value;

/// A Tool Result: the content that enters the Conversation and whether it was
/// an error. Mirrors baud's `Baud.Tools.result/0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

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
        Box::new(run_command::RunCommand),
        Box::new(web_fetch::WebFetch),
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
/// Sources the built-in set only. F8 (MCP) makes the deferred set
/// instance-dependent - MCP tools register on a specific [`ToolRegistry`] and
/// are all deferred - so once MCP lands this section must be sourced from the
/// Run's live registry instead. The single-line throwaway build here is the
/// interim: it owns the same summary logic the live registry would report.
pub fn deferred_summary() -> Vec<(String, String)> {
    crate::tool_registry::ToolRegistry::new(tools()).deferred_summary()
}

/// Runs the named tool with the raw decoded input and the ctx, then Shapes the
/// result to the Result Cap: the extension-free dispatch path. Delegates
/// validation + dispatch to the Run's [`ToolRegistry`] (on the ctx).
pub async fn run(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    let mut result = execute(name, input, ctx).await;
    result.content = shaping::shape(name, input, &result.content, ctx.result_cap);
    result
}

/// Runs the named tool WITHOUT Shaping - the raw result. A thin delegator to
/// the ctx's [`ToolRegistry::execute`], which validates the input against the
/// tool's JSON Schema before dispatch; an unknown tool name and an `Err` return
/// both come back as `is_error` results.
pub async fn execute(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    ctx.registry.execute(name, input, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const EXPECTED_NAMES: &[&str] = &[
        "todo_write",
        "read_file",
        "list_files",
        "glob",
        "grep",
        "edit_file",
        "write_file",
        "run_command",
        "web_fetch",
        "tool_search",
    ];

    fn ctx(root: &std::path::Path, cap: usize) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), cap)
    }

    // ---- all/specs ----

    #[test]
    fn returns_every_tool_in_prompt_order_todo_write_first() {
        assert_eq!(tools().len(), 10);
        let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, EXPECTED_NAMES);
    }

    #[test]
    fn specs_returns_one_spec_per_tool_in_registry_order() {
        let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, EXPECTED_NAMES);
    }

    #[test]
    fn deferred_append_is_a_no_op_for_the_builtin_set() {
        // P1a defers nothing, so the Agent's Deferred Tools append
        // (`deferred_tools_section(&deferred_summary())`) must add nothing to
        // the system prompt. Proves the seam is inert until a phase flips
        // `should_defer`.
        assert!(deferred_summary().is_empty());
        assert!(crate::context_files::deferred_tools_section(&deferred_summary()).is_empty());
    }

    #[test]
    fn every_spec_is_anthropic_tool_format_with_string_keyed_schema() {
        for spec in specs() {
            assert!(!spec.name.is_empty());
            assert!(!spec.description.is_empty());

            let schema = &spec.input_schema;
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
            assert!(schema["required"].is_array());
            for r in schema["required"].as_array().unwrap() {
                assert!(r.is_string());
            }
            for (_key, prop) in schema["properties"].as_object().unwrap() {
                assert!(prop["type"].is_string());
                assert!(
                    prop["description"]
                        .as_str()
                        .map(|d| !d.is_empty())
                        .unwrap_or(false)
                );
            }
            for r in schema["required"].as_array().unwrap() {
                let key = r.as_str().unwrap();
                assert!(schema["properties"].get(key).is_some());
            }
        }
    }

    // ---- execute ----

    #[tokio::test]
    async fn execute_returns_the_raw_result_without_shaping() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.txt"), "a".repeat(500)).unwrap();

        let result = execute(
            "read_file",
            &json!({"path": "big.txt"}),
            &ctx(tmp.path(), 100),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "a".repeat(500));
    }

    #[tokio::test]
    async fn execute_unknown_tool_is_an_error_result() {
        let tmp = TempDir::new().unwrap();
        let result = execute("bogus_tool", &json!({}), &ctx(tmp.path(), 100)).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
    }

    // ---- run ----

    #[tokio::test]
    async fn run_unknown_tool_is_an_error_result() {
        let tmp = TempDir::new().unwrap();
        let result = run("bogus_tool", &json!({}), &ctx(tmp.path(), 10_000)).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
        assert!(result.content.contains("bogus_tool"));
    }

    #[tokio::test]
    async fn run_ok_maps_to_is_error_false() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("present.txt"), "").unwrap();
        let result = run(
            "list_files",
            &json!({"path": "."}),
            &ctx(tmp.path(), 10_000),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("present.txt"));
    }

    #[tokio::test]
    async fn run_error_maps_to_is_error_true() {
        let tmp = TempDir::new().unwrap();
        let result = run(
            "read_file",
            &json!({"path": "definitely_missing.txt"}),
            &ctx(tmp.path(), 10_000),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("enoent"));
    }

    #[tokio::test]
    async fn run_malformed_input_becomes_an_error_result() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path(), 10_000);
        assert!(run("read_file", &json!({}), &c).await.is_error);
        assert!(run("read_file", &json!({"path": 42}), &c).await.is_error);
        assert!(
            run("edit_file", &json!({"path": 1, "old_str": 2}), &c)
                .await
                .is_error
        );
        assert!(run("grep", &json!({}), &c).await.is_error);
    }

    #[tokio::test]
    async fn run_results_are_shaped_to_the_result_cap() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.txt"), "a".repeat(500)).unwrap();

        let result = run(
            "read_file",
            &json!({"path": "big.txt"}),
            &ctx(tmp.path(), 100),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            format!(
                "{}\n[truncated: output is 500 chars, showing the first 100]",
                "a".repeat(100)
            )
        );
    }

    #[tokio::test]
    async fn run_command_results_keep_start_and_end_when_shaped() {
        let tmp = TempDir::new().unwrap();
        let result = run(
            "run_command",
            &json!({"command": "printf 'START'; printf 'x%.0s' $(seq 500); printf 'END'"}),
            &ctx(tmp.path(), 100),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("START"));
        assert!(
            result
                .content
                .contains("chars omitted from the middle of this output")
        );
        assert!(result.content.contains("[exit code: 0]"));
    }
}
