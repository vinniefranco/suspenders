//! The OpenAI Chat Completions API adapter (ADR-0037): everything this Api's
//! wire format needs, end to end - request building, transport, the Bearer
//! authorization header, the SSE fold, stop-reason mapping, and usage
//! extraction. Covers LM Studio, llama.cpp, DeepSeek, Groq, OpenRouter, and
//! most OpenAI-compatible hosts; per-Model compat quirks within the dialect
//! land in Stage C.
//!
//! ## Internal architecture
//!
//! - Request building (wire-format conversion) lives in [`request`] - pure, no
//!   transport reference; it produces the complete payload this module sends.
//! - SSE chunk decoding (the streaming state machine) lives in [`stream`] - a
//!   pure fold, testable with canned event lists.
//!
//! [`complete`] wires them together: reqwest for HTTP, `eventsource-stream`
//! for SSE framing, [`stream::StreamState`] for decoding, and
//! [`crate::llm::throttle`] to pace the `on_event` callback.

pub mod request;
pub mod stream;
pub mod text_tool_call;

use futures_util::StreamExt;
use serde_json::Value;

use crate::llm::model::Model;
use crate::llm::provider::Provider;
use crate::llm::response::Response;
use crate::llm::throttle::{Decision, Throttle, monotonic_ms};
use crate::llm::{Delta, LlmRequest, OnEvent, StreamEvent, emit, models_from_body};
use stream::{SseEvent, StreamState};

/// Minimum ms between streaming updates. At ~30fps the UI stays responsive to
/// keyboard input; text rendering above this rate is imperceptible and only
/// floods the channel.
const STREAM_INTERVAL_MS: i64 = 33;

/// One streaming completion over `provider`'s endpoint
/// (`POST {base_url}/chat/completions`). Honors the error algebra (ADR-0002):
/// never `Err`, never panic - every failure is a Response with an `Error`
/// stop reason and whatever partial content had streamed.
pub(super) async fn complete(
    req: &LlmRequest,
    model: &Model,
    provider: &Provider,
    on_event: &mut OnEvent<'_>,
) -> Response {
    let payload = request::build_request(req, model);
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    let client = reqwest::Client::new();
    let sent = authorized(client.post(&url), provider)
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .await;

    let resp = match sent {
        Ok(resp) => resp,
        // Connection refused, DNS failure, etc. - no content streamed.
        Err(e) => return Response::error(request_err(e)),
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Response::error(format!("request_failed: HTTP {status}: {body}"));
    }

    // Fold the SSE frames into the pure state machine, pacing `on_event`. The
    // request's Tool Call style rides into the fold so finalization knows
    // whether to recover a text-emitted call (qwen parity).
    let mut state = StreamState::with_style(req.tool_call_style);
    let mut throttle = Throttle::new(STREAM_INTERVAL_MS);
    let mut sse = resp.bytes_stream().eventsource();

    while let Some(item) = sse.next().await {
        match item {
            Ok(event) => {
                let sse_event = parse_frame(&event.data);
                // Extract any renderable delta BEFORE folding so we can
                // snapshot the state AFTER folding this delta in.
                let delta = delta_of(&sse_event);
                state.handle_event(&sse_event);

                if let Some(delta) = delta
                    && throttle.tick(monotonic_ms()) == Decision::Emit
                {
                    emit(
                        on_event,
                        StreamEvent {
                            delta,
                            content: state.snapshot(),
                        },
                    );
                }

                // `data: [DONE]` is the dialect's terminator; hosts may hold
                // the connection open past it.
                if sse_event == SseEvent::Done {
                    break;
                }
            }
            // Mid-stream death (dropped connection, framing error): fold an
            // error so partial content survives (the error algebra).
            Err(e) => {
                state.handle_event(&SseEvent::ParseError(format!("stream_error: {e}")));
                break;
            }
        }
    }

    state.finalize()
}

