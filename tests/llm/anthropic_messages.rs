
use super::*;
use crate::content::ContentBlock;
use crate::content::Message;
use crate::llm::adapter_test_support as ats;
use crate::llm::model::Api;
use crate::llm::response::StopReason;
use crate::llm::{Dispatcher, Llm, malformed_input_marker};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- SSE body helpers (canned Anthropic event stream) ---

fn frame(name: &str, data: Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&data).unwrap()
    )
}

fn sse_body(frames: &[String]) -> String {
    frames.concat()
}

fn message_start(usage: Value) -> String {
    frame(
        "message_start",
        json!({ "type": "message_start", "message": { "id": "msg_test", "usage": usage } }),
    )
}
fn block_start(index: Value, cb: Value) -> String {
    frame(
        "content_block_start",
        json!({ "type": "content_block_start", "index": index, "content_block": cb }),
    )
}
fn block_delta(index: Value, delta: Value) -> String {
    frame(
        "content_block_delta",
        json!({ "type": "content_block_delta", "index": index, "delta": delta }),
    )
}
fn block_stop(index: Value) -> String {
    frame(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": index }),
    )
}
fn message_delta(stop_reason: &str, usage: Value) -> String {
    frame(
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason }, "usage": usage }),
    )
}
fn message_stop() -> String {
    frame("message_stop", json!({ "type": "message_stop" }))
}

fn simple_request() -> LlmRequest {
    LlmRequest::new(
        "You are Baud.",
        vec![Message::user(vec![ContentBlock::text("hi")])],
        vec![],
    )
}

async fn serve_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn provider_for(server: &MockServer) -> Provider {
    Provider {
        id: "local".into(),
        base_url: format!("{}/v1", server.uri()),
        token: "test-token".into(),
        api: Api::AnthropicMessages,
        context_window: Some(64_000),
        custom: true,
    }
}

fn test_model() -> Model {
    Model::new(
        "local",
        "test-model",
        Api::AnthropicMessages,
        64_000,
        16_000,
    )
}

// The production path: the Dispatcher routes on the Model's Api into this
// adapter, so exercising it covers routing AND transport.
fn dispatcher_for(server: &MockServer) -> Dispatcher {
    Dispatcher::new(vec![provider_for(server)])
}

fn no_op() -> impl FnMut(&StreamEvent) + Send {
    |_ev: &StreamEvent| {}
}

// ------------------------------------------------------------------
// (a) Happy path: thinking block then text block
// ------------------------------------------------------------------

#[tokio::test]
async fn streams_thinking_and_text_excludes_thinking_merges_usage() {
    let server = MockServer::start().await;
    serve_sse(
        &server,
        sse_body(&[
            message_start(json!({ "input_tokens": 11, "output_tokens": 1 })),
            block_start(json!(0), json!({ "type": "thinking", "thinking": "" })),
            block_delta(
                json!(0),
                json!({ "type": "thinking_delta", "thinking": "pondering" }),
            ),
            block_delta(
                json!(0),
                json!({ "type": "thinking_delta", "thinking": " deeply" }),
            ),
            block_stop(json!(0)),
            block_start(json!(1), json!({ "type": "text", "text": "" })),
            block_delta(json!(1), json!({ "type": "text_delta", "text": "Hello" })),
            block_delta(json!(1), json!({ "type": "text_delta", "text": " world" })),
            block_stop(json!(1)),
            message_delta("end_turn", json!({ "output_tokens": 42 })),
            message_stop(),
        ]),
    )
    .await;

    let (events, result) =
        ats::run_and_collect(dispatcher_for(&server), &simple_request(), &test_model()).await;

    // At least the first delta (a thinking one) arrived through the callback.
    assert!(
        matches!(events.first().map(|e| &e.delta), Some(Delta::Thinking(s)) if s == "pondering")
    );

    // Final content correct; thinking excluded; usage merged.
    assert_eq!(result.content, vec![ContentBlock::text("Hello world")]);
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.error, None);
    assert_eq!(result.usage.input_tokens, Some(11));
    assert_eq!(result.usage.output_tokens, Some(42));
}

// ------------------------------------------------------------------
// (b) tool_use with input_json_delta split mid-JSON-token
// ------------------------------------------------------------------

