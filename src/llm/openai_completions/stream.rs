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

use crate::content::{ContentBlock, Usage};
use crate::llm::decode_tool_input;
use crate::llm::response::{Response, StopReason};

/// One parsed SSE frame handed to the fold. The transport turns each `data:`
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
    pub fn finalize(self) -> Response {
        let mut content = Vec::new();
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

        match self.error {
            None => Response {
                content,
                stop_reason: stop_reason_of(self.finish_reason.as_deref()),
                usage: self.usage,
                error: None,
            },
            Some(error) => Response::error_with(error, content, self.usage),
        }
    }
}

/// Folds a sequence of parsed SSE events into a [`Response`] - the pure core.
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
mod tests {
    use super::*;
    use crate::llm::{malformed_input_marker, malformed_tool_input};
    use serde_json::json;

    // Event constructors mirroring the anthropic stream test helpers.
    fn delta(d: Value) -> SseEvent {
        SseEvent::Chunk(json!({ "choices": [{ "delta": d }] }))
    }
    fn finish(reason: &str) -> SseEvent {
        SseEvent::Chunk(json!({ "choices": [{ "delta": {}, "finish_reason": reason }] }))
    }
    fn usage_chunk(usage: Value) -> SseEvent {
        SseEvent::Chunk(json!({ "choices": [], "usage": usage }))
    }
    fn tool_fragment(fragment: Value) -> SseEvent {
        delta(json!({ "tool_calls": [fragment] }))
    }

    fn fold(events: Vec<SseEvent>) -> Response {
        fold_sse(events)
    }

    fn state(events: Vec<SseEvent>) -> StreamState {
        let mut s = StreamState::new();
        for e in &events {
            s.handle_event(e);
        }
        s
    }

    // --- happy path ---

    #[test]
    fn content_deltas_accumulate_into_one_text_block() {
        let r = fold(vec![
            delta(json!({ "role": "assistant", "content": "" })),
            delta(json!({ "content": "Hello " })),
            delta(json!({ "content": "world" })),
            finish("stop"),
            SseEvent::Done,
        ]);
        assert_eq!(r.content, vec![ContentBlock::text("Hello world")]);
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(r.error, None);
    }

    #[test]
    fn reasoning_content_streams_as_thinking_and_is_excluded_from_content() {
        let events = vec![
            delta(json!({ "reasoning_content": "ponder" })),
            delta(json!({ "reasoning_content": " deeply" })),
            delta(json!({ "content": "Answer" })),
        ];
        // Snapshot renders the thinking block for the UI.
        assert_eq!(
            state(events.clone()).snapshot(),
            vec![
                ContentBlock::Thinking {
                    text: "ponder deeply".into()
                },
                ContentBlock::text("Answer"),
            ]
        );
        // The final Response drops it - Thinking never enters the Conversation.
        let mut events = events;
        events.push(finish("stop"));
        assert_eq!(fold(events).content, vec![ContentBlock::text("Answer")]);
    }

    // --- tool_calls fragments ---

