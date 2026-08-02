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

use serde_json::Value;

use crate::llm::model::Model;
use crate::llm::provider::Provider;
use crate::llm::response::Response;
use crate::llm::transport::{self, SseProtocol, request_err};
use crate::llm::{Delta, DiscoveredModel, LlmRequest, OnEvent, ToolCallStyle};
use stream::{SseEvent, StreamState};

/// This Api's SSE strategy for the shared [`transport`] driver. Carries the
/// request's Tool Call `style` so [`new_fold`](SseProtocol::new_fold) seeds the
/// state for the text-emitted-call fallback (qwen parity), and reports the
/// dialect's `[DONE]` terminator through [`is_done`](SseProtocol::is_done).
struct OpenaiProtocol {
    style: ToolCallStyle,
}

impl SseProtocol for OpenaiProtocol {
    type Fold = StreamState;

    fn new_fold(&self) -> StreamState {
        StreamState::with_style(self.style)
    }

    fn parse_frame(&self, _name: &str, data: &str) -> SseEvent {
        parse_frame(data)
    }

    fn delta_of(&self, event: &SseEvent) -> Option<Delta> {
        delta_of(event)
    }

    fn is_done(&self, event: &SseEvent) -> bool {
        *event == SseEvent::Done
    }
}

/// One streaming completion over `provider`'s endpoint
/// (`POST {base_url}/chat/completions`). Honors the error algebra (ADR-0002):
/// never `Err`, never panic - every failure is a Response with an `Error`
/// stop reason and whatever partial content had streamed. The send/status/stream
/// flow is the shared [`transport`] driver; this adapter supplies only the
/// OpenAI request (URL + Bearer auth) and its SSE strategy.
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
    let request = authorized(reqwest::Client::new().post(&url), provider)
        .header("content-type", "application/json")
        .json(&payload);
    let protocol = OpenaiProtocol {
        style: req.tool_call_style,
    };
    transport::stream_completion(request, &protocol, on_event).await
}

/// The read-only models listing (`GET {base_url}/models`, ADR-0002 amendment)
/// with Bearer auth. The response shape is the one both REST APIs share; the
/// shared [`transport`] driver owns the send/status/parse flow.
pub(super) async fn list_models(provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    // The discovery cap (see [`super::DISCOVERY_TIMEOUT`]): a blackholed
    // host times out into the same request_failed Err as any other failure.
    let client = reqwest::Client::builder()
        .timeout(super::DISCOVERY_TIMEOUT)
        .build()
        .map_err(request_err)?;
    let request = authorized(client.get(&url), provider);
    transport::fetch_models(request).await
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

#[cfg(test)]
#[path = "../../tests/llm/openai_completions.rs"]
mod tests;
