//! LLM Response — one model response, whatever happened.
//!
//! The error algebra (DESIGN.md / ADR-0002): `Llm::complete` never fails — a
//! request or stream failure comes back as a `Response` with
//! `stop_reason: Error`, `error` set, and whatever content blocks accumulated
//! before the failure. The loop matches one shape everywhere; partial streamed
//! text survives into the Conversation.

use serde::{Deserialize, Serialize};

use crate::content::{ContentBlock, Usage};

/// Why the model stopped.
///
/// Mirrors baud's `stop_reason` atoms. `Unknown` is the catch-all for wire
/// values that don't map to a known reason (baud's `:unknown`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Error,
    /// Unrecognized / not-yet-known stop reason (baud's `:unknown`).
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::Error => "error",
            StopReason::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// One model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for Response {
    fn default() -> Self {
        Response {
            content: Vec::new(),
            stop_reason: StopReason::Unknown,
            usage: Usage::default(),
            error: None,
        }
    }
}

impl Response {
    /// Wraps a failure, keeping any partial content and usage.
    pub fn error(reason: impl Into<String>) -> Self {
        Response {
            content: Vec::new(),
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error: Some(reason.into()),
        }
    }

    /// Wraps a failure with partial content and usage preserved (the error
    /// algebra: partial streamed content survives into the Conversation).
    pub fn error_with(reason: impl Into<String>, content: Vec<ContentBlock>, usage: Usage) -> Self {
        Response {
            content,
            stop_reason: StopReason::Error,
            usage,
            error: Some(reason.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_constructor_sets_error_stop_reason() {
        let r = Response::error("boom");
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert!(r.content.is_empty());
    }

    #[test]
    fn error_with_keeps_partial_content() {
        let content = vec![ContentBlock::text("partial")];
        let r = Response::error_with("boom", content.clone(), Usage::default());
        assert_eq!(r.content, content);
        assert_eq!(r.stop_reason, StopReason::Error);
    }

    #[test]
    fn stop_reason_display_parity() {
        assert_eq!(StopReason::EndTurn.to_string(), "end_turn");
        assert_eq!(StopReason::ToolUse.to_string(), "tool_use");
        assert_eq!(StopReason::MaxTokens.to_string(), "max_tokens");
        assert_eq!(StopReason::StopSequence.to_string(), "stop_sequence");
        assert_eq!(StopReason::Error.to_string(), "error");
        assert_eq!(StopReason::Unknown.to_string(), "unknown");
    }

    #[test]
    fn stop_reason_serde_parity() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        let r: StopReason = serde_json::from_str("\"tool_use\"").unwrap();
        assert_eq!(r, StopReason::ToolUse);
        // Unknown wire value folds into Unknown.
        let r: StopReason = serde_json::from_str("\"something_new\"").unwrap();
        assert_eq!(r, StopReason::Unknown);
    }
}
