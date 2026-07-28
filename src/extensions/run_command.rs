//! The run_command exit-badge Extension (ADR-0007), Stage 3 of the UI plan.
//!
//! [`post_run`](RunCommand::post_run) reads the exit code out of a run_command
//! result - the `[exit code: N]` tail [`crate::tools::run_command::report`]
//! owns - and attaches it (plus a timeout marker) as Artifacts. It never
//! mutates model-facing content. [`present`](RunCommand::present) runs those
//! Artifacts into a one-liner badge on the Tool Result summary: `✓ exit 0`,
//! `✗ exit N`, or `✗ timed out`.
//!
//! The exit code is a SEMANTIC fact carried from execution as an Artifact, not
//! re-parsed at fold time from the merged stdout/stderr: `run_command::report`
//! is the single source for the tail, `run_command::parse_exit_code` its
//! inverse, and this extension the one consumer. The salient `key_arg` (the
//! command) is rendered separately by the transcript's `message_lines`, so the
//! final line reads `⋯ run_command  cargo test · ✓ exit 0`.
//!
//! NOTE / limitation: a FAILING run_command (`is_error: true`) now shows the
//! `✗ exit N` badge, but its stdout/stderr no longer rides the transcript line
//! (it stays in model content). That is the recede-machinery goal, and the
//! Stage 2 review's C2 gap ("large non-diff results don't fold") - accepted for
//! Stage 3.
//!
//! This Extension composes both roles (ADR-0042): a Middleware (`post_run`
//! attaches the exit-code/timeout Artifacts) and a Presenter (`present` runs
//! those Artifacts into the badge).

use std::collections::HashMap;

use serde_json::Value;

use crate::middleware::{Middleware, Token};
use crate::presenter::Presenter;
use crate::tools::run_command;
use crate::view_model::TranscriptItem;

/// The Artifact keys this extension reserves, declared in one place (the diff
/// extension's convention): a producer and consumer that disagree fail to compile.
mod keys {
    /// `artifacts`: the run_command exit code [`super::RunCommand::post_run`]
    /// recovers from the result tail, read back by
    /// [`super::RunCommand::present`] to build the badge.
    pub const EXIT_CODE: &str = "exit_code";

    /// `artifacts`: set when the result is the timeout report, so the badge
    /// reads `✗ timed out` (there is no exit code on a timeout).
    pub const TIMED_OUT: &str = "timed_out";
}

/// The one tool this extension acts on.
const TOOL: &str = "run_command";

/// The run_command exit-badge extension.
pub struct RunCommand;

impl Middleware for RunCommand {
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        if token.tool != TOOL {
            return token;
        }
        let content = match token.result.as_ref() {
            Some(result) => result.content.as_str(),
            None => return token,
        };
        if run_command::parse_timed_out(content) {
            return token.put_artifact(keys::TIMED_OUT, true);
        }
        match run_command::parse_exit_code(content) {
            Some(code) => token.put_artifact(keys::EXIT_CODE, code as i64),
            None => token,
        }
    }
}

impl Presenter for RunCommand {
    fn present(
        &self,
        item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        // Rewrite a run_command Tool Result summary to the exit badge; without
        // the artifact (or for any other tool) pass through unchanged.
        match item {
            TranscriptItem::ToolResult {
                name,
                is_error,
                key_arg,
                summary,
            } if name == TOOL => match badge(artifacts) {
                Some(badge) => TranscriptItem::ToolResult {
                    name,
                    summary: badge,
                    is_error,
                    key_arg,
                },
                None => TranscriptItem::ToolResult {
                    name,
                    summary,
                    is_error,
                    key_arg,
                },
            },
            other => other,
        }
    }
}

// The badge string for a run_command result's Artifacts, or `None` when neither
// marker is present (so the summary passes through). A timeout wins over an exit
// code (a timed-out command has no meaningful code).
fn badge(artifacts: &HashMap<String, Value>) -> Option<String> {
    if artifacts
        .get(keys::TIMED_OUT)
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some("✗ timed out".to_string());
    }
    let code = artifacts.get(keys::EXIT_CODE).and_then(Value::as_i64)?;
    Some(if code == 0 {
        "✓ exit 0".to_string()
    } else {
        format!("✗ exit {code}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::token::TokenResult;
    use crate::tool::ToolCtx;
    use serde_json::json;

    fn ctx() -> ToolCtx {
        ToolCtx {
            root: "/nowhere".into(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
        }
    }

    fn token_with(tool: &str, content: &str, is_error: bool) -> Token {
        let mut token = Token::new(tool, json!({"command": "cargo test"}), ctx());
        token.result = Some(TokenResult {
            content: content.to_string(),
            is_error,
        });
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
            token.result.as_ref().unwrap().content,
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
}
