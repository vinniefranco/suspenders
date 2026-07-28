//! Shared content shapes (DESIGN.md "Shared data shapes").
//!
//! In baud these are plain atom-keyed maps told apart by a `:type`
//! discriminator. In Rust they are serde-tagged domain enums (strong domain
//! enums, `serde_json::Value` only at open edges - ADR: the tool_use `input`
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

    // qual:test_helper
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
    /// itself - shared by the Run loop's dispatch, the Scout's, and the
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

/// Provenance (CONTEXT.md, ADR-0037): the Provider id and Model id that
/// produced an assistant message, stamped as it enters the Conversation and
/// persisted on assistant events in the Session Log. Two plain strings, not
/// the Api: the Api is derivable from them, and provider configs can drift
/// across sessions. Read at request-shaping ([`crate::llm::transform`]):
/// history whose Provenance matches the target Model replays verbatim;
/// history from elsewhere is normalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The Provider's identifier (the scope of the scoped id).
    pub provider: String,
    /// The model's own identifier at that Provider.
    pub model: String,
}

impl Provenance {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Provenance {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// A Conversation message: a role, an ordered list of content blocks, and -
/// on assistant messages a model produced - the Provenance of that model.
/// `None` Provenance means unknown (user messages, Voice-authored markers):
/// the transform pass treats unknown as a cross-Provider mismatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Message {
            role,
            content,
            provenance: None,
        }
    }

    pub fn user(content: Vec<ContentBlock>) -> Self {
        Message::new(Role::User, content)
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message::new(Role::Assistant, content)
    }

    /// An assistant message stamped with the Provenance of the Model that
    /// produced it (CONTEXT.md: Provenance).
    pub fn assistant_from(content: Vec<ContentBlock>, provenance: Provenance) -> Self {
        Message {
            role: Role::Assistant,
            content,
            provenance: Some(provenance),
        }
    }
}

/// Token usage reported by the API. Kept flexible - every field optional -
/// so partial/streamed usage maps deserialize without loss.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    /// A usage carrying only `input_tokens`.
    // qual:test_helper
    pub fn with_input_tokens(input_tokens: u64) -> Self {
        Usage {
            input_tokens: Some(input_tokens),
            ..Usage::default()
        }
    }

    /// The size of the previous request as the server reported it: the sum
    /// of all four figures, absent ones counted as 0. A lower bound for the
    /// token estimate (ADR-0036). `None` when `input_tokens` is absent - a
    /// usage map without it is no signal, not a zero floor.
    pub fn context_floor(&self) -> Option<u64> {
        self.input_tokens.map(|input| {
            input
                + self.output_tokens.unwrap_or(0)
                + self.cache_read_input_tokens.unwrap_or(0)
                + self.cache_creation_input_tokens.unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- context_floor/1 ----

    #[test]
    fn context_floor_sums_all_four_figures() {
        let usage = Usage {
            input_tokens: Some(200),
            output_tokens: Some(300),
            cache_read_input_tokens: Some(90_000),
            cache_creation_input_tokens: Some(1_500),
        };
        assert_eq!(usage.context_floor(), Some(92_000));
    }

    #[test]
    fn context_floor_counts_absent_figures_as_zero() {
        assert_eq!(Usage::with_input_tokens(200).context_floor(), Some(200));
    }

    #[test]
    fn context_floor_is_none_without_input_tokens() {
        // A usage map without input_tokens is no signal, not a zero floor -
        // even when the cache figures are present.
        assert_eq!(Usage::default().context_floor(), None);
        let cache_only = Usage {
            input_tokens: None,
            output_tokens: Some(300),
            cache_read_input_tokens: Some(90_000),
            cache_creation_input_tokens: Some(1_500),
        };
        assert_eq!(cache_only.context_floor(), None);
    }
}