#[tokio::test]
async fn assembles_tool_use_input_from_partial_json_split_across_events() {
    let server = MockServer::start().await;
    serve_sse(
            &server,
            sse_body(&[
                message_start(json!({ "input_tokens": 5, "output_tokens": 1 })),
                block_start(json!(0), json!({ "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {} })),
                block_delta(json!(0), json!({ "type": "input_json_delta", "partial_json": "{\"pa" })),
                block_delta(json!(0), json!({ "type": "input_json_delta", "partial_json": "th\": \"x\"}" })),
                block_stop(json!(0)),
                block_start(json!(1), json!({ "type": "tool_use", "id": "toolu_2", "name": "list_directory", "input": {} })),
                block_stop(json!(1)),
                message_delta("tool_use", json!({ "output_tokens": 9 })),
                message_stop(),
            ]),
        )
        .await;

    let (events, result) =
        ats::run_and_collect(dispatcher_for(&server), &simple_request(), &test_model()).await;

    assert_eq!(
        result.content,
        vec![
            ContentBlock::tool_use("toolu_1", "read_file", json!({ "path": "x" })),
            ContentBlock::tool_use("toolu_2", "list_directory", json!({})),
        ]
    );
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    // input_json deltas never fire the callback.
    assert!(events.is_empty());
}

// ------------------------------------------------------------------
// LM Studio quirk: servers that omit block indexes
// ------------------------------------------------------------------

#[tokio::test]
async fn omitted_block_indexes_do_not_collapse() {
    let server = MockServer::start().await;
    serve_sse(
        &server,
        sse_body(&[
            message_start(json!({ "input_tokens": 5 })),
            block_start(Value::Null, json!({ "type": "text", "text": "" })),
            block_delta(
                Value::Null,
                json!({ "type": "text_delta", "text": "Let me check." }),
            ),
            block_stop(Value::Null),
            block_start(
                Value::Null,
                json!({ "type": "tool_use", "id": "t1", "name": "list_directory" }),
            ),
            block_delta(
                Value::Null,
                json!({ "type": "input_json_delta", "partial_json": "{\"path\": \".\"}" }),
            ),
            block_stop(Value::Null),
            message_delta("tool_use", json!({ "output_tokens": 9 })),
            message_stop(),
        ]),
    )
    .await;

    let result = dispatcher_for(&server)
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    assert_eq!(
        result.content,
        vec![
            ContentBlock::text("Let me check."),
            ContentBlock::tool_use("t1", "list_directory", json!({ "path": "." })),
        ]
    );
}

#[tokio::test]
async fn malformed_tool_input_is_marked_not_silently_emptied() {
    let server = MockServer::start().await;
    let malformed = "{\"path\": tru";
    serve_sse(
        &server,
        sse_body(&[
            message_start(json!({})),
            block_start(
                json!(0),
                json!({ "type": "tool_use", "id": "t1", "name": "list_directory" }),
            ),
            block_delta(
                json!(0),
                json!({ "type": "input_json_delta", "partial_json": malformed }),
            ),
            block_stop(json!(0)),
            message_delta("tool_use", json!({})),
            message_stop(),
        ]),
    )
    .await;

    let result = dispatcher_for(&server)
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    assert_eq!(
        result.content,
        vec![ContentBlock::tool_use(
            "t1",
            "list_directory",
            malformed_input_marker(malformed)
        )]
    );
}

#[tokio::test]
async fn maps_max_tokens_stop_reason() {
    let server = MockServer::start().await;
    serve_sse(
        &server,
        sse_body(&[
            message_start(json!({ "input_tokens": 3, "output_tokens": 1 })),
            block_start(json!(0), json!({ "type": "text", "text": "" })),
            block_delta(json!(0), json!({ "type": "text_delta", "text": "truncat" })),
            block_stop(json!(0)),
            message_delta("max_tokens", json!({ "output_tokens": 16_000 })),
            message_stop(),
        ]),
    )
    .await;

    let result = dispatcher_for(&server)
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    assert_eq!(result.stop_reason, StopReason::MaxTokens);
    assert_eq!(result.content, vec![ContentBlock::text("truncat")]);
}

// ------------------------------------------------------------------
// Error algebra: complete MUST NOT fail
// ------------------------------------------------------------------

#[tokio::test]
async fn http_500_becomes_error_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "type": "error",
            "error": { "type": "api_error", "message": "server exploded" }
        })))
        .mount(&server)
        .await;

    let result = dispatcher_for(&server)
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    ats::assert_complete_is_error(result);
}

#[tokio::test]
async fn connection_refused_becomes_error_response() {
    // A port nothing listens on.
    let refused = Provider {
        id: "local".into(),
        base_url: "http://localhost:1/v1".into(),
        token: "t".into(),
        api: Api::AnthropicMessages,
        context_window: Some(64_000),
        custom: true,
    };
    let result = Dispatcher::new(vec![refused])
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    ats::assert_complete_error_response_empty(result);
}

