//! Pure SSE chunk stream state machine for the OpenAI Chat Completions API.
//!
//! A deterministic fold over parsed SSE chunks. No I/O, no transport
//! dependency, no throttling. The transport layer parses each `data:` frame
//! into an [`SseEvent`] and folds the sequence with [`fold_sse`], or drives it
//! incrementally through [`StreamState`].
//!
//! ## Dialect differences from `anthropic_messages::stream`
//!
//! - No block-lifecycle events: `data: [DONE]` terminates, every other frame
//!   is one chunk whose `choices[0].delta` carries the increments.
//! - One implicit text channel (`delta.content`) and one thinking channel
//!   (`delta.reasoning_content`, the DeepSeek/LM Studio dialect) instead of
//!   indexed blocks; Tool Calls arrive as `delta.tool_calls[]` fragments keyed
//!   by their own `index` (first fragment carries `id` and `function.name`,
//!   the rest append `function.arguments`).
//! - `finish_reason` rides a choice; `usage` rides a final often choices-empty
//!   chunk (present only when the request asked via `stream_options`).
//!
//! The boundary semantics are identical: thinking is accumulated for the
//! snapshot (UI rendering) and dropped from the final Response content (it
//! never enters the Conversation); malformed tool arguments are tagged with
//! the shared malformed-input marker; open state survives a truncated or
//! errored stream as partial content (the error algebra).

use std::collections::BTreeMap;

use serde_json::Value;

use super::text_tool_call::{ParsedCall, extract_tool_calls};
use super::xml_tool_call::{contains_xml_tool_calls, try_recover_xml_tool_calls};
use crate::content::{ContentBlock, Usage};
use crate::llm::response::{Response, StopReason};
use crate::llm::{ToolCallStyle, decode_tool_input, malformed_input_marker};

/// One parsed SSE frame handed to the fold. The transport runs each `data:`
/// body into a [`SseEvent::Chunk`], the `[DONE]` terminator into
/// [`SseEvent::Done`], and a framing/JSON failure into
/// [`SseEvent::ParseError`].
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    Chunk(Value),
    Done,
    ParseError(String),
}

/// An in-flight Tool Call accumulating from `delta.tool_calls[]` fragments.
/// Arguments accumulate as a raw string and are decoded at finalization.
#[derive(Debug, Clone, Default, PartialEq)]
struct OpenToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// The fold state.
#[derive(Debug, Clone, Default)]
pub struct StreamState {
    thinking: String,
    text: String,
    tool_calls: BTreeMap<u64, OpenToolCall>,
    finish_reason: Option<String>,
    usage: Usage,
    error: Option<String>,
    /// How this fold resolves Tool Calls the model emitted as content text
    /// (qwen parity). Set by the transport from the request; [`ToolCallStyle::Auto`]
    /// by default (the fold's own `new`/`default` never sees the request).
    style: ToolCallStyle,
}

impl StreamState {
    pub fn new() -> Self {
        StreamState::default()
    }

    /// The fold with an explicit Tool Call style (the transport's entry): the
    /// request's `tool_call_style` decides whether [`finalize`] falls back to
    /// parsing the content text when the structured channel is empty.
    ///
    /// [`finalize`]: StreamState::finalize
    pub fn with_style(style: ToolCallStyle) -> Self {
        StreamState {
            style,
            ..StreamState::default()
        }
    }

    /// Folds one parsed SSE event. Once an error is recorded, subsequent
    /// events are ignored (the error takes precedence - the error algebra).
    pub fn handle_event(&mut self, event: &SseEvent) {
        if self.error.is_some() {
            return;
        }

        match event {
            SseEvent::ParseError(reason) => {
                self.error = Some(reason.clone());
            }
            // Termination is the transport's concern; the fold has nothing
            // left to record.
            SseEvent::Done => {}
            SseEvent::Chunk(chunk) => self.handle_chunk(chunk),
        }
    }

