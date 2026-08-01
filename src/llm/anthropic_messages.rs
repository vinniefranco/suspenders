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
//! - SSE event decoding (the streaming state machine) lives in [`stream`] - a
//!   pure fold, testable with canned event lists.
//!
//! [`complete`] wires them together: reqwest for HTTP, `eventsource-stream`
//! for SSE framing, [`stream::StreamState`] for decoding, and
//! [`crate::llm::throttle`] to pace the `on_event` callback.

pub mod request;
pub mod stream;

use futures_util::StreamExt;
use serde_json::Value;

use crate::llm::model::Model;
use crate::llm::provider::Provider;
use crate::llm::response::Response;
use crate::llm::throttle::{Decision, Throttle, monotonic_ms};
use crate::llm::{
    Delta, DiscoveredModel, LlmRequest, OnEvent, StreamEvent, emit, models_from_body,
};
use stream::{SseEvent, StreamState};

/// Minimum ms between streaming updates. At ~30fps the UI stays responsive to
/// keyboard input; text rendering above this rate is imperceptible and only
/// floods the channel.
const STREAM_INTERVAL_MS: i64 = 33;

/// One streaming completion over `provider`'s endpoint. Honors the error
/// algebra (ADR-0002): never `Err`, never panic - every failure is a Response
/// with an `Error` stop reason and whatever partial content had streamed.
pub(super) async fn complete(
    req: &LlmRequest,
    model: &Model,
    provider: &Provider,
    on_event: &mut OnEvent<'_>,
) -> Response {
    let payload = request::build_request(req, model);
    let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let sent = client
        .post(&url)
        .header("x-api-key", &provider.token)
        .header("anthropic-version", "2023-06-01")
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

/// The read-only models listing (`GET {base_url}/models`, ADR-0002 amendment).
/// The models-list shape is common to the Anthropic and OpenAI REST APIs
/// (`{"data": [{"id": …}]}`); the Anthropic headers ride because this adapter
/// owns them.
pub(super) async fn list_models(provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));

    // The discovery cap (see [`super::DISCOVERY_TIMEOUT`]): a blackholed
    // host times out into the same request_failed Err as any other failure.
    let client = reqwest::Client::builder()
        .timeout(super::DISCOVERY_TIMEOUT)
        .build()
        .map_err(request_err)?;
    let sent = client
        .get(&url)
        .header("x-api-key", &provider.token)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await;

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

/// Formats a `request_failed: {e}` error string. Shared by every failure arm
/// in this adapter so the literal is typed once.
fn request_err(e: impl std::fmt::Display) -> String {
    format!("request_failed: {e}")
}

// `eventsource-stream`'s `Eventsource` trait extension on byte streams.
use eventsource_stream::Eventsource;

#[cfg(test)]
#[path = "../../tests/llm/anthropic_messages.rs"]
mod tests;