#[tokio::test]
async fn error_sse_event_yields_error_with_partial_content_kept() {
    let server = MockServer::start().await;
    serve_sse(
            &server,
            sse_body(&[
                message_start(json!({ "input_tokens": 3, "output_tokens": 1 })),
                block_start(json!(0), json!({ "type": "text", "text": "" })),
                block_delta(json!(0), json!({ "type": "text_delta", "text": "partial thou" })),
                frame("error", json!({ "type": "error", "error": { "type": "overloaded_error", "message": "busy" } })),
            ]),
        )
        .await;

    let result = dispatcher_for(&server)
        .complete(&simple_request(), &test_model(), &mut no_op())
        .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    let err = result.error.clone().unwrap();
    assert!(err.contains("api_stream_error"), "error was: {err}");
    assert!(err.contains("overloaded_error"), "error was: {err}");
    // The open block was finalized as-is: partial text survives.
    assert_eq!(result.content, vec![ContentBlock::text("partial thou")]);
}

// ------------------------------------------------------------------
// Request-side assertion: outgoing JSON body and headers
// ------------------------------------------------------------------

#[tokio::test]
async fn sends_system_max_tokens_stream_tools_and_wire_format_messages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(&[
                    message_start(json!({ "input_tokens": 1, "output_tokens": 1 })),
                    block_start(json!(0), json!({ "type": "text", "text": "" })),
                    block_delta(json!(0), json!({ "type": "text_delta", "text": "ok" })),
                    block_stop(json!(0)),
                    message_delta("end_turn", json!({ "output_tokens": 2 })),
                    message_stop(),
                ])),
        )
        .mount(&server)
        .await;

    let tool_spec = crate::content::ToolSpec {
        name: "read_file".into(),
        description: "Reads the contents of a file.".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    };

    let messages = vec![
        Message::user(vec![ContentBlock::text("read mix.exs")]),
        Message::assistant(vec![
            ContentBlock::text("Reading it."),
            ContentBlock::tool_use("toolu_1", "read_file", json!({ "path": "mix.exs" })),
        ]),
        Message::user(vec![ContentBlock::tool_result(
            "toolu_1",
            "defmodule ...",
            false,
        )]),
    ];

    let result = dispatcher_for(&server)
        .complete(
            &LlmRequest::new("You are Baud.", messages, vec![tool_spec]),
            &test_model(),
            &mut no_op(),
        )
        .await;
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // Inspect the recorded request: the adapter's headers and body.
    let received = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        received.headers.get("x-api-key").unwrap().to_str().unwrap(),
        "test-token"
    );
    assert_eq!(
        received
            .headers
            .get("anthropic-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "2023-06-01"
    );
    let body: Value = received.body_json().unwrap();

    assert_eq!(body["model"], json!("test-model"));
    assert_eq!(body["system"], json!("You are Baud."));
    assert_eq!(body["max_tokens"], json!(16_000));
    assert_eq!(body["stream"], json!(true));
    assert!(body.as_object().unwrap().get("temperature").is_none());

    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], json!("read_file"));
    assert_eq!(
        tools[0]["description"],
        json!("Reads the contents of a file.")
    );
    assert_eq!(tools[0]["input_schema"]["type"], json!("object"));
    assert_eq!(tools[0]["input_schema"]["required"], json!(["path"]));
    assert_eq!(
        tools[0]["input_schema"]["properties"]["path"]["type"],
        json!("string")
    );

    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs[0],
        json!({ "role": "user", "content": [{ "type": "text", "text": "read mix.exs" }] })
    );
    assert_eq!(msgs[1]["role"], json!("assistant"));
    let m2 = msgs[1]["content"].as_array().unwrap();
    assert_eq!(m2[0], json!({ "type": "text", "text": "Reading it." }));
    assert_eq!(
        m2[1],
        json!({ "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "mix.exs" } })
    );
    assert_eq!(msgs[2]["role"], json!("user"));
    let m3 = msgs[2]["content"].as_array().unwrap();
    assert_eq!(
        m3[0],
        json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "is_error": false,
            "content": [{ "type": "text", "text": "defmodule ..." }]
        })
    );
}

#[tokio::test]
async fn sends_the_requests_temperature_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(&[
                    message_start(json!({ "input_tokens": 1, "output_tokens": 1 })),
                    message_delta("end_turn", json!({ "output_tokens": 2 })),
                    message_stop(),
                ])),
        )
        .mount(&server)
        .await;

    let req = simple_request().with_temperature(Some(0.7));
    let body = ats::body_for(dispatcher_for(&server), req, &test_model(), &server).await;
    assert_eq!(body["temperature"], json!(0.7));
}

