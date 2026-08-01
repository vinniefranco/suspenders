//! Pure SSE event stream state machine for the Anthropic Messages API.
//!
//! A deterministic fold over parsed SSE events. No I/O, no transport
//! dependency, no throttling. The transport layer parses each `event:`/`data:`
//! frame into an [`SseEvent`] and folds the sequence with [`fold_sse`], or
//! drives it incrementally through [`StreamState`].
//!
//! ## Quirks handled
//!
//! - Servers that omit `"index"` on content-block events (LM Studio quirk):
//!   fall back to a monotonic counter so text + tool_use deltas don't collapse
//!   into one entry; deltas/stops without an index target the last-opened
//!   block.
//! - Malformed tool input JSON: tagged so a mangled Tool Call stays
//!   distinguishable from a valid empty-input call, and vended across the
//!   boundary as a domain signal via [`crate::llm::malformed_tool_input`]
//!   (the sentinel that carries it stays private to the boundary). Empty
//!   accumulated JSON decodes to `{}`.
//! - Open blocks surviving a truncated or errored stream: their partial text
//!   is preserved in the final content (the error algebra).
//! - Thinking blocks: accumulated for the snapshot (UI rendering) and dropped
//!   from the final Response content (they never enter the Conversation).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::content::{ContentBlock, Usage};
use crate::llm::decode_tool_input;
use crate::llm::response::{Response, StopReason};

/// One parsed SSE frame handed to the fold. The transport runs each
/// `event:`/`data:` frame into a [`SseEvent::Event`]; a framing/JSON failure
/// becomes [`SseEvent::ParseError`] (baud's `{:error, reason}`).
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    Event { name: String, data: Value },
    ParseError(String),
}

impl SseEvent {
    pub fn event(name: impl Into<String>, data: Value) -> Self {
        SseEvent::Event {
            name: name.into(),
            data,
        }
    }
}

/// An in-flight content block (accumulating). Distinct from [`ContentBlock`]:
/// a tool_use accumulates raw partial JSON here and is decoded at stop.
#[derive(Debug, Clone, PartialEq)]
enum OpenBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Other,
}

/// The fold state.
#[derive(Debug, Clone, Default)]
pub struct StreamState {
    open: BTreeMap<usize, OpenBlock>,
    /// Finalized blocks by index. `None` marks a dropped block (e.g. `Other`).
    done: BTreeMap<usize, Option<ContentBlock>>,
    stop_reason: Option<String>,
    usage_start: Usage,
    usage_delta: Usage,
    error: Option<String>,
    auto_index: usize,
    current_index: Option<usize>,
}

impl StreamState {
    pub fn new() -> Self {
        StreamState::default()
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
            SseEvent::Event { name, data } => self.handle_named(name, data),
        }
    }

    fn handle_named(&mut self, name: &str, data: &Value) {
        match name {
            "message_start" => {
                if let Some(usage) = data.get("message").and_then(|m| m.get("usage")) {
                    self.usage_start = parse_usage(usage);
                }
            }
            "content_block_start" => {
                let index = index_of(data).unwrap_or(self.auto_index);
                let block = open_block(data.get("content_block").unwrap_or(&Value::Null));
                self.open.insert(index, block);
                self.current_index = Some(index);
                self.auto_index = self.auto_index.max(index + 1);
            }
            "content_block_delta" => {
                let index = index_of(data).or(self.current_index);
                if let Some(index) = index {
                    let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                    self.apply_content_delta(index, &delta);
                }
            }
            "content_block_stop" => {
                let index = index_of(data).or(self.current_index);
                if let Some(index) = index
                    && let Some(block) = self.open.remove(&index)
                {
                    self.done.insert(index, finalize_block(block));
                }
            }
            "message_delta" => {
                if let Some(sr) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(usage) = data.get("usage") {
                    merge_usage(&mut self.usage_delta, &parse_usage(usage));
                }
            }
            "error" => {
                self.error = Some(format!("api_stream_error: {data}"));
            }
            // message_stop, ping, unknown events - no-op.
            _ => {}
        }
    }

    fn apply_content_delta(&mut self, index: usize, delta: &Value) {
        let Some(block) = self.open.get_mut(&index) else {
            return;
        };
        let dtype = delta.get("type").and_then(|v| v.as_str());
        match (dtype, block) {
            (Some("text_delta"), OpenBlock::Text { text }) => {
                if let Some(s) = delta.get("text").and_then(|v| v.as_str()) {
                    text.push_str(s);
                }
            }
            (Some("thinking_delta"), OpenBlock::Thinking { text }) => {
                if let Some(s) = delta.get("thinking").and_then(|v| v.as_str()) {
                    text.push_str(s);
                }
            }
            (Some("input_json_delta"), OpenBlock::ToolUse { json, .. }) => {
                if let Some(s) = delta.get("partial_json").and_then(|v| v.as_str()) {
                    json.push_str(s);
                }
            }
            _ => {}
        }
    }

    /// The accumulated content blocks so far, including open (in-flight)
    /// blocks. Thinking blocks are included so the UI can render them
    /// statelessly; they are dropped from the final Response.
    pub fn snapshot(&self) -> Vec<ContentBlock> {
        let mut by_index: BTreeMap<usize, Option<ContentBlock>> = self.done.clone();
        for (i, block) in &self.open {
            by_index.insert(*i, snapshot_block(block));
        }
        by_index.into_values().flatten().collect()
    }

    /// Finalizes the stream state into a [`Response`].
    ///
    /// Open blocks are finalized (truncated/errored streams keep partial
    /// content). Thinking blocks are dropped from content. Usage is merged from
    /// `message_start` and `message_delta`.
    pub fn finalize(self) -> Response {
        let mut by_index = self.done;
        for (index, block) in self.open {
            by_index
                .entry(index)
                .or_insert_with(|| finalize_block(block));
        }

        let content: Vec<ContentBlock> = by_index
            .into_values()
            .flatten()
            .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
            .collect();

        let mut usage = self.usage_start;
        merge_usage(&mut usage, &self.usage_delta);

        match self.error {
            None => Response {
                content,
                stop_reason: stop_reason_of(self.stop_reason.as_deref()),
                usage,
                error: None,
            },
            Some(error) => Response::error_with(error, content, usage),
        }
    }
}