    fn handle_chunk(&mut self, chunk: &Value) {
        // Some OpenAI-compatible servers surface failure as an error object
        // in a data line rather than an HTTP status (the error algebra:
        // failure is data, partial content survives).
        if let Some(err) = chunk.get("error") {
            self.error = Some(format!("api_stream_error: {err}"));
            return;
        }
        // Usage rides the final chunk when stream_options asked for it; that
        // chunk's `choices` is typically empty, so read it before the choice.
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = parse_usage(usage);
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(s) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            self.thinking.push_str(s);
        }
        if let Some(s) = delta.get("content").and_then(|v| v.as_str()) {
            self.text.push_str(s);
        }
        if let Some(fragments) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for fragment in fragments {
                self.apply_tool_fragment(fragment);
            }
        }
    }

    /// Accumulates one `delta.tool_calls[]` fragment. Fragments are keyed by
    /// `index`: the first carries `id` and `function.name`, subsequent ones
    /// append `function.arguments`. A fragment without an index targets the
    /// last opened entry (defensive - mirrors the missing-index quirk the
    /// anthropic fold absorbs).
    fn apply_tool_fragment(&mut self, fragment: &Value) {
        let index = fragment
            .get("index")
            .and_then(|v| v.as_u64())
            .or_else(|| self.tool_calls.keys().next_back().copied())
            .unwrap_or(0);
        let entry = self.tool_calls.entry(index).or_default();
        if let Some(id) = fragment.get("id").and_then(|v| v.as_str()) {
            entry.id = id.to_string();
        }
        let function = fragment.get("function").unwrap_or(&Value::Null);
        if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
            entry.name = name.to_string();
        }
        if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
            entry.arguments.push_str(arguments);
        }
    }

    /// The accumulated content blocks so far, including in-flight ones.
    /// Thinking is included so the UI can render it statelessly; it is
    /// dropped from the final Response. An in-flight Tool Call shows
    /// `input: null`, as the anthropic snapshot does.
    pub fn snapshot(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: self.thinking.clone(),
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }
        for call in self.tool_calls.values() {
            blocks.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: Value::Null,
            });
        }
        blocks
    }

    /// Finalizes the stream state into a [`Response`].
    ///
    /// Text precedes Tool Calls (the dialect streams them so). Thinking is
    /// dropped from content. Tool arguments are decoded here - a mangled
    /// accumulation becomes the shared malformed-input marker. A truncated or
    /// errored stream keeps its partial content (the error algebra).
    ///
    /// ## Text-emitted Tool Call fallback (qwen parity)
    ///
    /// When the structured `delta.tool_calls[]` channel came back EMPTY and the
    /// style admits it ([`ToolCallStyle::Auto`]/[`ToolCallStyle::Text`], not
    /// [`ToolCallStyle::Structured`]), the accumulated content text is mined for
    /// a text-emitted call in three tiers of decreasing precedence:
    ///
    /// 1. **Structured** `delta.tool_calls[]` ALWAYS win: the two text tiers run
    ///    only when none arrived, and never touch the structured decode.
    /// 2. **Hermes/Qwen-coder** markup ([`extract_tool_calls`]): a hit replaces
    ///    the text block with the parsed preamble (only when non-empty) followed
    ///    by one `ToolUse` per recovered call.
    /// 3. **Anthropic/Claude XML** `<invoke>` markup
    ///    ([`try_recover_xml_tool_calls`]): tried ONLY when the Hermes path did
    ///    not hit, and only when there is no stream error, a finish reason is
    ///    present, the text is non-empty, and [`contains_xml_tool_calls`] sees
    ///    the dialect. A recovery emits the recovered `remaining_text` (the
    ///    surrounding prose minus the stripped invoke blocks) when non-empty,
    ///    then one `ToolUse` per recovered call. Unlike the Hermes branch (which
    ///    keeps only the leading preamble), the invoke branch keeps qwen's
    ///    `remaining_text`; this divergence is intentional and faithful to each
    ///    dialect's origin.
    ///
    /// Either text tier forces `stop_reason` to `ToolUse` - a text-emitted call
    /// means the model wants a tool even if `finish_reason` was "stop".
    pub fn finalize(self) -> Response {
        let mut content = Vec::new();
        let mut text_call_stop = None;

        let text_style = self.tool_calls.is_empty() && self.style != ToolCallStyle::Structured;

        if text_style && let Some(parse) = extract_tool_calls(&self.text) {
            if !parse.preamble.is_empty() {
                content.push(ContentBlock::text(parse.preamble));
            }
            for (n, call) in parse.calls.into_iter().enumerate() {
                content.push(text_call_block(n, call));
            }
            // A text-emitted call is a tool request regardless of finish_reason.
            text_call_stop = Some(StopReason::ToolUse);
        } else if text_style
            && self.error.is_none()
            && self.finish_reason.is_some()
            && !self.text.is_empty()
            && contains_xml_tool_calls(&self.text)
            && let recovery = try_recover_xml_tool_calls(&self.text)
            && recovery.recovered
        {
            if !recovery.remaining_text.is_empty() {
                content.push(ContentBlock::text(recovery.remaining_text));
            }
            for (n, call) in recovery.calls.into_iter().enumerate() {
                content.push(text_call_block(n, call));
            }
            // A text-emitted call is a tool request regardless of finish_reason.
            text_call_stop = Some(StopReason::ToolUse);
        } else {
            if !self.text.is_empty() {
                content.push(ContentBlock::text(self.text));
            }
            for call in self.tool_calls.into_values() {
                content.push(ContentBlock::ToolUse {
                    id: call.id,
                    name: call.name,
                    input: decode_tool_input(&call.arguments),
                });
            }
        }

        match self.error {
            None => Response {
                content,
                stop_reason: text_call_stop
                    .unwrap_or_else(|| stop_reason_of(self.finish_reason.as_deref())),
                usage: self.usage,
                error: None,
            },
            Some(error) => Response::error_with(error, content, self.usage),
        }
    }
}

