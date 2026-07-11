//! Shared content shapes (DESIGN.md "Shared data shapes").
//!
//! In baud these are plain atom-keyed maps told apart by a `:type`
//! discriminator. In Rust they are serde-tagged domain enums (strong domain
//! enums, `serde_json::Value` only at open edges — ADR: the tool_use `input`
//! is the open edge, so it stays a `Value`).
//!
//! Thinking is NEVER stored in messages (CONTEXT.md); the `Thinking` variant
//! exists for the LLM snapshot path, not the Conversation.

use serde::{Deserialize, Serialize};

/// A content block. `#[serde(tag = "type")]` mirrors baud's `:type`
/// discriminator; `rename_all = "snake_case"` matches the atom names
/// (`text`, `tool_use`, `tool_result`, `thinking`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Thinking {
        text: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        }
    }

    /// Is this block a Tool Call (`tool_use`)? A property of the content
    /// itself — shared by the Turn loop's dispatch, the Scout's, and the
    /// empty Governor's reply predicate.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, ContentBlock::ToolUse { .. })
    }
}

/// A message's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A Conversation message: a role and an ordered list of content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Message { role, content }
    }

    pub fn user(content: Vec<ContentBlock>) -> Self {
        Message::new(Role::User, content)
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message::new(Role::Assistant, content)
    }
}

/// Token usage reported by the API. Kept flexible — every field optional —
/// so partial/streamed usage maps deserialize without loss.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}
