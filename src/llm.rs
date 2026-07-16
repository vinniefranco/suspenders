//! The LLM client boundary. Speaks the Anthropic Messages API (streaming SSE).
//!
//! This is the only module allowed to touch the HTTP/SSE transport (ADR-0002).
//! Everything else speaks the plain typed shapes from DESIGN.md.
//!
//! Connection settings (endpoint, token, model, max_tokens, temperature)
//! arrive as a [`Connection`] on every call — this module reads no config. The
//! connection argument is the test seam: transport tests point one at a mock
//! server; logic tests inject a [`test_support::FakeLlm`] behind the [`Llm`]
//! trait (ADR-0020).
//!
//! ## Internal architecture
//!
//! - Request building (wire-format conversion) lives in [`request`] — pure, no
//!   transport reference; it produces the complete payload this module sends.
//! - SSE event decoding (the streaming state machine) lives in [`stream`] — a
//!   pure fold, testable with canned event lists.
//! - Emit pacing (the ~30fps UI accommodation) lives in [`throttle`] — a pure
//!   decision over caller-supplied clock ticks.
//!
//! [`AnthropicLlm`] wires the three together: reqwest for HTTP,
//! `eventsource-stream` for SSE framing, [`stream::fold_sse`] for decoding, and
//! [`throttle`] to pace the `on_event` callback.
//!
//! ## Error algebra (ADR-0002)
//!
//! [`Llm::complete`] NEVER returns `Err` and NEVER panics. Connection refused,
//! a non-2xx status, an SSE parse failure, and mid-stream death ALL yield a
//! [`Response`] with `stop_reason: Error`, `error` set, and whatever partial
//! content had streamed. Failure is data the Turn loop reads.

pub mod request;
pub mod response;
pub mod stream;
pub mod throttle;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;

use crate::session::connection::Connection;
use response::Response;
use stream::{Delta, SseEvent, StreamEvent, StreamState};
use throttle::{Decision, Throttle};

/// Minimum ms between streaming updates. At ~30fps the UI stays responsive to
/// keyboard input; text rendering above this rate is imperceptible and only
/// floods the channel.
const STREAM_INTERVAL_MS: i64 = 33;

/// The per-delta streaming callback. A named `for<'e>`-quantified trait object
/// so the `&StreamEvent` borrow stays independent of the async future's
/// lifetime (async-trait would otherwise unify an elided lifetime with the
/// future, breaking short-lived local borrows inside `complete`).
pub type OnEvent<'cb> = dyn FnMut(&StreamEvent) + Send + 'cb;

/// The LLM boundary seam. Object-safe so `Arc<dyn Llm>` works (ADR-0020).
///
/// `request` is the complete wire payload (build it with
/// [`request::build_request`]). `on_event` gets one [`StreamEvent`] per
/// text/thinking delta in arrival order — the delta plus the snapshot of
/// blocks accumulated so far (open block included; consumers re-render
/// statelessly). Snapshots carry the accumulated thinking block so the UI can
/// render Thinking without bookkeeping; it is dropped from the returned
/// content and never enters the Conversation.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(
        &self,
        request: Value,
        connection: &Connection,
        on_event: &mut OnEvent<'_>,
    ) -> Response;

    /// Lists the model identifiers the server offers (`GET {base_url}/models`).
    ///
    /// Unlike [`complete`], this RETURNS a `Result` rather than folding failure
    /// into a Response (ADR-0002 amendment, ADR-0033): it is a discrete,
    /// user-triggered query, not the streaming Turn loop, so a plain `Err` the
    /// caller surfaces as an info line is simpler than the never-Err error
    /// algebra. Connection refused, a non-2xx status, and an unparseable body
    /// are all `Err`; a well-formed response with no `data` is `Ok(vec![])`.
    ///
    /// [`complete`]: Llm::complete
    async fn list_models(&self, connection: &Connection) -> Result<Vec<String>, String>;
}

/// The real boundary. Holds nothing config-ish — reads only the `connection`
/// argument on each call (ADR-0002, ADR-0020).
#[derive(Debug, Clone, Default)]
pub struct AnthropicLlm;

impl AnthropicLlm {
    pub fn new() -> Self {
        AnthropicLlm
    }
}