/// One text-recovered Tool Call as a [`ContentBlock::ToolUse`]: the id is
/// synthesized (`text-call-{n}` - the wire carried none) and the input routes
/// through the shared malformed-marker discipline so a call whose parsed input
/// is somehow not an object stays distinguishable, exactly as the structured
/// decoder tags a mangled accumulation.
fn text_call_block(n: usize, call: ParsedCall) -> ContentBlock {
    let input = if call.input.is_object() {
        call.input
    } else {
        // The parser always yields an object today; this keeps the malformed
        // discipline intact if that ever changes, mirroring `decode_tool_input`.
        malformed_input_marker(&call.input.to_string())
    };
    ContentBlock::ToolUse {
        id: format!("text-call-{n}"),
        name: call.name,
        input,
    }
}

impl crate::llm::transport::SseFold for StreamState {
    type Event = SseEvent;

    fn handle_event(&mut self, event: &SseEvent) {
        StreamState::handle_event(self, event);
    }

    fn snapshot(&self) -> Vec<ContentBlock> {
        StreamState::snapshot(self)
    }

    fn finalize(self) -> Response {
        StreamState::finalize(self)
    }

    fn stream_error(message: String) -> SseEvent {
        SseEvent::ParseError(message)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Maps the dialect's `finish_reason` conservatively: unknown values (e.g.
/// `content_filter`) become [`StopReason::Unknown`], never a panic.
fn stop_reason_of(wire: Option<&str>) -> StopReason {
    match wire {
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls") => StopReason::ToolUse,
        _ => StopReason::Unknown,
    }
}

/// Parses a Chat Completions usage map into the typed [`Usage`]:
/// `prompt_tokens` is input, `completion_tokens` output, and
/// `prompt_tokens_details.cached_tokens` the cache read when present. The
/// dialect reports no cache-creation figure.
fn parse_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: v.get("prompt_tokens").and_then(|n| n.as_u64()),
        output_tokens: v.get("completion_tokens").and_then(|n| n.as_u64()),
        cache_read_input_tokens: v
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|n| n.as_u64()),
        cache_creation_input_tokens: None,
    }
}

#[cfg(test)]
#[path = "../../../tests/llm/openai_completions/stream.rs"]
mod tests;
