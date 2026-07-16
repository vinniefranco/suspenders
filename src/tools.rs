//! Registry of Suspenders tools.
//!
//! Registry order is prompt order: the order the model sees the tool specs.
//! [`execute`] turns every outcome into a `{content, is_error}` Tool Result, so
//! a tool can never crash the Turn. [`run`] adds Shaping on top — the
//! plugin-free dispatch path.

pub mod edit_file;
pub mod explore;
pub mod grep;
pub mod list_files;
pub mod plan;
pub mod read_file;
pub mod run_command;
pub mod shaping;
pub mod web_fetch;
pub mod write_file;

use crate::content::ContentBlock;
use crate::llm::stream::malformed_tool_input;
use crate::tool::{Tool, ToolCtx, ToolSpec, validate};
use serde_json::Value;

/// A Tool Result: the content that enters the Conversation and whether it was
/// an error. Mirrors baud's `Baud.Tools.result/0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

// The registry, in prompt order. The plan Tool leads so a small model sees it
// first and plans early (CONTEXT.md: Plan). Boxed trait objects so the async
// `run` stays object-safe (async-trait).
fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(plan::Plan),
        Box::new(read_file::ReadFile),
        Box::new(list_files::ListFiles),
        Box::new(grep::Grep),
        Box::new(explore::Explore),
        Box::new(edit_file::EditFile),
        Box::new(write_file::WriteFile),
        Box::new(run_command::RunCommand),
        Box::new(web_fetch::WebFetch),
    ]
}

// The Scout's read-only Tool subset (CONTEXT.md: Scout): search and read only.
// Excludes explore (no nesting), plan, and every mutating/command Tool.
fn scout_tools_list() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(read_file::ReadFile),
        Box::new(list_files::ListFiles),
        Box::new(grep::Grep),
    ]
}

/// All tool specs, in prompt order.
pub fn specs() -> Vec<ToolSpec> {
    tools().iter().map(|t| t.spec()).collect()
}

/// The Scout's read-only Tool specs, in registry order.
pub fn scout_specs() -> Vec<ToolSpec> {
    scout_tools_list().iter().map(|t| t.spec()).collect()
}

/// Tool specs for the Verification Pass (ADR-0016): run_command only.
pub fn verification_specs() -> Vec<ToolSpec> {
    specs()
        .into_iter()
        .filter(|s| s.name == "run_command")
        .collect()
}

/// Runs a batch of read-only Tool Calls plugin-free and collects their Tool
/// Result blocks, in emission order.
///
/// This is the read-only collect-results core the Scout's Pass loop repeats:
/// for every `tool_use` block, run the tool through [`run`] (Shaped, no
/// Plugins) and wrap its outcome in a `tool_result` block. Non-`tool_use`
/// blocks are ignored. A malformed-input sentinel (the LLM layer's tag for
/// input JSON that never parsed) is blanked to an empty object so the tool's
/// own validation rejects it rather than acting on undecoded JSON.
///
/// The Turn's batch (`turn::batch`) deliberately does NOT share this: it
/// interleaves the Governor answering arbiter, the Plugin lifecycle, the
/// Approval gate, Ledger recording, and per-result checkpointing around each
/// call — none of which a read-only Scout has. The shared unit is only this
/// plugin-free core.
pub async fn run_read_only(blocks: &[ContentBlock], ctx: &ToolCtx) -> Vec<ContentBlock> {
    let mut results = Vec::new();
    for block in blocks {
        if let ContentBlock::ToolUse { id, name, input } = block {
            let input = sanitize_input(input);
            let result = run(name, &input, ctx).await;
            results.push(ContentBlock::tool_result(
                id,
                result.content,
                result.is_error,
            ));
        }
    }
    results
}

/// Blanks a malformed-input-tagged Tool Call input to an empty object. The LLM
/// layer tags inputs whose JSON never decoded; never run those — let the tool's
/// own validation reject an empty map.
fn sanitize_input(input: &Value) -> Value {
    if malformed_tool_input(input).is_some() {
        return Value::Object(serde_json::Map::new());
    }
    input.clone()
}

/// Runs the named tool with the raw decoded input and the ctx, then Shapes the
/// result to the Result Cap: the plugin-free dispatch path.
pub async fn run(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    let mut result = execute(name, input, ctx).await;
    let start_line = if name == "read_file" {
        input.get("start_line").and_then(|v| v.as_i64())
    } else {
        None
    };
    result.content = shaping::shape(name, &result.content, ctx.result_cap, start_line);
    result
}

