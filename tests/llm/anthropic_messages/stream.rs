
use super::*;
use crate::llm::{malformed_input_marker, malformed_tool_input};
use serde_json::json;

// Event constructors mirroring the baud test helpers.
fn ms() -> SseEvent {
    SseEvent::event(
        "message_start",
        json!({ "message": { "usage": { "input_tokens": 5 } } }),
    )
}
fn bs(index: i64, block: Value) -> SseEvent {
    SseEvent::event(
        "content_block_start",
        json!({ "index": index, "content_block": block }),
    )
}
fn bd(index: i64, delta: Value) -> SseEvent {
    SseEvent::event(
        "content_block_delta",
        json!({ "index": index, "delta": delta }),
    )
}
fn bstop(index: i64) -> SseEvent {
    SseEvent::event("content_block_stop", json!({ "index": index }))
}
fn md(stop_reason: &str, usage: Value) -> SseEvent {
    SseEvent::event(
        "message_delta",
        json!({ "delta": { "stop_reason": stop_reason }, "usage": usage }),
    )
}
/// A named event without an index (LM Studio quirk).
fn nm(name: &str, data: Value) -> SseEvent {
    SseEvent::event(name, data)
}

/// Folds a sequence of parsed SSE events into a [`Response`] - the pure core,
/// exercised only by these tests (the transport drives the state incrementally).
fn fold_sse(events: impl IntoIterator<Item = SseEvent>) -> Response {
    let mut state = StreamState::new();
    for event in events {
        state.handle_event(&event);
    }
    state.finalize()
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
fn text_blocks_produce_content_and_stop_reason() {
    let r = fold(vec![
        ms(),
        bs(0, json!({ "type": "text", "text": "" })),
        bd(0, json!({ "type": "text_delta", "text": "Hello " })),
        bd(0, json!({ "type": "text_delta", "text": "world" })),
        bstop(0),
        md("end_turn", json!({ "output_tokens": 42 })),
    ]);
    assert_eq!(r.content, vec![ContentBlock::text("Hello world")]);
    assert_eq!(r.stop_reason, StopReason::EndTurn);
}

#[test]
fn thinking_blocks_excluded_from_final_content() {
    let r = fold(vec![
        bs(0, json!({ "type": "thinking" })),
        bd(0, json!({ "type": "thinking_delta", "thinking": "ponder" })),
        bstop(0),
        bs(1, json!({ "type": "text", "text": "" })),
        bd(1, json!({ "type": "text_delta", "text": "Answer" })),
        bstop(1),
        md("end_turn", json!({})),
    ]);
    assert_eq!(r.content, vec![ContentBlock::text("Answer")]);
}

#[test]
fn snapshot_includes_open_thinking_blocks() {
    let s = state(vec![
        bs(0, json!({ "type": "thinking" })),
        bd(
            0,
            json!({ "type": "thinking_delta", "thinking": "thinking" }),
        ),
    ]);
    assert!(
        s.snapshot()
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. }))
    );
}

// --- tool_use ---

#[test]
fn assembles_input_from_partial_json_deltas() {
    let r = fold(vec![
        bs(
            0,
            json!({ "type": "tool_use", "id": "t1", "name": "read_file" }),
        ),
        bd(
            0,
            json!({ "type": "input_json_delta", "partial_json": "{\"path\": \".ex" }),
        ),
        bd(
            0,
            json!({ "type": "input_json_delta", "partial_json": "\"}" }),
        ),
        bstop(0),
        md("tool_use", json!({})),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use(
            "t1",
            "read_file",
            json!({ "path": ".ex" })
        )]
    );
}

#[test]
fn malformed_json_marked_with_sentinel() {
    let malformed = "{\"path\": tru";
    let r = fold(vec![
        bs(
            0,
            json!({ "type": "tool_use", "id": "t1", "name": "list_directory" }),
        ),
        bd(
            0,
            json!({ "type": "input_json_delta", "partial_json": malformed }),
        ),
        bstop(0),
        md("tool_use", json!({})),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use(
            "t1",
            "list_directory",
            malformed_input_marker(malformed)
        )]
    );
    // The boundary vends the fact semantically: the accessor returns the
    // raw unparsed text, without any caller spelling the wire sentinel.
    let ContentBlock::ToolUse { input, .. } = &r.content[0] else {
        panic!("expected tool_use");
    };
    assert_eq!(malformed_tool_input(input), Some(malformed));
}

#[test]
fn valid_tool_input_is_not_malformed() {
    assert_eq!(malformed_tool_input(&json!({ "path": "." })), None);
    assert_eq!(malformed_tool_input(&json!({})), None);
}

#[test]
fn empty_accumulated_json_becomes_empty_map() {
    let r = fold(vec![
        bs(
            0,
            json!({ "type": "tool_use", "id": "t1", "name": "list_directory" }),
        ),
        bstop(0),
        md("tool_use", json!({})),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use("t1", "list_directory", json!({}))]
    );
}

// --- LM Studio quirk: missing index ---

#[test]
fn blocks_without_index_do_not_collapse() {
    let r = fold(vec![
        nm(
            "content_block_start",
            json!({ "content_block": { "type": "text", "text": "" } }),
        ),
        nm(
            "content_block_delta",
            json!({ "delta": { "type": "text_delta", "text": "Let me " } }),
        ),
        nm("content_block_stop", json!({})),
        nm(
            "content_block_start",
            json!({ "content_block": { "type": "tool_use", "id": "t1", "name": "list_directory" } }),
        ),
        nm(
            "content_block_delta",
            json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"path\": \".\"}" } }),
        ),
        nm("content_block_stop", json!({})),
        md("tool_use", json!({})),
    ]);
    assert_eq!(
        r.content,
        vec![
            ContentBlock::text("Let me "),
            ContentBlock::tool_use("t1", "list_directory", json!({ "path": "." })),
        ]
    );
}