// ------------------------------------------------------------------
// No-think rescue: the break-glass field rides one request
// ------------------------------------------------------------------

async fn capture_body_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(&[
                    message_start(json!({ "input_tokens": 1, "output_tokens": 1 })),
                    message_delta("end_turn", json!({ "output_tokens": 2 })),
                    message_stop(),
                ])),
        )
        .mount(&server)
        .await;
    server
}

// no_think:true sends chat_template_kwargs; no_think:false and no flag both
// omit it entirely, byte-identical to a plain request.
#[tokio::test]
async fn no_think_flag_controls_chat_template_kwargs() {
    let server = capture_body_server().await;
    let body = ats::body_for(
        dispatcher_for(&server),
        simple_request().with_no_think(true),
        &test_model(),
        &server,
    )
    .await;
    assert_eq!(
        body["chat_template_kwargs"],
        json!({ "enable_thinking": false })
    );

    let server2 = capture_body_server().await;
    let body2 = ats::body_for(
        dispatcher_for(&server2),
        simple_request().with_no_think(false),
        &test_model(),
        &server2,
    )
    .await;
    assert!(
        body2
            .as_object()
            .unwrap()
            .get("chat_template_kwargs")
            .is_none()
    );

    let server3 = capture_body_server().await;
    let body3 = ats::body_for(
        dispatcher_for(&server3),
        simple_request(),
        &test_model(),
        &server3,
    )
    .await;
    assert!(
        body3
            .as_object()
            .unwrap()
            .get("chat_template_kwargs")
            .is_none()
    );
}

// ------------------------------------------------------------------
// list_models - the read-only models-list endpoint (ADR-0002 amendment)
// ------------------------------------------------------------------

async fn serve_models(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// A windowless [`DiscoveredModel`] - the shape a host reports when it omits
/// `meta.n_ctx` (the common case for these adapter tests).
fn disc(id: &str) -> DiscoveredModel {
    DiscoveredModel {
        id: id.to_string(),
        context_window: None,
    }
}

#[tokio::test]
async fn list_models_returns_ids_in_order() {
    let server = MockServer::start().await;
    serve_models(
        &server,
        200,
        json!({ "data": [{ "id": "a" }, { "id": "b" }] }),
    )
    .await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert_eq!(result, Ok(vec![disc("a"), disc("b")]));
}

#[tokio::test]
async fn list_models_reads_the_server_window_from_meta_n_ctx() {
    let server = MockServer::start().await;
    serve_models(
        &server,
        200,
        json!({ "data": [{ "id": "a", "meta": { "n_ctx": 145664 } }, { "id": "b" }] }),
    )
    .await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert_eq!(
        result,
        Ok(vec![
            DiscoveredModel {
                id: "a".to_string(),
                context_window: Some(145_664),
            },
            disc("b"),
        ])
    );
}

#[tokio::test]
async fn list_models_empty_data_is_ok_empty() {
    let server = MockServer::start().await;
    serve_models(&server, 200, json!({ "data": [] })).await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert_eq!(result, Ok(vec![]));
}

#[tokio::test]
async fn list_models_missing_data_is_ok_empty() {
    let server = MockServer::start().await;
    serve_models(&server, 200, json!({})).await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert_eq!(result, Ok(vec![]));
}

#[tokio::test]
async fn list_models_skips_entries_without_a_string_id() {
    let server = MockServer::start().await;
    serve_models(
        &server,
        200,
        json!({ "data": [{ "id": "a" }, { "id": 7 }, { "name": "no-id" }, { "id": "b" }] }),
    )
    .await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert_eq!(result, Ok(vec![disc("a"), disc("b")]));
}

#[tokio::test]
async fn list_models_http_404_is_error() {
    let server = MockServer::start().await;
    serve_models(&server, 404, json!({ "type": "error" })).await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert!(result.is_err(), "expected Err, got {result:?}");
    assert!(result.unwrap_err().contains("404"));
}

#[tokio::test]
async fn list_models_connection_refused_is_error() {
    // A port nothing listens on.
    let refused = Provider {
        id: "local".into(),
        base_url: "http://localhost:1/v1".into(),
        token: "t".into(),
        api: Api::AnthropicMessages,
        context_window: Some(64_000),
        custom: true,
    };
    let result = Dispatcher::new(vec![refused.clone()])
        .list_models(&refused)
        .await;
    ats::assert_list_models_request_failed(result);
}

#[tokio::test]
async fn list_models_non_json_body_is_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let result = dispatcher_for(&server)
        .list_models(&provider_for(&server))
        .await;
    assert!(result.is_err());
}
