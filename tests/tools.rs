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
    "enter_plan_mode",
    "exit_plan_mode",
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
    // 15 built-ins: task_stop (P4b, ADR-0063) is deferred so it does NOT appear
    // on the base wire list (`specs()`); enter_plan_mode (always-visible) and
    // exit_plan_mode (deferred + always_load, so always-declared) DO (ADR-0067).
    assert_eq!(tools().len(), 15);
    let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, EXPECTED_NAMES);
}

#[test]
fn specs_returns_one_spec_per_tool_in_registry_order() {
    let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, EXPECTED_NAMES);
}

// Each built-in declares the Kind its qwen v0.21.4 counterpart carries (ADR-0067),
// so the plan-mode verdict fold reads the same read-only / mutator facts qwen
// does. A drifted Kind here would silently change plan-mode enforcement.
#[test]
fn every_builtin_declares_its_qwen_kind() {
    use crate::approvals::Kind;
    let by_name: std::collections::HashMap<String, Kind> =
        tools().iter().map(|t| (t.spec().name, t.kind())).collect();
    let expect = [
        ("todo_write", Kind::Think),
        ("read_file", Kind::Read),
        ("list_directory", Kind::Search),
        ("glob", Kind::Search),
        ("grep_search", Kind::Search),
        ("edit", Kind::Edit),
        ("write_file", Kind::Edit),
        ("notebook_edit", Kind::Edit),
        ("run_shell_command", Kind::Execute),
        ("web_fetch", Kind::Fetch),
        ("ask_user_question", Kind::Think),
        ("enter_plan_mode", Kind::Think),
        ("exit_plan_mode", Kind::Think),
        ("task_stop", Kind::Other),
        ("tool_search", Kind::Other),
    ];
    for (name, kind) in expect {
        assert_eq!(
            by_name.get(name),
            Some(&kind),
            "{name} must declare {kind:?}"
        );
    }
}

// Each built-in declares the CutPolicy Shaping applies to its oversized Tool
// Result. run_shell_command keeps its head AND tail (the exit code and last
// errors live at the end); read_file cuts at a line boundary with a
// file-absolute resume offset; every other tool takes the default Head. A
// drifted declaration here would silently change a tool's cut markers.
#[test]
fn every_builtin_declares_its_cut_policy() {
    use crate::tool::CutPolicy;
    for tool in tools() {
        let expect = match tool.spec().name.as_str() {
            "read_file" => CutPolicy::HeadWithResume,
            "run_shell_command" => CutPolicy::HeadTail,
            _ => CutPolicy::Head,
        };
        assert_eq!(
            tool.cut_policy(),
            expect,
            "{} must declare {expect:?}",
            tool.spec().name
        );
    }
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