#[async_trait]
impl Llm for AnthropicLlm {
    async fn complete(
        &self,
        request: Value,
        connection: &Connection,
        on_event: &mut OnEvent<'_>,
    ) -> Response {
        let url = format!("{}/messages", connection.base_url.trim_end_matches('/'));

        let client = reqwest::Client::new();
        let sent = client
            .post(&url)
            .header("x-api-key", &connection.token)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await;

        let resp = match sent {
            Ok(resp) => resp,
            // Connection refused, DNS failure, etc. — no content streamed.
            Err(e) => return Response::error(format!("request_failed: {e}")),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Response::error(format!("request_failed: HTTP {status}: {body}"));
        }

        // Fold the SSE frames into the pure state machine, pacing `on_event`.
        let mut state = StreamState::new();
        let mut throttle = Throttle::new(STREAM_INTERVAL_MS);
        let mut sse = resp.bytes_stream().eventsource();

        while let Some(item) = sse.next().await {
            match item {
                Ok(event) => {
                    let sse_event = parse_frame(&event.event, &event.data);
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

    async fn list_models(&self, connection: &Connection) -> Result<Vec<String>, String> {
        let url = format!("{}/models", connection.base_url.trim_end_matches('/'));

        let client = reqwest::Client::new();
        let sent = client
            .get(&url)
            .header("x-api-key", &connection.token)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await;

        let resp = match sent {
            Ok(resp) => resp,
            // Connection refused, DNS failure, etc.
            Err(e) => return Err(format!("request_failed: {e}")),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("request_failed: HTTP {status}: {body}"));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| format!("request_failed: {e}"))?;
        let value: Value =
            serde_json::from_str(&body).map_err(|e| format!("request_failed: {e}"))?;

        // The models-list shape is common to the Anthropic and OpenAI REST
        // APIs: `{"data": [{"id": …}]}`. Parse `data[].id`, leniently skipping
        // any entry that lacks a string `id`. Missing/empty `data` → empty vec.
        let ids = value
            .get("data")
            .and_then(|d| d.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.get("id").and_then(|id| id.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }
}

/// Emits one event through the callback. A free function so the `&StreamEvent`
/// borrow is a fresh, function-local lifetime rather than being unified with
/// the async-trait future's lifetime (which triggers a spurious borrow error).
fn emit(on_event: &mut OnEvent<'_>, ev: StreamEvent) {
    on_event(&ev);
}

/// A process-monotonic millisecond clock for throttle pacing.
fn monotonic_ms() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as i64
}

/// Turns a raw `event:`/`data:` frame into a parsed [`SseEvent`]. A data body
/// that isn't valid JSON becomes a [`SseEvent::ParseError`] (the error
/// algebra: an SSE parse failure is data, not an exception).
fn parse_frame(name: &str, data: &str) -> SseEvent {
    match serde_json::from_str::<Value>(data) {
        Ok(value) => SseEvent::event(name, value),
        Err(e) => SseEvent::ParseError(format!("sse_parse_failed: {e}")),
    }
}

/// The renderable delta carried by a `content_block_delta` frame, if any.
/// Mirrors baud's `extract_delta`: only text and thinking deltas fire the
/// callback; input_json and everything else stay quiet.
fn delta_of(event: &SseEvent) -> Option<Delta> {
    let SseEvent::Event { name, data } = event else {
        return None;
    };
    if name != "content_block_delta" {
        return None;
    }
    let delta = data.get("delta")?;
    match delta.get("type").and_then(|v| v.as_str()) {
        Some("text_delta") => delta
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| Delta::Text(s.to_string())),
        Some("thinking_delta") => delta
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(|s| Delta::Thinking(s.to_string())),
        _ => None,
    }
}

// `eventsource-stream`'s `Eventsource` trait extension on byte streams.
use eventsource_stream::Eventsource;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::content::Message;
    use crate::llm::request::{self, LlmRequest};
    use crate::llm::response::StopReason;
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

    fn simple_request() -> Value {
        let req = LlmRequest::new(
            "You are Baud.",
            vec![Message::user(vec![ContentBlock::text("hi")])],
            vec![],
        );
        // A placeholder connection just to render; the mock's connection is
        // supplied separately in each test.
        req_to_value(req)
    }

    fn req_to_value(req: LlmRequest) -> Value {
        let conn = Connection::new("http://placeholder/v1", "test-token", "test-model", 16_000);
        request::build_request(&req, &conn)
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

    fn connection_for(server: &MockServer) -> Connection {
        Connection::new(
            format!("{}/v1", server.uri()),
            "test-token",
            "test-model",
            16_000,
        )
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

        let conn = connection_for(&server);
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut on_event = |ev: &StreamEvent| events.push(ev.clone());

        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut on_event)
            .await;

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
                block_start(json!(1), json!({ "type": "tool_use", "id": "toolu_2", "name": "list_files", "input": {} })),
                block_stop(json!(1)),
                message_delta("tool_use", json!({ "output_tokens": 9 })),
                message_stop(),
            ]),
        )
        .await;

