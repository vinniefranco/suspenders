
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
        tool_fragment(json!({ "index": 1, "id": "c2", "function": { "name": "list_directory" } })),
        tool_fragment(json!({ "index": 0, "function": { "arguments": "{\"path\": \"a\"}" } })),
        tool_fragment(json!({ "index": 1, "function": { "arguments": "{}" } })),
        finish("tool_calls"),
    ]);
    assert_eq!(
        r.content,
        vec![
            ContentBlock::tool_use("c1", "read_file", json!({ "path": "a" })),
            ContentBlock::tool_use("c2", "list_directory", json!({})),
        ]
    );
}

#[test]
fn text_precedes_tool_calls_in_final_content() {
    let r = fold(vec![
        delta(json!({ "content": "Let me check." })),
        tool_fragment(json!({
            "index": 0, "id": "c1",
            "function": { "name": "list_directory", "arguments": "{}" }
        })),
        finish("tool_calls"),
    ]);
    assert_eq!(
        r.content,
        vec![
            ContentBlock::text("Let me check."),
            ContentBlock::tool_use("c1", "list_directory", json!({})),
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
            "function": { "name": "list_directory", "arguments": malformed }
        })),
        finish("tool_calls"),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use(
            "c1",
            "list_directory",
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
        tool_fragment(json!({ "index": 0, "id": "c1", "function": { "name": "list_directory" } })),
        finish("tool_calls"),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use("c1", "list_directory", json!({}))]
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

// --- text-emitted Tool Call fallback (qwen parity) ---

// Folds `events` under an explicit style, then finalizes.
fn fold_styled(style: ToolCallStyle, events: Vec<SseEvent>) -> Response {
    let mut s = StreamState::with_style(style);
    for e in &events {
        s.handle_event(e);
    }
    s.finalize()
}

#[test]
fn text_markup_split_across_deltas_finalizes_to_a_tool_use() {
    // Streaming chunks the markup, so it arrives in several content deltas;
    // finish_reason is "stop", yet a text-emitted call must finalize as
    // ToolUse. Auto (the default) recovers it.
    let r = fold(vec![
        delta(json!({ "content": "<tool_call>\n<function=run_shell_command>\n" })),
        delta(json!({ "content": "<parameter=command>\nmix test\n" })),
        delta(json!({ "content": "</parameter>\n</function>\n</tool_call>" })),
        finish("stop"),
        SseEvent::Done,
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use(
            "text-call-0",
            "run_shell_command",
            json!({ "command": "mix test" })
        )]
    );
    // The "stop" finish_reason is overridden - the model wants a tool.
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[test]
fn text_markup_preamble_becomes_a_leading_text_block() {
    let r = fold(vec![
        delta(json!({ "content": "I need to run the tests:\n\n" })),
        delta(json!({
            "content": "<tool_call>\n<function=run_shell_command>\n<parameter=command>\nmix test\n</parameter>\n</function>\n</tool_call>"
        })),
        finish("stop"),
    ]);
    assert_eq!(
        r.content,
        vec![
            ContentBlock::text("I need to run the tests:"),
            ContentBlock::tool_use(
                "text-call-0",
                "run_shell_command",
                json!({ "command": "mix test" })
            ),
        ]
    );
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[test]
fn a_structured_tool_call_wins_over_coincident_text_markup() {
    // Both channels carry a call: the structured one must win, and the
    // markup stays plain text (never re-parsed into a second ToolUse).
    let r = fold(vec![
        delta(
            json!({ "content": "<tool_call>\n<function=list_directory>\n</function>\n</tool_call>" }),
        ),
        tool_fragment(json!({
            "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\": \"x\"}" }
        })),
        finish("tool_calls"),
    ]);
    assert_eq!(
        r.content,
        vec![
            ContentBlock::text("<tool_call>\n<function=list_directory>\n</function>\n</tool_call>"),
            ContentBlock::tool_use("call_1", "read_file", json!({ "path": "x" })),
        ]
    );
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[test]
fn structured_style_never_parses_text_markup() {
    // The opt-out: the same markup stays plain text, stop_reason unchanged.
    let r = fold_styled(
        ToolCallStyle::Structured,
        vec![
            delta(json!({
                "content": "<tool_call>\n<function=run_shell_command>\n<parameter=command>\nmix test\n</parameter>\n</function>\n</tool_call>"
            })),
            finish("stop"),
        ],
    );
    assert_eq!(r.content.len(), 1);
    assert!(matches!(&r.content[0], ContentBlock::Text { .. }));
    assert_eq!(r.stop_reason, StopReason::EndTurn);
}

#[test]
fn text_style_recovers_like_auto() {
    // Text == Auto for now: it forces the same recovery Auto already does.
    let r = fold_styled(
        ToolCallStyle::Text,
        vec![
            delta(json!({
                "content": "<tool_call>\n<function=list_directory>\n<parameter=path>\n.\n</parameter>\n</function>\n</tool_call>"
            })),
            finish("stop"),
        ],
    );
    assert_eq!(
        r.content,
        vec![ContentBlock::tool_use(
            "text-call-0",
            "list_directory",
            json!({ "path": "." })
        )]
    );
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[test]
fn plain_prose_that_only_mentions_markup_stays_text() {
    // Line-anchor guard: the markup appears only inside a sentence, so the
    // fallback declines and the answer stays plain text.
    let r = fold(vec![
        delta(json!({
            "content": "Done. I could not run mix test - the <tool_call> was withdrawn."
        })),
        finish("stop"),
    ]);
    assert_eq!(
        r.content,
        vec![ContentBlock::text(
            "Done. I could not run mix test - the <tool_call> was withdrawn."
        )]
    );
    assert_eq!(r.stop_reason, StopReason::EndTurn);
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
