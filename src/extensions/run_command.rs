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
const TOOL: &str = "run_shell_command";

/// The run_command exit-badge extension.
pub struct RunCommand;

impl Middleware for RunCommand {
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        if token.tool != TOOL {
            return token;
        }
        let content = match token.result.as_ref() {
            Some(result) => result.text_of(),
            None => return token,
        };
        if run_command::parse_timed_out(&content) {
            return token.put_artifact(keys::TIMED_OUT, true);
        }
        match run_command::parse_exit_code(&content) {
            Some(code) => token.put_artifact(keys::EXIT_CODE, code as i64),
            None => token,
        }
    }
}

impl Presenter for RunCommand {
    fn present(
        &self,
        mut item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        // Rewrite a run_command Tool Result summary to the exit badge; without
        // the artifact (or for any other tool) pass through unchanged.
        if let TranscriptItem::ToolResult {
            ref name,
            ref mut summary,
            ..
        } = item
            && name == TOOL
            && let Some(b) = badge(artifacts)
        {
            *summary = b;
        }
        item
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
#[path = "../../tests/unit/extensions/run_command.rs"]
mod tests;