        let conn = connection_for(&server);
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut on_event = |ev: &StreamEvent| events.push(ev.clone());

        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut on_event)
            .await;

        assert_eq!(
            result.content,
            vec![
                ContentBlock::tool_use("toolu_1", "read_file", json!({ "path": "x" })),
                ContentBlock::tool_use("toolu_2", "list_files", json!({})),
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
                    json!({ "type": "tool_use", "id": "t1", "name": "list_files" }),
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut no_op())
            .await;

        assert_eq!(
            result.content,
            vec![
                ContentBlock::text("Let me check."),
                ContentBlock::tool_use("t1", "list_files", json!({ "path": "." })),
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
                    json!({ "type": "tool_use", "id": "t1", "name": "list_files" }),
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut no_op())
            .await;

        assert_eq!(
            result.content,
            vec![ContentBlock::tool_use(
                "t1",
                "list_files",
                stream::malformed_input_marker(malformed)
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut no_op())
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut no_op())
            .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn connection_refused_becomes_error_response() {
        // A port nothing listens on.
        let refused = Connection::new("http://localhost:1/v1", "t", "m", 100);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &refused, &mut no_op())
            .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.content, Vec::<ContentBlock>::new());
        assert!(result.error.is_some());
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new()
            .complete(simple_request(), &conn, &mut no_op())
            .await;

        assert_eq!(result.stop_reason, StopReason::Error);
        let err = result.error.clone().unwrap();
        assert!(err.contains("api_stream_error"), "error was: {err}");
        assert!(err.contains("overloaded_error"), "error was: {err}");
        // The open block was finalized as-is: partial text survives.
        assert_eq!(result.content, vec![ContentBlock::text("partial thou")]);
    }

    // ------------------------------------------------------------------
    // Request-side assertion: outgoing JSON body
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

        let tool_spec = crate::tool::ToolSpec {
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

        let conn = connection_for(&server);
        let payload = req_to_value_for(
            LlmRequest::new("You are Baud.", messages, vec![tool_spec]),
            &conn,
        );

        let result = AnthropicLlm::new()
            .complete(payload, &conn, &mut no_op())
            .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        // Inspect the recorded request body.
        let received = &server.received_requests().await.unwrap()[0];
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
            json!({ "type": "tool_result", "tool_use_id": "toolu_1", "content": "defmodule ...", "is_error": false })
        );
    }

    #[tokio::test]
    async fn sends_the_connections_temperature_when_configured() {
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

        let conn = connection_for(&server).with_temperature(Some(0.7));
        let payload = req_to_value_for(
            LlmRequest::new(
                "You are Baud.",
                vec![Message::user(vec![ContentBlock::text("hi")])],
                vec![],
            ),
            &conn,
        );
        AnthropicLlm::new()
            .complete(payload, &conn, &mut no_op())
            .await;

        let received = &server.received_requests().await.unwrap()[0];
        let body: Value = received.body_json().unwrap();
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

    #[tokio::test]
    async fn no_think_true_carries_chat_template_kwargs_false() {
        let server = capture_body_server().await;
        let conn = connection_for(&server);
        let payload = req_to_value_for(
            LlmRequest::new(
                "You are Baud.",
                vec![Message::user(vec![ContentBlock::text("hi")])],
                vec![],
            )
            .with_no_think(true),
            &conn,
        );
        AnthropicLlm::new()
            .complete(payload, &conn, &mut no_op())
            .await;

        let received = &server.received_requests().await.unwrap()[0];
        let body: Value = received.body_json().unwrap();
        assert_eq!(
            body["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
    }

    #[tokio::test]
    async fn request_without_flag_carries_no_chat_template_kwargs() {
        let server = capture_body_server().await;
        let conn = connection_for(&server);
        let payload = req_to_value_for(
            LlmRequest::new(
                "You are Baud.",
                vec![Message::user(vec![ContentBlock::text("hi")])],
                vec![],
            ),
            &conn,
        );
        AnthropicLlm::new()
            .complete(payload, &conn, &mut no_op())
            .await;

        let received = &server.received_requests().await.unwrap()[0];
        let body: Value = received.body_json().unwrap();
        assert!(
            body.as_object()
                .unwrap()
                .get("chat_template_kwargs")
                .is_none()
        );
    }

    #[tokio::test]
    async fn no_think_false_byte_identical_to_no_flag() {
        let server = capture_body_server().await;
        let conn = connection_for(&server);
        let payload = req_to_value_for(
            LlmRequest::new(
                "You are Baud.",
                vec![Message::user(vec![ContentBlock::text("hi")])],
                vec![],
            )
            .with_no_think(false),
            &conn,
        );
        AnthropicLlm::new()
            .complete(payload, &conn, &mut no_op())
            .await;

        let received = &server.received_requests().await.unwrap()[0];
        let body: Value = received.body_json().unwrap();
        assert!(
            body.as_object()
                .unwrap()
                .get("chat_template_kwargs")
                .is_none()
        );
    }

    // Renders a request against a specific connection (so model/temperature/
    // no_think reach the wire correctly).
    fn req_to_value_for(req: LlmRequest, conn: &Connection) -> Value {
        request::build_request(&req, conn)
    }

    // ------------------------------------------------------------------
    // FakeLlm (ADR-0020) — exercised here so the seam is covered.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn fake_llm_fires_deltas_and_returns_scripted_response() {
        use crate::test_support::{Entry, FakeLlm};

        let fake = FakeLlm::script(vec![Entry::response(
            vec![Delta::Text("Hel".into()), Delta::Text("lo".into())],
            Response {
                content: vec![ContentBlock::text("Hello")],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                error: None,
            },
        )]);

        let conn = Connection::new("http://x/v1", "t", "m", 100);
        let mut seen: Vec<Delta> = Vec::new();
        let mut on_event = |ev: &StreamEvent| seen.push(ev.delta.clone());

        let result = fake.complete(json!({}), &conn, &mut on_event).await;

        assert_eq!(
            seen,
            vec![Delta::Text("Hel".into()), Delta::Text("lo".into())]
        );
        assert_eq!(result.content, vec![ContentBlock::text("Hello")]);
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn fake_llm_error_entry_normalizes_to_error_response() {
        use crate::test_support::{Entry, FakeLlm};

        let fake = FakeLlm::script(vec![Entry::error("boom")]);
        let conn = Connection::new("http://x/v1", "t", "m", 100);

        let result = fake.complete(json!({}), &conn, &mut no_op()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error.as_deref(), Some("boom"));
    }

    // ------------------------------------------------------------------
    // list_models — the read-only models-list endpoint (ADR-0002 amendment)
    // ------------------------------------------------------------------

    async fn serve_models(server: &MockServer, status: u16, body: Value) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new().list_models(&conn).await;
        assert_eq!(result, Ok(vec!["a".to_string(), "b".to_string()]));
    }

    #[tokio::test]
    async fn list_models_empty_data_is_ok_empty() {
        let server = MockServer::start().await;
        serve_models(&server, 200, json!({ "data": [] })).await;

        let conn = connection_for(&server);
        assert_eq!(AnthropicLlm::new().list_models(&conn).await, Ok(vec![]));
    }

    #[tokio::test]
    async fn list_models_missing_data_is_ok_empty() {
        let server = MockServer::start().await;
        serve_models(&server, 200, json!({})).await;

        let conn = connection_for(&server);
        assert_eq!(AnthropicLlm::new().list_models(&conn).await, Ok(vec![]));
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

        let conn = connection_for(&server);
        let result = AnthropicLlm::new().list_models(&conn).await;
        assert_eq!(result, Ok(vec!["a".to_string(), "b".to_string()]));
    }

    #[tokio::test]
    async fn list_models_http_404_is_error() {
        let server = MockServer::start().await;
        serve_models(&server, 404, json!({ "type": "error" })).await;

        let conn = connection_for(&server);
        let result = AnthropicLlm::new().list_models(&conn).await;
        assert!(result.is_err(), "expected Err, got {result:?}");
        assert!(result.unwrap_err().contains("404"));
    }

    #[tokio::test]
    async fn list_models_connection_refused_is_error() {
        // A port nothing listens on.
        let refused = Connection::new("http://localhost:1/v1", "t", "m", 100);
        let result = AnthropicLlm::new().list_models(&refused).await;
        assert!(result.is_err(), "expected Err, got {result:?}");
        assert!(result.unwrap_err().contains("request_failed"));
    }

    #[tokio::test]
    async fn list_models_non_json_body_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let conn = connection_for(&server);
        assert!(AnthropicLlm::new().list_models(&conn).await.is_err());
    }

    #[tokio::test]
    async fn fake_llm_list_models_scripts_ok_then_err() {
        use crate::test_support::FakeLlm;

        let fake = FakeLlm::script(std::iter::empty()).with_models(vec![
            Ok(vec!["m1".to_string(), "m2".to_string()]),
            Err("boom".to_string()),
        ]);
        let conn = Connection::new("http://x/v1", "t", "m", 100);

        assert_eq!(
            fake.list_models(&conn).await,
            Ok(vec!["m1".to_string(), "m2".to_string()])
        );
        assert_eq!(fake.list_models(&conn).await, Err("boom".to_string()));
        // Exhausted queue falls back to the benign empty list.
        assert_eq!(fake.list_models(&conn).await, Ok(vec![]));
    }
}
