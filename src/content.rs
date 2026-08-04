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
/// (`text`, `tool_use`, `tool_result`, `thinking`, `image`, `document`).
///
/// A Tool Result's `content` is a [`ResultBlock`] list (ADR-0059): the common
/// case is a single Text block, media reaches the wire when the Model supports
/// it. Read its text projection through [`result_blocks_text`].
///
/// `Image`/`Document` are first-class USER-message media (ADR-0068, At
/// Expansion): a `@path` mention that resolves to an image or PDF becomes one of
/// these base64 blocks on the user turn, emitted directly by both wire adapters
/// (Anthropic `image`/`document` `source.base64`; OpenAI `image_url`/`file`
/// data-URL) - never a synthesized Tool Call and never a fabricated Tool Result.
/// The wire-build-time degrade pass ([`crate::llm::transform`], ADR-0059)
/// replaces media a captured Model cannot accept with the unsupported-modality
/// placeholder, so At Expansion emits media unconditionally.
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
    Image {
        mime: String,
        data: String,
    },
    Document {
        mime: String,
        data: String,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    #[cfg(test)]
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

    /// A first-class user-message image block (ADR-0068): base64 `data` of the
    /// given `mime`. The image path of At Expansion.
    pub fn image(mime: impl Into<String>, data: impl Into<String>) -> Self {
        ContentBlock::Image {
            mime: mime.into(),
            data: data.into(),
        }
    }

    /// A first-class user-message document (PDF) block (ADR-0068): base64 `data`
    /// of the given `mime`. The document path of At Expansion.
    pub fn document(mime: impl Into<String>, data: impl Into<String>) -> Self {
        ContentBlock::Document {
            mime: mime.into(),
            data: data.into(),
        }
    }

    /// Is this block a Tool Call (`tool_use`)? A property of the content
    /// itself - shared by the Run loop's dispatch and the loop-detector's
    /// tool-signature.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, ContentBlock::ToolUse { .. })
    }

    /// The short text projection of a media block (ADR-0068, ADR-0059): an
    /// [`ContentBlock::Image`] renders as `[image: <mime>]` and a
    /// [`ContentBlock::Document`] as `[document: <mime>]`, mirroring the
    /// [`result_blocks_text`] convention exactly. `None` for every non-media
    /// block, so a text-projection `filter_map` reads a media block as its
    /// placeholder without special-casing each call site. Text/ToolUse/
    /// ToolResult/Thinking each carry their own projection rule where they are
    /// projected and are not this helper's concern.
    pub fn media_placeholder(&self) -> Option<String> {
        match self {
            ContentBlock::Image { mime, .. } => Some(format!("[image: {mime}]")),
            ContentBlock::Document { mime, .. } => Some(format!("[document: {mime}]")),
            _ => None,
        }
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

/// A submitted user prompt (ADR-0068): the ordered content-block list a submit
/// or Steer carries from the Composer to the Conversation. Text-only today (a
/// single [`ContentBlock::Text`]); At Expansion (P3) fills it with the
/// `Image`/`Document` blocks a `@path` mention resolves to. Restricted in
/// spirit to user content - Text/Image/Document - so a Tool Call or Tool Result
/// never rides a prompt.
///
/// [`From<String>`]/[`From<&str>`] wrap a bare string as a single Text block, so
/// existing ergonomic call sites (`agent.submit("foo")`, the Composer's draft)
/// widen the pipe unchanged. [`UserPrompt::text`] is the projection the places
/// that still need a display string read: the transcript user line, the history
/// ring, and the `user_prompt_submit` hook (`&str`).
#[derive(Debug, Clone, PartialEq)]
pub struct UserPrompt {
    blocks: Vec<ContentBlock>,
}

impl UserPrompt {
    /// A prompt over an explicit block list - the At Expansion path (P3), where
    /// a `@path` mention has produced text + media blocks.
    pub fn from_blocks(blocks: Vec<ContentBlock>) -> Self {
        UserPrompt { blocks }
    }

    /// The prompt's content blocks, consumed as they enter the Conversation as
    /// a user [`Message`].
    pub fn into_blocks(self) -> Vec<ContentBlock> {
        self.blocks
    }

    /// The prompt's content blocks, borrowed.
    pub fn blocks(&self) -> &[ContentBlock] {
        &self.blocks
    }

    /// The text projection (ADR-0068): Text blocks concatenated, each media
    /// block rendered as its `[image: <mime>]` / `[document: <mime>]`
    /// placeholder (the [`ContentBlock::media_placeholder`] convention). The one
    /// display-string read - the transcript user line, the history ring, and the
    /// `user_prompt_submit` hook. A pure-text prompt projects to exactly its
    /// text, so a text-only submit is byte-identical to today.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                other => other.media_placeholder(),
            })
            .collect()
    }

    /// The USER-FACING display text (ADR-0068, BUG 4): the Text blocks
    /// concatenated, with media blocks OMITTED entirely (no `[image: <mime>]`
    /// placeholder). This is the text the user actually typed - the `@path`
    /// mention residual - so a transcript / steering line shows clean typed text,
    /// not the wire media projection [`UserPrompt::text`] leaks. A pure-text prompt
    /// is byte-identical to [`UserPrompt::text`]; the two diverge only when media
    /// is present, where display drops it and the wire projection keeps a
    /// `[image: <mime>]` marker.
    pub fn display_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Is this a plain single-Text prompt (no media)? The text-only case the
    /// Session Log persists as the byte-identical `user_text` entry; a media
    /// prompt persists the full block list instead.
    pub fn is_plain_text(&self) -> bool {
        matches!(self.blocks.as_slice(), [ContentBlock::Text { .. }])
    }
}

impl From<String> for UserPrompt {
    fn from(text: String) -> Self {
        UserPrompt {
            blocks: vec![ContentBlock::Text { text }],
        }
    }
}

impl From<&str> for UserPrompt {
    fn from(text: &str) -> Self {
        UserPrompt::from(text.to_string())
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
    #[cfg(test)]
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
