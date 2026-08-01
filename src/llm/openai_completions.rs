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
use crate::llm::{
    Delta, DiscoveredModel, LlmRequest, OnEvent, StreamEvent, emit, models_from_body,
};
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
pub(super) async fn list_models(provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
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
#[path = "../../tests/llm/openai_completions.rs"]
mod tests;