#[test]
fn delta_without_index_targets_last_opened_block() {
    let r = fold(vec![
        nm(
            "content_block_start",
            json!({ "content_block": { "type": "text", "text": "" } }),
        ),
        nm(
            "content_block_delta",
            json!({ "delta": { "type": "text_delta", "text": "m1" } }),
        ),
        nm(
            "content_block_start",
            json!({ "content_block": { "type": "text", "text": "" } }),
        ),
        nm(
            "content_block_delta",
            json!({ "delta": { "type": "text_delta", "text": "m2" } }),
        ),
        nm("content_block_stop", json!({})),
        nm("content_block_stop", json!({})),
        md("end_turn", json!({})),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::text("m1"), ContentBlock::text("m2")]
    );
}

// --- error paths ---

#[test]
fn error_sse_event_yields_error_response_with_partial_content() {
    let r = fold(vec![
        bs(0, json!({ "type": "text", "text": "" })),
        bd(0, json!({ "type": "text_delta", "text": "partial" })),
        nm(
            "error",
            json!({ "type": "error", "error": { "type": "overloaded" } }),
        ),
    ]);
    assert_eq!(r.stop_reason, StopReason::Error);
    assert_eq!(r.content, vec![ContentBlock::text("partial")]);
}

#[test]
fn after_error_subsequent_events_ignored() {
    let r = fold(vec![
        nm("error", json!({})),
        bs(0, json!({ "type": "text", "text": "ignored" })),
    ]);
    assert_eq!(r.content, Vec::<ContentBlock>::new());
}

#[test]
fn parse_error_produces_error_response() {
    let r = fold(vec![SseEvent::ParseError("parse_failed".into())]);
    assert_eq!(r.stop_reason, StopReason::Error);
}

// --- stop_reason mapping ---

#[test]
fn known_reasons_map_to_variants() {
    for (wire, expected) in [
        ("end_turn", StopReason::EndTurn),
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxTokens),
        ("stop_sequence", StopReason::StopSequence),
    ] {
        let r = fold(vec![md(wire, json!({}))]);
        assert_eq!(r.stop_reason, expected);
    }
}

#[test]
fn unrecognized_becomes_unknown() {
    let r = fold(vec![md("something_new", json!({}))]);
    assert_eq!(r.stop_reason, StopReason::Unknown);
}

// --- snapshot ---

#[test]
fn snapshot_open_and_closed_blocks_both_appear() {
    let s = state(vec![
        bs(0, json!({ "type": "text", "text": "" })),
        bd(0, json!({ "type": "text_delta", "text": "part1" })),
        bstop(0),
        bs(1, json!({ "type": "text", "text": "" })),
        bd(1, json!({ "type": "text_delta", "text": "part2" })),
    ]);
    assert_eq!(
        s.snapshot(),
        vec![ContentBlock::text("part1"), ContentBlock::text("part2")]
    );
}

#[test]
fn snapshot_open_tool_use_shows_input_null() {
    let s = state(vec![bs(
        0,
        json!({ "type": "tool_use", "id": "t1", "name": "read_file" }),
    )]);
    assert_eq!(
        s.snapshot(),
        vec![ContentBlock::tool_use("t1", "read_file", Value::Null)]
    );
}

// --- usage merge ---

#[test]
fn usage_merges_start_and_delta() {
    let r = fold(vec![
        SseEvent::event(
            "message_start",
            json!({ "message": { "usage": { "input_tokens": 11, "output_tokens": 1 } } }),
        ),
        md("end_turn", json!({ "output_tokens": 42 })),
    ]);
    assert_eq!(r.usage.input_tokens, Some(11));
    assert_eq!(r.usage.output_tokens, Some(42));
}

#[test]
fn usage_parses_all_four_figures_from_message_start() {
    let r = fold(vec![SseEvent::event(
        "message_start",
        json!({ "message": { "usage": {
                "input_tokens": 11,
                "output_tokens": 1,
                "cache_read_input_tokens": 90_000,
                "cache_creation_input_tokens": 1_500
            } } }),
    )]);
    assert_eq!(r.usage.input_tokens, Some(11));
    assert_eq!(r.usage.output_tokens, Some(1));
    assert_eq!(r.usage.cache_read_input_tokens, Some(90_000));
    assert_eq!(r.usage.cache_creation_input_tokens, Some(1_500));
}

#[test]
fn usage_merge_carries_cache_figures_past_message_delta() {
    // message_start supplies input_tokens and the cache figures,
    // message_delta the final output_tokens; the merge holds all four.
    let r = fold(vec![
        SseEvent::event(
            "message_start",
            json!({ "message": { "usage": {
                    "input_tokens": 11,
                    "cache_read_input_tokens": 90_000,
                    "cache_creation_input_tokens": 1_500
                } } }),
        ),
        md("end_turn", json!({ "output_tokens": 42 })),
    ]);
    assert_eq!(
        r.usage,
        Usage {
            input_tokens: Some(11),
            output_tokens: Some(42),
            cache_read_input_tokens: Some(90_000),
            cache_creation_input_tokens: Some(1_500),
        }
    );
}