/// Runs the named tool WITHOUT Shaping — the raw result. Input is validated
/// against the tool's JSON Schema before execution; an unknown tool name and an
/// `Err` return both come back as `is_error` results.
pub async fn execute(name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    let all = tools();
    let tool = match all.iter().find(|t| t.spec().name == name) {
        Some(t) => t,
        None => {
            return ToolResult {
                content: format!("unknown tool: {name:?}"),
                is_error: true,
            };
        }
    };

    // Validate against the tool's own schema before dispatch. The empty-map
    // case is handled by using an empty object when input is not an object.
    let empty = serde_json::Map::new();
    let input_map = input.as_object().unwrap_or(&empty);
    if let Err(reason) = validate(&tool.spec().input_schema, input_map) {
        return ToolResult {
            content: reason,
            is_error: true,
        };
    }

    match tool.run(input, ctx).await {
        Ok(content) => ToolResult {
            content,
            is_error: false,
        },
        Err(reason) => ToolResult {
            content: reason,
            is_error: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const EXPECTED_NAMES: &[&str] = &[
        "plan",
        "read_file",
        "list_files",
        "grep",
        "explore",
        "edit_file",
        "write_file",
        "run_command",
        "web_fetch",
    ];

    fn ctx(root: &std::path::Path, cap: usize) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: cap,
            command_timeout_ms: 120_000,
            scout: None,
        }
    }

    // ---- all/specs ----

    #[test]
    fn returns_every_tool_in_prompt_order_plan_first() {
        assert_eq!(tools().len(), 9);
        let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, EXPECTED_NAMES);
    }

    #[test]
    fn scout_tools_is_exactly_read_file_list_files_grep() {
        let names: Vec<String> = scout_specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["read_file", "list_files", "grep"]);
    }

    #[test]
    fn scout_specs_mirrors_the_subset_for_the_scouts_llm_request() {
        let names: Vec<String> = scout_specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["read_file", "list_files", "grep"]);
    }

    #[test]
    fn scout_tools_excludes_explore_plan_and_mutating_tools() {
        let names: Vec<String> = scout_specs().iter().map(|s| s.name.clone()).collect();
        for excluded in [
            "explore",
            "plan",
            "edit_file",
            "write_file",
            "run_command",
            "web_fetch",
        ] {
            assert!(!names.contains(&excluded.to_string()));
        }
    }

    #[test]
    fn specs_returns_one_spec_per_tool_in_registry_order() {
        let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, EXPECTED_NAMES);
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

    // ---- sanitize_input ----

    // A tool input the boundary marked malformed is blanked to an empty
    // object, so the read-only tool's own validation rejects it instead of
    // acting on undecoded JSON. The marker is built and read through the
    // boundary's domain accessors, not the wire sentinel.
    #[test]
    fn sanitize_input_empties_a_malformed_marked_input() {
        let tagged = crate::llm::stream::malformed_input_marker("{\"path\": tru");
        assert_eq!(sanitize_input(&tagged), json!({}));

        // A well-formed input passes through untouched.
        let ok = json!({ "path": "lib/widget.ex" });
        assert_eq!(sanitize_input(&ok), ok);
    }

    // ---- run_read_only ----

    // The read-only batch core runs every tool_use block plugin-free and Shaped,
    // collecting one tool_result block per call in emission order; non-tool_use
    // blocks are ignored.
    #[tokio::test]
    async fn run_read_only_collects_a_result_per_tool_use_in_order() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha").unwrap();

        let blocks = vec![
            ContentBlock::text("thinking out loud"),
            ContentBlock::tool_use("t1", "read_file", json!({ "path": "a.txt" })),
            ContentBlock::tool_use("t2", "read_file", json!({ "path": "missing.txt" })),
        ];
        let results = run_read_only(&blocks, &ctx(tmp.path(), 10_000)).await;

        assert_eq!(results.len(), 2);
        match &results[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content, "alpha");
                assert!(!is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
        match &results[1] {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "t2");
                assert!(is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    // A malformed-input-tagged Tool Call is never run on undecoded JSON: it is
    // blanked to an empty object, so the tool's validation rejects it as an
    // error result rather than acting on garbage.
    #[tokio::test]
    async fn run_read_only_rejects_a_malformed_tagged_input() {
        let tmp = TempDir::new().unwrap();
        let blocks = vec![ContentBlock::tool_use(
            "t1",
            "read_file",
            crate::llm::stream::malformed_input_marker("{\"path\": tru"),
        )];
        let results = run_read_only(&blocks, &ctx(tmp.path(), 10_000)).await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            &results[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
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