/// The read-only models listing (`GET {base_url}/models`, ADR-0002 amendment)
/// with Bearer auth. The response shape is the one both REST APIs share;
/// [`models_from_body`] owns the parse.
pub(super) async fn list_models(provider: &Provider) -> Result<Vec<String>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));

    // The discovery cap (see [`super::DISCOVERY_TIMEOUT`]): a blackholed
    // host times out into the same request_failed Err as any other failure.
    let client = reqwest::Client::builder()
        .timeout(super::DISCOVERY_TIMEOUT)
        .build()
        .map_err(request_err)?;
    let sent = authorized(client.get(&url), provider).send().await;

    let resp = match sent {
        Ok(resp) => resp,
        // Connection refused, DNS failure, etc.
        Err(e) => return Err(request_err(e)),
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("request_failed: HTTP {status}: {body}"));
    }

    let body = resp.text().await.map_err(request_err)?;
    models_from_body(&body)
}

/// Attaches `Authorization: Bearer {token}` - omitted entirely when the token
/// is empty, so a local server (LM Studio, llama.cpp) sees no auth header at
/// all.
fn authorized(builder: reqwest::RequestBuilder, provider: &Provider) -> reqwest::RequestBuilder {
    if provider.token.is_empty() {
        builder
    } else {
        builder.header("authorization", format!("Bearer {}", provider.token))
    }
}

/// Runs one `data:` frame body into a parsed [`SseEvent`]. `[DONE]` is the
/// dialect's terminator; any other body must be a JSON chunk, and one that
/// isn't valid JSON becomes a [`SseEvent::ParseError`] (the error algebra: an
/// SSE parse failure is data, not an exception).
fn parse_frame(data: &str) -> SseEvent {
    if data.trim() == "[DONE]" {
        return SseEvent::Done;
    }
    match serde_json::from_str::<Value>(data) {
        Ok(value) => SseEvent::Chunk(value),
        Err(e) => SseEvent::ParseError(format!("sse_parse_failed: {e}")),
    }
}

