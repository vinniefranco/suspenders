//! The Anthropic Messages API adapter (ADR-0002, ADR-0037): everything this
//! Api's wire format needs, end to end - request building, transport, headers
//! (`x-api-key`, `anthropic-version`), the SSE fold, stop-reason mapping, and
//! usage extraction. The only module (with its submodules) that touches this
//! protocol's HTTP/SSE shapes.
//!
//! ## Internal architecture
//!
//! - Request building (wire-format conversion) lives in [`request`] - pure, no
//!   transport reference; it produces the complete payload this module sends.
//! - SSE event decoding (the streaming state machine) lives in `stream` - a
//!   pure fold, testable with canned event lists. The module is private to
//!   this adapter, so only its `SseProtocol` strategy (and the fold's
//!   in-module tests) can drive the state: the never-Err error algebra is
//!   enforced by visibility, not convention.
//!
//! `complete` wires them together: reqwest for HTTP, `eventsource-stream`
//! for SSE framing, `stream::StreamState` for decoding, and
//! [`crate::llm::throttle`] to pace the `on_event` callback.

pub mod request;
mod stream;

use serde_json::Value;

use crate::llm::model::Model;
use crate::llm::provider::Provider;
use crate::llm::response::Response;
use crate::llm::transport::{self, SseProtocol, request_err};
use crate::llm::{Delta, DiscoveredModel, LlmRequest, OnEvent};
use stream::{SseEvent, StreamState};

/// This Api's SSE strategy for the shared [`transport`] driver: the frame
/// parsing, delta extraction, and fold-state factory that make the Anthropic
/// dialect concrete. The dialect has no `[DONE]` terminator (`message_stop`
/// ends the stream), so [`is_done`](SseProtocol::is_done) is always `false`.
struct AnthropicProtocol;

impl SseProtocol for AnthropicProtocol {
    type Fold = StreamState;

    fn new_fold(&self) -> StreamState {
        StreamState::new()
    }

    fn parse_frame(&self, name: &str, data: &str) -> SseEvent {
        parse_frame(name, data)
    }

    fn delta_of(&self, event: &SseEvent) -> Option<Delta> {
        delta_of(event)
    }

    fn is_done(&self, _event: &SseEvent) -> bool {
        false
    }
}

/// One streaming completion over `provider`'s endpoint. Honors the error
/// algebra (ADR-0002): never `Err`, never panic - every failure is a Response
/// with an `Error` stop reason and whatever partial content had streamed. The
/// send/status/stream flow is the shared [`transport`] driver; this adapter
/// supplies only the Anthropic request (URL + headers) and its SSE strategy.
pub(super) async fn complete(
    req: &LlmRequest,
    model: &Model,
    provider: &Provider,
    on_event: &mut OnEvent<'_>,
) -> Response {
    let payload = request::build_request(req, model);
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));
    let request = anthropic_headers(reqwest::Client::new().post(&url), provider)
        .header("content-type", "application/json")
        .json(&payload);
    transport::stream_completion(request, &AnthropicProtocol, on_event).await
}

/// The read-only models listing (`GET {base_url}/models`, ADR-0002 amendment).
/// The models-list shape is common to the Anthropic and OpenAI REST APIs
/// (`{"data": [{"id": …}]}`); the Anthropic headers ride because this adapter
/// owns them. The send/status/parse flow is the shared [`transport`] driver.
pub(super) async fn list_models(provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    // The discovery cap (see [`super::DISCOVERY_TIMEOUT`]): a blackholed
    // host times out into the same request_failed Err as any other failure.
    let client = reqwest::Client::builder()
        .timeout(super::DISCOVERY_TIMEOUT)
        .build()
        .map_err(request_err)?;
    let request = anthropic_headers(client.get(&url), provider);
    transport::fetch_models(request).await
}

/// Attaches the Anthropic auth/version headers (`x-api-key`,
/// `anthropic-version`) - the pair every request to this Api carries, typed
/// once so `complete` and `list_models` cannot drift.
fn anthropic_headers(
    builder: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    builder
        .header("x-api-key", &provider.token)
        .header("anthropic-version", "2023-06-01")
}

/// Runs a raw `event:`/`data:` frame into a parsed [`SseEvent`]. A data body
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

#[cfg(test)]
#[path = "../../tests/llm/anthropic_messages.rs"]
mod tests;