    #[test]
    fn assembles_tool_call_from_fragments_keyed_by_index() {
        let r = fold(vec![
            tool_fragment(json!({
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "read_file", "arguments": "" }
            })),
            tool_fragment(json!({ "index": 0, "function": { "arguments": "{\"pa" } })),
            tool_fragment(json!({ "index": 0, "function": { "arguments": "th\": \"x\"}" } })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![ContentBlock::tool_use(
                "call_1",
                "read_file",
                json!({ "path": "x" })
            )]
        );
        assert_eq!(r.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn parallel_tool_calls_interleave_without_collapsing() {
        let r = fold(vec![
            tool_fragment(json!({ "index": 0, "id": "c1", "function": { "name": "read_file" } })),
            tool_fragment(json!({ "index": 1, "id": "c2", "function": { "name": "list_files" } })),
            tool_fragment(json!({ "index": 0, "function": { "arguments": "{\"path\": \"a\"}" } })),
            tool_fragment(json!({ "index": 1, "function": { "arguments": "{}" } })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![
                ContentBlock::tool_use("c1", "read_file", json!({ "path": "a" })),
                ContentBlock::tool_use("c2", "list_files", json!({})),
            ]
        );
    }

    #[test]
    fn text_precedes_tool_calls_in_final_content() {
        let r = fold(vec![
            delta(json!({ "content": "Let me check." })),
            tool_fragment(json!({
                "index": 0, "id": "c1",
                "function": { "name": "list_files", "arguments": "{}" }
            })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![
                ContentBlock::text("Let me check."),
                ContentBlock::tool_use("c1", "list_files", json!({})),
            ]
        );
    }

    #[test]
    fn a_fragment_without_index_targets_the_last_opened_call() {
        let r = fold(vec![
            tool_fragment(json!({ "id": "c1", "function": { "name": "read_file" } })),
            tool_fragment(json!({ "function": { "arguments": "{\"path\": \".\"}" } })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![ContentBlock::tool_use(
                "c1",
                "read_file",
                json!({ "path": "." })
            )]
        );
    }

    #[test]
    fn malformed_arguments_marked_with_the_shared_sentinel() {
        let malformed = "{\"path\": tru";
        let r = fold(vec![
            tool_fragment(json!({
                "index": 0, "id": "c1",
                "function": { "name": "list_files", "arguments": malformed }
            })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![ContentBlock::tool_use(
                "c1",
                "list_files",
                malformed_input_marker(malformed)
            )]
        );
        let ContentBlock::ToolUse { input, .. } = &r.content[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(malformed_tool_input(input), Some(malformed));
    }

    #[test]
    fn empty_accumulated_arguments_become_an_empty_map() {
        let r = fold(vec![
            tool_fragment(json!({ "index": 0, "id": "c1", "function": { "name": "list_files" } })),
            finish("tool_calls"),
        ]);
        assert_eq!(
            r.content,
            vec![ContentBlock::tool_use("c1", "list_files", json!({}))]
        );
    }

    // --- finish_reason mapping ---

    #[test]
    fn known_reasons_map_to_variants() {
        for (wire, expected) in [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::ToolUse),
        ] {
            assert_eq!(fold(vec![finish(wire)]).stop_reason, expected);
        }
    }

    #[test]
    fn unrecognized_reasons_map_conservatively_to_unknown() {
        assert_eq!(
            fold(vec![finish("content_filter")]).stop_reason,
            StopReason::Unknown
        );
        // No finish_reason at all (a stream that just ended) is Unknown too.
        assert_eq!(fold(vec![]).stop_reason, StopReason::Unknown);
    }

    // --- usage ---

    #[test]
    fn usage_rides_the_final_choices_empty_chunk() {
        let r = fold(vec![
            delta(json!({ "content": "hi" })),
            finish("stop"),
            usage_chunk(json!({
                "prompt_tokens": 11,
                "completion_tokens": 42,
                "prompt_tokens_details": { "cached_tokens": 9_000 }
            })),
            SseEvent::Done,
        ]);
        assert_eq!(r.usage.input_tokens, Some(11));
        assert_eq!(r.usage.output_tokens, Some(42));
        assert_eq!(r.usage.cache_read_input_tokens, Some(9_000));
        assert_eq!(r.usage.cache_creation_input_tokens, None);
    }

    #[test]
    fn usage_without_cache_details_leaves_cache_read_unset() {
        let r = fold(vec![usage_chunk(
            json!({ "prompt_tokens": 5, "completion_tokens": 7 }),
        )]);
        assert_eq!(r.usage.cache_read_input_tokens, None);
    }

    // --- error paths ---

    #[test]
    fn parse_error_yields_error_response_with_partial_content() {
        let r = fold(vec![
            delta(json!({ "content": "partial" })),
            SseEvent::ParseError("sse_parse_failed: boom".into()),
        ]);
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.content, vec![ContentBlock::text("partial")]);
    }

    #[test]
    fn an_error_object_in_a_data_line_yields_error_with_partial_content() {
        let r = fold(vec![
            delta(json!({ "content": "partial thou" })),
            SseEvent::Chunk(json!({ "error": { "message": "overloaded", "code": 503 } })),
        ]);
        assert_eq!(r.stop_reason, StopReason::Error);
        let err = r.error.clone().unwrap();
        assert!(err.contains("api_stream_error"), "error was: {err}");
        assert!(err.contains("overloaded"), "error was: {err}");
        assert_eq!(r.content, vec![ContentBlock::text("partial thou")]);
    }

    #[test]
    fn after_error_subsequent_events_ignored() {
        let r = fold(vec![
            SseEvent::ParseError("boom".into()),
            delta(json!({ "content": "ignored" })),
        ]);
        assert_eq!(r.content, Vec::<ContentBlock>::new());
    }

    #[test]
    fn an_errored_stream_keeps_partial_tool_calls_decoded() {
        let r = fold(vec![
            tool_fragment(json!({
                "index": 0, "id": "c1",
                "function": { "name": "read_file", "arguments": "{\"pa" }
            })),
            SseEvent::ParseError("stream_error: died".into()),
        ]);
        // The truncated arguments decode as malformed, never silently emptied.
        assert_eq!(
            r.content,
            vec![ContentBlock::tool_use(
                "c1",
                "read_file",
                malformed_input_marker("{\"pa")
            )]
        );
    }

    // --- snapshot ---

    #[test]
    fn snapshot_shows_open_tool_calls_with_input_null() {
        let s = state(vec![tool_fragment(json!({
            "index": 0, "id": "c1", "function": { "name": "read_file", "arguments": "{\"pa" }
        }))]);
        assert_eq!(
            s.snapshot(),
            vec![ContentBlock::tool_use("c1", "read_file", Value::Null)]
        );
    }

    // --- chunk shape leniency ---

    #[test]
    fn chunks_without_choices_delta_or_with_null_content_are_no_ops() {
        let r = fold(vec![
            SseEvent::Chunk(json!({})),
            SseEvent::Chunk(json!({ "choices": [] })),
            SseEvent::Chunk(json!({ "choices": [{}] })),
            delta(json!({ "content": Value::Null })),
            delta(json!({ "content": "ok" })),
            finish("stop"),
            SseEvent::Done,
        ]);
        assert_eq!(r.content, vec![ContentBlock::text("ok")]);
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }
}