/// Folds a sequence of parsed SSE events into a [`Response`] - the pure core.
// qual:test_helper
pub fn fold_sse(events: impl IntoIterator<Item = SseEvent>) -> Response {
    let mut state = StreamState::new();
    for event in events {
        state.handle_event(&event);
    }
    state.finalize()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn index_of(data: &Value) -> Option<usize> {
    data.get("index")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
}

fn open_block(cb: &Value) -> OpenBlock {
    match cb.get("type").and_then(|v| v.as_str()) {
        Some("text") => OpenBlock::Text {
            text: cb
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("thinking") => OpenBlock::Thinking {
            text: String::new(),
        },
        Some("tool_use") => OpenBlock::ToolUse {
            id: cb
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: cb
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            json: String::new(),
        },
        _ => OpenBlock::Other,
    }
}

/// The snapshot form of an open block. Thinking and text pass through as-is;
/// an open tool_use shows `input: null`; everything else is dropped.
fn snapshot_block(block: &OpenBlock) -> Option<ContentBlock> {
    match block {
        OpenBlock::Text { text } => Some(ContentBlock::Text { text: text.clone() }),
        OpenBlock::Thinking { text } => Some(ContentBlock::Thinking { text: text.clone() }),
        OpenBlock::ToolUse { id, name, .. } => Some(ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: Value::Null,
        }),
        OpenBlock::Other => None,
    }
}

fn finalize_block(block: OpenBlock) -> Option<ContentBlock> {
    match block {
        OpenBlock::Text { text } => Some(ContentBlock::Text { text }),
        OpenBlock::Thinking { text } => Some(ContentBlock::Thinking { text }),
        OpenBlock::ToolUse { id, name, json } => Some(ContentBlock::ToolUse {
            id,
            name,
            input: decode_tool_input(&json),
        }),
        OpenBlock::Other => None,
    }
}

fn stop_reason_of(wire: Option<&str>) -> StopReason {
    match wire {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::Unknown,
    }
}

/// Parses an Anthropic usage map into the typed [`Usage`].
fn parse_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: v.get("input_tokens").and_then(|n| n.as_u64()),
        output_tokens: v.get("output_tokens").and_then(|n| n.as_u64()),
        cache_read_input_tokens: v.get("cache_read_input_tokens").and_then(|n| n.as_u64()),
        cache_creation_input_tokens: v
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_u64()),
    }
}

/// Merges `delta` over `base` field-by-field (a present field wins). Mirrors
/// baud's `Map.merge`: message_start supplies input_tokens and the cache
/// figures, message_delta the final output_tokens.
fn merge_usage(base: &mut Usage, delta: &Usage) {
    if delta.input_tokens.is_some() {
        base.input_tokens = delta.input_tokens;
    }
    if delta.output_tokens.is_some() {
        base.output_tokens = delta.output_tokens;
    }
    if delta.cache_read_input_tokens.is_some() {
        base.cache_read_input_tokens = delta.cache_read_input_tokens;
    }
    if delta.cache_creation_input_tokens.is_some() {
        base.cache_creation_input_tokens = delta.cache_creation_input_tokens;
    }
}

#[cfg(test)]
#[path = "../../../tests/llm/anthropic_messages/stream.rs"]
mod tests;
