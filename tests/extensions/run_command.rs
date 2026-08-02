use super::*;
use crate::middleware::token::TokenResult;
use crate::tool::ToolCtx;
use serde_json::json;

fn ctx() -> ToolCtx {
    ToolCtx::for_test("/nowhere".into(), 10_000)
}

fn token_with(tool: &str, content: &str, is_error: bool) -> Token {
    let mut token = Token::new(tool, json!({"command": "cargo test"}), ctx());
    token.result = Some(TokenResult::text(content, is_error));
    token
}

fn run(content: &str, is_error: bool) -> Token {
    RunCommand.post_run(token_with(TOOL, content, is_error), &json!({}))
}

fn result_item(name: &str) -> TranscriptItem {
    TranscriptItem::ToolResult {
        name: name.to_string(),
        summary: "some output (+3 more lines)".to_string(),
        is_error: false,
        key_arg: Some("cargo test".to_string()),
    }
}

// ---- post_run ----

#[test]
fn post_run_attaches_the_exit_code_from_the_report_tail() {
    let token = run("all tests passed\n[exit code: 0]", false);
    assert_eq!(token.artifacts.get(keys::EXIT_CODE), Some(&json!(0)));

    let token = run("boom\n[exit code: 3]", true);
    assert_eq!(token.artifacts.get(keys::EXIT_CODE), Some(&json!(3)));
}

#[test]
fn post_run_marks_a_timeout_and_attaches_no_exit_code() {
    let token = run("[command timed out after 100ms]", true);
    assert_eq!(token.artifacts.get(keys::TIMED_OUT), Some(&json!(true)));
    assert!(!token.artifacts.contains_key(keys::EXIT_CODE));
}

#[test]
fn post_run_never_mutates_model_facing_content() {
    let token = run("output\n[exit code: 0]", false);
    assert_eq!(
        token.result.as_ref().unwrap().text_of(),
        "output\n[exit code: 0]"
    );
}

#[test]
fn post_run_ignores_other_tools_and_missing_tails() {
    let other = RunCommand.post_run(
        token_with("read_file", "output\n[exit code: 0]", false),
        &json!({}),
    );
    assert!(other.artifacts.is_empty());

    let no_tail = run("output with no tail", false);
    assert!(no_tail.artifacts.is_empty());
}

// ---- present ----

#[test]
fn present_renders_the_success_badge() {
    let mut artifacts = HashMap::new();
    artifacts.insert(keys::EXIT_CODE.to_string(), json!(0));
    let presented = RunCommand.present(result_item(TOOL), &artifacts, &json!({}));
    match presented {
        TranscriptItem::ToolResult {
            summary, key_arg, ..
        } => {
            assert_eq!(summary, "✓ exit 0");
            assert_eq!(key_arg.as_deref(), Some("cargo test"));
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
}

#[test]
fn present_renders_the_failure_and_timeout_badges() {
    let mut fail = HashMap::new();
    fail.insert(keys::EXIT_CODE.to_string(), json!(3));
    match RunCommand.present(result_item(TOOL), &fail, &json!({})) {
        TranscriptItem::ToolResult { summary, .. } => assert_eq!(summary, "✗ exit 3"),
        other => panic!("expected a ToolResult, got {other:?}"),
    }

    let mut timeout = HashMap::new();
    timeout.insert(keys::TIMED_OUT.to_string(), json!(true));
    match RunCommand.present(result_item(TOOL), &timeout, &json!({})) {
        TranscriptItem::ToolResult { summary, .. } => assert_eq!(summary, "✗ timed out"),
        other => panic!("expected a ToolResult, got {other:?}"),
    }
}

#[test]
fn present_passes_through_without_the_artifact_or_for_other_tools() {
    // No artifact: the summary survives.
    let passthrough = RunCommand.present(result_item(TOOL), &HashMap::new(), &json!({}));
    assert_eq!(passthrough, result_item(TOOL));

    // Another tool with an exit_code artifact is left alone.
    let mut artifacts = HashMap::new();
    artifacts.insert(keys::EXIT_CODE.to_string(), json!(0));
    let other = RunCommand.present(result_item("read_file"), &artifacts, &json!({}));
    assert_eq!(other, result_item("read_file"));
}
