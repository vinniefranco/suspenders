//! LLM Response - one model response, whatever happened.
//!
//! The error algebra (DESIGN.md / ADR-0002): `Llm::complete` never fails - a
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

    /// Is this error a retryable malformed-tool-call generation (ADR-0030)?
    /// A missing error string is not retryable - fail loud by default.
    pub fn is_retryable(&self) -> bool {
        self.error.as_deref().is_some_and(is_retryable_error)
    }
}

/// A conservative classifier over an LLM error string: `true` ONLY for the
/// malformed-tool-call class (ADR-0030) - the local server's
/// `Failed to generate a valid tool call`, which `llm/stream.rs` wraps as
/// `api_stream_error: ...`. Everything else is `false`, fail-loud by default:
/// `Context size has been exceeded` (the KV-pool 500) and transport errors
/// are pointless or a budget problem to retry, not a generation one.
pub fn is_retryable_error(error: &str) -> bool {
    error.contains("Failed to generate a valid tool call")
}

#[cfg(test)]
#[path = "../../tests/unit/llm/response.rs"]
mod tests;