/// The renderable delta carried by a chunk, if any: `delta.reasoning_content`
/// (the DeepSeek/LM Studio thinking dialect) fires as Thinking,
/// `delta.content` as Text. Tool-call fragments and usage-only chunks stay
/// quiet, and so does the empty content of the role-announcing first chunk.
fn delta_of(event: &SseEvent) -> Option<Delta> {
    let SseEvent::Chunk(chunk) = event else {
        return None;
    };
    let delta = chunk.get("choices")?.get(0)?.get("delta")?;
    if let Some(s) = non_empty_str(delta.get("reasoning_content")) {
        return Some(Delta::Thinking(s.to_string()));
    }
    non_empty_str(delta.get("content")).map(|s| Delta::Text(s.to_string()))
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Formats a `request_failed: {e}` error string. Shared by every failure arm
/// in this adapter so the literal is typed once.
fn request_err(e: impl std::fmt::Display) -> String {
    format!("request_failed: {e}")
}

// `eventsource-stream`'s `Eventsource` trait extension on byte streams.
use eventsource_stream::Eventsource;

#[cfg(test)]
mod tests {
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

    // --- SSE body helpers (canned Chat Completions chunk stream) ---

    fn frame(data: &Value) -> String {
        format!("data: {data}\n\n")
    }

    fn delta_frame(delta: Value) -> String {
        frame(&json!({ "choices": [{ "delta": delta }] }))
    }

    fn finish_frame(reason: &str) -> String {
        frame(&json!({ "choices": [{ "delta": {}, "finish_reason": reason }] }))
    }

    fn usage_frame(usage: Value) -> String {
        frame(&json!({ "choices": [], "usage": usage }))
    }

    fn done() -> String {
        "data: [DONE]\n\n".to_string()
    }

    fn sse_body(frames: &[String]) -> String {
        frames.concat()
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
            .and(path("/v1/chat/completions"))
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
            id: "groq".into(),
            base_url: format!("{}/v1", server.uri()),
            token: "test-token".into(),
            api: Api::OpenaiCompletions,
            context_window: Some(64_000),
            custom: true,
        }
    }

    fn test_model() -> Model {
        Model::new("groq", "test-model", Api::OpenaiCompletions, 64_000, 16_000)
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
    // (a) Happy path: reasoning then content, usage on the final chunk
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn streams_thinking_and_text_excludes_thinking_takes_usage() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "role": "assistant", "content": "" })),
                delta_frame(json!({ "reasoning_content": "pondering" })),
                delta_frame(json!({ "reasoning_content": " deeply" })),
                delta_frame(json!({ "content": "Hello" })),
                delta_frame(json!({ "content": " world" })),
                finish_frame("stop"),
                usage_frame(json!({
                    "prompt_tokens": 11,
                    "completion_tokens": 42,
                    "prompt_tokens_details": { "cached_tokens": 9 }
                })),
                done(),
            ]),
        )
        .await;

        let (events, result) =
            ats::run_and_collect(dispatcher_for(&server), &simple_request(), &test_model()).await;

        // At least the first delta (a thinking one) arrived through the callback.
        assert!(
            matches!(events.first().map(|e| &e.delta), Some(Delta::Thinking(s)) if s == "pondering")
        );

        // Final content correct; thinking excluded; usage taken.
        assert_eq!(result.content, vec![ContentBlock::text("Hello world")]);
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert_eq!(result.error, None);
        assert_eq!(result.usage.input_tokens, Some(11));
        assert_eq!(result.usage.output_tokens, Some(42));
        assert_eq!(result.usage.cache_read_input_tokens, Some(9));
    }

    // ------------------------------------------------------------------
    // (b) tool_calls fragments split mid-JSON-token
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn assembles_tool_calls_from_fragments_split_across_chunks() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": "read_file", "arguments": "" }
                }] })),
                delta_frame(json!({ "tool_calls": [{
                    "index": 0, "function": { "arguments": "{\"pa" }
                }] })),
                delta_frame(json!({ "tool_calls": [{
                    "index": 0, "function": { "arguments": "th\": \"x\"}" }
                }] })),
                delta_frame(json!({ "tool_calls": [{
                    "index": 1, "id": "call_2", "type": "function",
                    "function": { "name": "list_directory", "arguments": "{}" }
                }] })),
                finish_frame("tool_calls"),
                done(),
            ]),
        )
        .await;

        let (events, result) =
            ats::run_and_collect(dispatcher_for(&server), &simple_request(), &test_model()).await;

        assert_eq!(
            result.content,
            vec![
                ContentBlock::tool_use("call_1", "read_file", json!({ "path": "x" })),
                ContentBlock::tool_use("call_2", "list_directory", json!({})),
            ]
        );
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        // tool_calls fragments never fire the callback.
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn malformed_tool_arguments_are_marked_not_silently_emptied() {
        let server = MockServer::start().await;
        let malformed = "{\"path\": tru";
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "tool_calls": [{
                    "index": 0, "id": "c1",
                    "function": { "name": "list_directory", "arguments": malformed }
                }] })),
                finish_frame("tool_calls"),
                done(),
            ]),
        )
        .await;

        let result = dispatcher_for(&server)
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        assert_eq!(
            result.content,
            vec![ContentBlock::tool_use(
                "c1",
                "list_directory",
                malformed_input_marker(malformed)
            )]
        );
    }

    #[tokio::test]
    async fn text_emitted_tool_call_markup_finalizes_as_tool_use() {
        // Qwen3-Coder emits the call as content text, chunked across deltas,
        // with finish_reason "stop". The adapter recovers it end to end.
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "content": "I'll run the tests:\n\n" })),
                delta_frame(json!({ "content": "<tool_call>\n<function=run_shell_command>\n" })),
                delta_frame(json!({ "content": "<parameter=command>\nmix test\n" })),
                delta_frame(json!({ "content": "</parameter>\n</function>\n</tool_call>" })),
                finish_frame("stop"),
                done(),
            ]),
        )
        .await;

        let result = dispatcher_for(&server)
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        assert_eq!(
            result.content,
            vec![
                ContentBlock::text("I'll run the tests:"),
                ContentBlock::tool_use(
                    "text-call-0",
                    "run_shell_command",
                    json!({ "command": "mix test" })
                ),
            ]
        );
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn maps_length_finish_reason_to_max_tokens() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "content": "truncat" })),
                finish_frame("length"),
                done(),
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
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "error": { "message": "server exploded", "type": "server_error" }
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
            id: "groq".into(),
            base_url: "http://localhost:1/v1".into(),
            token: "t".into(),
            api: Api::OpenaiCompletions,
            context_window: Some(64_000),
            custom: true,
        };
        let result = Dispatcher::new(vec![refused])
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        ats::assert_complete_error_response_empty(result);
    }

    #[tokio::test]
    async fn an_error_data_line_yields_error_with_partial_content_kept() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "content": "partial thou" })),
                frame(&json!({ "error": { "message": "overloaded", "code": 503 } })),
            ]),
        )
        .await;

        let result = dispatcher_for(&server)
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        let err = result.error.clone().unwrap();
        assert!(err.contains("api_stream_error"), "error was: {err}");
        assert!(err.contains("overloaded"), "error was: {err}");
        assert_eq!(result.content, vec![ContentBlock::text("partial thou")]);
    }

    #[tokio::test]
    async fn a_non_json_data_line_yields_error_with_partial_content_kept() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "content": "partial" })),
                "data: not json\n\n".to_string(),
                delta_frame(json!({ "content": " ignored" })),
                done(),
            ]),
        )
        .await;

        let result = dispatcher_for(&server)
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error.unwrap().contains("sse_parse_failed"));
        assert_eq!(result.content, vec![ContentBlock::text("partial")]);
    }

    #[tokio::test]
    async fn a_stream_ending_without_done_still_finalizes() {
        // Mid-stream death after valid chunks: the body just ends.
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[delta_frame(json!({ "content": "partial" }))]),
        )
        .await;

        let result = dispatcher_for(&server)
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;

        // No finish_reason arrived: conservative Unknown, partial content kept.
        assert_eq!(result.stop_reason, StopReason::Unknown);
        assert_eq!(result.content, vec![ContentBlock::text("partial")]);
    }

    // ------------------------------------------------------------------
    // Request-side assertion: outgoing JSON body and headers
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn sends_bearer_auth_stream_options_and_wire_format_messages() {
        let server = MockServer::start().await;
        serve_sse(
            &server,
            sse_body(&[
                delta_frame(json!({ "content": "ok" })),
                finish_frame("stop"),
                done(),
            ]),
        )
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
                ContentBlock::tool_use("call_1", "read_file", json!({ "path": "mix.exs" })),
            ]),
            Message::user(vec![ContentBlock::tool_result(
                "call_1",
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
            received
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        let body: Value = received.body_json().unwrap();

        assert_eq!(body["model"], json!("test-model"));
        assert_eq!(body["max_tokens"], json!(16_000));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert!(body.as_object().unwrap().get("temperature").is_none());

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("read_file"));
        assert_eq!(
            tools[0]["function"]["parameters"]["required"],
            json!(["path"])
        );

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[0],
            json!({ "role": "system", "content": "You are Baud." })
        );
        assert_eq!(
            msgs[1],
            json!({ "role": "user", "content": "read mix.exs" })
        );
        assert_eq!(msgs[2]["role"], json!("assistant"));
        assert_eq!(msgs[2]["content"], json!("Reading it."));
        assert_eq!(
            msgs[2]["tool_calls"],
            json!([{
                "id": "call_1",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"mix.exs\"}" }
            }])
        );
        assert_eq!(
            msgs[3],
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "defmodule ..." })
        );
    }

    #[tokio::test]
    async fn an_empty_token_omits_the_authorization_header() {
        let server = MockServer::start().await;
        serve_sse(&server, sse_body(&[finish_frame("stop"), done()])).await;

        let mut provider = provider_for(&server);
        provider.token = "".into();
        let result = Dispatcher::new(vec![provider])
            .complete(&simple_request(), &test_model(), &mut no_op())
            .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        let received = &server.received_requests().await.unwrap()[0];
        assert!(received.headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn no_think_true_carries_chat_template_kwargs_false() {
        let server = MockServer::start().await;
        serve_sse(&server, sse_body(&[finish_frame("stop"), done()])).await;

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
    }

    // ------------------------------------------------------------------
    // list_models - GET {base_url}/models with Bearer auth
    // ------------------------------------------------------------------

    async fn serve_models(server: &MockServer, status: u16, body: Value) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn list_models_returns_ids_and_sends_bearer_auth() {
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
        assert_eq!(result, Ok(vec!["a".to_string(), "b".to_string()]));

        let received = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            received
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
    }

    #[tokio::test]
    async fn list_models_http_404_is_error() {
        let server = MockServer::start().await;
        serve_models(&server, 404, json!({ "error": "not found" })).await;

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
            id: "groq".into(),
            base_url: "http://localhost:1/v1".into(),
            token: "t".into(),
            api: Api::OpenaiCompletions,
            context_window: Some(64_000),
            custom: true,
        };
        let result = Dispatcher::new(vec![refused.clone()])
            .list_models(&refused)
            .await;
        ats::assert_list_models_request_failed(result);
    }
}
