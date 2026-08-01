
use super::*;
use serde_json::json;
use tempfile::TempDir;

const EXPECTED_NAMES: &[&str] = &[
    "todo_write",
    "read_file",
    "list_directory",
    "glob",
    "grep_search",
    "edit",
    "write_file",
    "notebook_edit",
    "run_shell_command",
    "web_fetch",
    "ask_user_question",
    "tool_search",
];

fn ctx(root: &std::path::Path, cap: usize) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), cap)
}

// The text projection of a Tool Result's block list (ADR-0059) - the common
// single-Text-block case, so these result-content assertions read as before.
fn text_of(result: &ToolResult) -> String {
    crate::content::result_blocks_text(&result.content)
}

// ---- all/specs ----

#[test]
fn returns_every_tool_in_prompt_order_todo_write_first() {
    // 13 built-ins: task_stop (P4b, ADR-0063) joined the set, but it is
    // deferred so it does NOT appear on the base wire list (`specs()`).
    assert_eq!(tools().len(), 13);
    let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, EXPECTED_NAMES);
}

#[test]
fn specs_returns_one_spec_per_tool_in_registry_order() {
    let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, EXPECTED_NAMES);
}

#[test]
fn task_stop_is_the_deferred_builtin() {
    // task_stop (P4b, ADR-0063) is the first deferred built-in: it appears in
    // the Deferred Tools summary (so the model can discover it via
    // `tool_search`) but NOT on the base wire list (`specs()`).
    let summary = deferred_summary();
    assert!(summary.iter().any(|(name, _)| name == "task_stop"));
    assert!(!crate::context_files::deferred_tools_section(&summary).is_empty());
    assert!(!specs().iter().any(|s| s.name == "task_stop"));
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

    let target = tmp.path().join("big.txt").to_string_lossy().into_owned();
    let result = execute(
        "read_file",
        &json!({"file_path": target}),
        &ctx(tmp.path(), 100),
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(text_of(&result), "a".repeat(500));
}

#[tokio::test]
async fn execute_unknown_tool_is_an_error_result() {
    let tmp = TempDir::new().unwrap();
    let result = execute("bogus_tool", &json!({}), &ctx(tmp.path(), 100)).await;
    assert!(result.is_error);
    assert!(text_of(&result).contains("unknown tool"));
}

// ---- run ----

#[tokio::test]
async fn run_unknown_tool_is_an_error_result() {
    let tmp = TempDir::new().unwrap();
    let result = run("bogus_tool", &json!({}), &ctx(tmp.path(), 10_000)).await;
    assert!(result.is_error);
    assert!(text_of(&result).contains("unknown tool"));
    assert!(text_of(&result).contains("bogus_tool"));
}

#[tokio::test]
async fn run_ok_maps_to_is_error_false() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("present.txt"), "").unwrap();
    // list_files requires an ABSOLUTE path (qwen ls contract); pass the
    // tempdir's absolute path.
    let result = run(
        "list_directory",
        &json!({"path": tmp.path().to_string_lossy()}),
        &ctx(tmp.path(), 10_000),
    )
    .await;
    assert!(!result.is_error);
    assert!(text_of(&result).contains("present.txt"));
}

#[tokio::test]
async fn run_error_maps_to_is_error_true() {
    let tmp = TempDir::new().unwrap();
    let target = tmp
        .path()
        .join("definitely_missing.txt")
        .to_string_lossy()
        .into_owned();
    let result = run(
        "read_file",
        &json!({"file_path": target}),
        &ctx(tmp.path(), 10_000),
    )
    .await;
    assert!(result.is_error);
    assert!(text_of(&result).contains("enoent"));
}

#[tokio::test]
async fn run_malformed_input_becomes_an_error_result() {
    let tmp = TempDir::new().unwrap();
    let c = ctx(tmp.path(), 10_000);
    assert!(run("read_file", &json!({}), &c).await.is_error);
    assert!(
        run("read_file", &json!({"file_path": 42}), &c)
            .await
            .is_error
    );
    assert!(
        run("edit", &json!({"path": 1, "old_str": 2}), &c)
            .await
            .is_error
    );
    assert!(run("grep_search", &json!({}), &c).await.is_error);
}

#[tokio::test]
async fn run_results_are_shaped_to_the_result_cap() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("big.txt"), "a".repeat(500)).unwrap();

    let target = tmp.path().join("big.txt").to_string_lossy().into_owned();
    let result = run(
        "read_file",
        &json!({"file_path": target}),
        &ctx(tmp.path(), 100),
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(
        text_of(&result),
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
        "run_shell_command",
        &json!({"command": "printf 'START'; printf 'x%.0s' $(seq 500); printf 'END'"}),
        &ctx(tmp.path(), 100),
    )
    .await;
    assert!(!result.is_error);
    assert!(text_of(&result).contains("START"));
    assert!(text_of(&result).contains("chars omitted from the middle of this output"));
    assert!(text_of(&result).contains("[exit code: 0]"));
}
