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

/// A tool's spec in Anthropic tool format: a name, a description, and a JSON
/// Schema `input_schema` (an open edge, so it stays a `serde_json::Value`).
/// Mirrors baud's `Baud.Tool.spec/0` shape. Serializes to exactly its wire
/// shape, so the Conversation's tool-spec overhead estimate counts what a
/// request carries without reaching into an adapter.
///
/// Lives here in the shared content-shapes leaf (alongside [`ContentBlock`]'s
/// `ToolUse`/`ToolResult`, the other wire tool-shapes) rather than in `tool`, so
/// the LLM boundary can carry it on an [`crate::llm::LlmRequest`] without an
/// `llm -> tool` edge - the `tool` capability layer names `Model` (`llm -> `
/// via [`crate::tool::caps::SideQueryRequest`]) and the two would otherwise
/// cycle. `tool` re-exports it (`crate::tool::ToolSpec`) so the tool authoring
/// contract still reads as one home.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One block of a Tool Result's content (ADR-0059): the canonical block-list a
/// Tool Result carries. The common case is a single [`ResultBlock::Text`]; media
/// blocks (image, PDF document) ride only when a tool produces them and the
/// target Model supports the modality, else they degrade to a text placeholder
/// (the read-time and wire-build-time degrade paths, ADR-0059).
///
/// `#[serde(tag = "type")]` matches the Session-Log projection (ADR-0010): a Tool
/// Result round-trips through the log as this tagged array. The Anthropic wire's
/// `image`/`document` shape (`source.base64`) is NOT this internal form - the
/// explicit visitor in `anthropic_messages::request` builds it (ADR-0002); this
/// enum is the domain shape, `data` a base64 string on the media variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultBlock {
    Text { text: String },
    Image { mime: String, data: String },
    Document { mime: String, data: String },
}

impl ResultBlock {
    /// A single Text block - the common case a text tool's result becomes.
    pub fn text(text: impl Into<String>) -> Self {
        ResultBlock::Text { text: text.into() }
    }
}

/// The VERBATIM unsupported-modality placeholder qwen-code emits (qwen v0.16.0
/// `fileUtils.ts` `unsupportedModalityMessage`): the text a media block degrades
/// to when the target Model lacks that modality. Shared by the wire-build-time
/// degrade pass ([`crate::llm::transform`]) and the OpenAI request visitor (whose
/// tool-role messages carry no media, ADR-0059). `modality` is "image" or "pdf";
/// `display_name` names the source so the model can reason about it.
pub fn unsupported_modality_placeholder(modality: &str, display_name: &str) -> String {
    format!(
        "[Unsupported {modality} file: \"{display_name}\". This model does not \
support {modality} input. The read_file tool cannot process this type of file \
either. To handle this file, try using skills if applicable, or any tools \
installed at system wide, or let the user know you cannot process this type of \
file.]"
    )
}

/// The text projection of a block list (ADR-0059): Text blocks concatenated,
/// each media block rendered as a short `[image: <mime>]` / `[document: <mime>]`
/// placeholder. The one place the block-list-to-text rule lives - the single
/// block-list-to-text projection path, read by the UI, the loop-detector,
/// summarize, the Session-Log projection, and the transform's orphan path.
pub fn result_blocks_text(blocks: &[ResultBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ResultBlock::Text { text } => text.clone(),
            ResultBlock::Image { mime, .. } => format!("[image: {mime}]"),
            ResultBlock::Document { mime, .. } => format!("[document: {mime}]"),
        })
        .collect()
}

/// The input modalities a Model accepts beyond text (ADR-0037, ADR-0059): image
/// and PDF. Default all-false, so a Model whose Catalog entry predates the
/// modality fields (the committed data's `#[serde(default)]`) accepts text only
/// and every media block degrades. A copied fact, stamped onto the Model at
/// resolve and onto the [`crate::tool::ToolCtx`] at ctx-build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modalities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub pdf: bool,
}

/// A content block. `#[serde(tag = "type")]` mirrors baud's `:type`
/// discriminator; `rename_all = "snake_case"` matches the atom names
/// (`text`, `tool_use`, `tool_result`, `thinking`).
///
/// A Tool Result's `content` is a [`ResultBlock`] list (ADR-0059): the common
/// case is a single Text block, media reaches the wire when the Model supports
/// it. Read its text projection through [`result_blocks_text`].
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
        content: Vec<ResultBlock>,
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

    /// A Tool Result carrying a single Text block - the common case, so every
    /// existing construction site (a text result) reads unchanged. Media results
    /// use [`ContentBlock::tool_result_blocks`].
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![ResultBlock::text(content)],
            is_error,
        }
    }

    /// A Tool Result carrying an explicit block list (ADR-0059): the media path,
    /// where a tool's result is more than one Text block.
    pub fn tool_result_blocks(
        tool_use_id: impl Into<String>,
        content: Vec<ResultBlock>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
            is_error,
        }
    }

    /// Is this block a Tool Call (`tool_use`)? A property of the content
    /// itself - shared by the Run loop's dispatch and the loop-detector's
    /// tool-signature.
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
#[path = "../tests/content.rs"]
mod tests;
