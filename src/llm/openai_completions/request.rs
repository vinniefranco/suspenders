//! Pure wire-format conversion for the OpenAI Chat Completions API.
//!
//! Converts the typed [`LlmRequest`] and the captured [`Model`] into the
//! string-keyed JSON this wire protocol expects. This module never references
//! the transport - it produces a plain `serde_json::Value` that the adapter
//! sends, nothing added downstream.
//!
//! ## Dialect differences from `anthropic_messages::request`
//!
//! - The system prompt is a leading `{"role": "system"}` message, not a
//!   top-level `system` key.
//! - Tool Calls are the assistant message's `tool_calls` array (with `input`
//!   JSON-encoded into the `function.arguments` STRING), not content blocks.
//! - Tool Results are `{"role": "tool"}` messages of their own, so one typed
//!   [`Message`] can fan out to several wire messages.
//! - Tool specs nest under `{"type": "function", "function": {...}}` with the
//!   schema keyed `parameters`, not `input_schema`.
//! - Usage on the streamed response must be requested explicitly via
//!   `stream_options.include_usage`.

use serde_json::{Map, Value, json};

use crate::content::ToolSpec;
use crate::content::{ContentBlock, Message, ResultBlock, Role};
use crate::llm::LlmRequest;
use crate::llm::model::Model;
use crate::llm::request_knobs::{self, Thinking};

/// The wire name of the output-cap field. Newer OpenAI models want
/// `max_completion_tokens`; models.dev records no such compat fact, so the
/// generated Catalog cannot carry it (Stage C outcome) - this constant stays
/// the one place a per-Model compat flag would plug in, when a host we serve
/// needs it. The Catalog's openai-completions hosts all accept `max_tokens`.
const MAX_TOKENS_FIELD: &str = "max_tokens";

/// Builds the complete Chat Completions payload as JSON.
///
/// Sets the model (the Model's bare id - scoping is a Suspenders fact, not a
/// wire one), the leading system message, the output cap, the streaming flag
/// plus `stream_options` (usage rides the final chunk only when asked), the
/// fanned-out messages, tool specs, and (conditionally) the sampling knobs
/// (`temperature`, `top_p`, `top_k`) and the no-think field. Keys the server
/// should default are omitted, not sent empty: no `"tools"` when `tools` is
/// empty, no `"temperature"`/`"top_p"`/`"top_k"` when the request carries
/// `None`. WHEN each dialect-shared knob applies is the shared
/// `request_knobs` decision spine (one place, both dialects); `top_p` and
/// `top_k` stay here because only this wire emits them.
pub fn build_request(request: &LlmRequest, model: &Model) -> Value {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(model.id));
    obj.insert(MAX_TOKENS_FIELD.into(), json!(model.max_tokens));
    obj.insert("stream".into(), json!(true));
    obj.insert("stream_options".into(), json!({ "include_usage": true }));

    let mut messages = vec![json!({ "role": "system", "content": request.system })];
    for message in &request.messages {
        wire_messages(message, &mut messages);
    }
    obj.insert("messages".into(), Value::Array(messages));

    request_knobs::insert_tools(&mut obj, &request.tools, wire_tool);
    request_knobs::insert_temperature(&mut obj, request);

    // Sampling cutoffs (Qwen3-Coder tuning): each omitted, not sent empty,
    // when the request carries `None` - the same discipline as temperature.
    // Dialect-local by decision (LlmRequest documents them as this wire's
    // knobs), so they stay out of the shared spine.
    if let Some(top_p) = request.top_p {
        obj.insert("top_p".into(), json!(top_p));
    }

    if let Some(top_k) = request.top_k {
        obj.insert("top_k".into(), json!(top_k));
    }

    match request_knobs::thinking(request) {
        Thinking::Suppress => request_knobs::insert_no_think(&mut obj),
        // This dialect has no thinking param to arm: reasoning rides back as
        // `reasoning_content` unprompted, so a budget is simply not part of
        // this wire (pinned by `thinking_budget_is_ignored_on_the_openai_wire`).
        Thinking::Budget(_) | Thinking::Server => {}
    }

    Value::Object(obj)
}

/// Converts one tool spec to a wire-format map. The dialect nests the spec
/// under `function` and names the JSON Schema `parameters`.
pub fn wire_tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        }
    })
}

/// Converts one typed [`Message`] into its wire messages, appending to `out`.
/// One typed message can fan out: each ToolResult block becomes its own
/// `{"role": "tool"}` message (this dialect carries tool results as messages,
/// not content blocks), and the remaining text becomes one role message.
///
/// Ordering: a typed message's blocks emit in order, and the single
/// text/tool_calls message for an assistant emits as one unit - so an
/// assistant message carrying `tool_calls` always precedes the tool messages
/// that answer it (they live in the FOLLOWING typed user message). Thinking
/// never enters the Conversation (CONTEXT.md); dropped defensively here.
pub fn wire_messages(message: &Message, out: &mut Vec<Value>) {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    // First-class user-message media (ADR-0068, At Expansion): image/document
    // parts collected here promote the message content from a plain string to
    // the OpenAI content-parts array. Empty on the common (text-only) path, so
    // that message still emits a plain-string `content`.
    let mut media_parts: Vec<Value> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(wire_tool_call(id, name, input));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": tool_content(content, *is_error),
                // `content` is a `&Vec<ResultBlock>` (ADR-0059).
            })),
            ContentBlock::Thinking { .. } => {}
            ContentBlock::Image { mime, data } => media_parts.push(image_url_part(mime, data)),
            ContentBlock::Document { mime, data } => media_parts.push(file_part(mime, data)),
        }
    }

    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let has_media = !media_parts.is_empty();
    let mut wire = Map::new();
    wire.insert("role".into(), json!(role));
    if has_media {
        // Media present: this dialect carries media as content parts, so the
        // string content is promoted to the parts array (ADR-0068) - a leading
        // `text` part when any text rides, then each media part in order.
        wire.insert("content".into(), content_parts(&text, media_parts));
    } else if !text.is_empty() || tool_calls.is_empty() {
        // Content is omitted (not sent empty) when tool_calls carry the message.
        wire.insert("content".into(), json!(text));
    }
    if !tool_calls.is_empty() {
        wire.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    // An assistant message always emits - its text and tool_calls are one
    // wire unit. A user message emits only when text OR media remains: its Tool
    // Results have already fanned out as role:"tool" messages above.
    if message.role == Role::Assistant || !text.is_empty() || has_media {
        out.push(Value::Object(wire));
    }
}

/// The OpenAI content-parts array (ADR-0068): a leading `text` part when text
/// rides, then the media parts in order. The shape a message takes once it
/// carries first-class user media.
fn content_parts(text: &str, media_parts: Vec<Value>) -> Value {
    let mut parts = Vec::with_capacity(media_parts.len() + 1);
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    parts.extend(media_parts);
    Value::Array(parts)
}

/// One image content part (ADR-0068): qwen's openai converter emits an image as
/// `{type:"image_url", image_url:{url:"data:<mime>;base64,<data>"}}`
/// (`converter.ts` `createMediaContentPart`), matched verbatim here.
fn image_url_part(mime: &str, data: &str) -> Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{data}") },
    })
}

/// One document (PDF) content part (ADR-0068): qwen's openai converter emits a
/// PDF as `{type:"file", file:{filename, file_data:"data:<mime>;base64,<data>"}}`
/// (`converter.ts` `createMediaContentPart`), matched here. The wire has no
/// source filename, so the mime stands in as the display filename.
fn file_part(mime: &str, data: &str) -> Value {
    json!({
        "type": "file",
        "file": {
            "filename": mime,
            "file_data": format!("data:{mime};base64,{data}"),
        },
    })
}

// The tool message's content: this dialect carries no media on a `role:"tool"`
// message (OpenAI multimodal tool-role is out of scope, ADR-0059), so a media
// block DEGRADES to the verbatim unsupported-modality placeholder; Text blocks
// join as before. This dialect also has no error slot, so an error result
// carries the Voice's marker in-band (ADR-0037); a successful text-only result
// passes through byte-identical.
fn tool_content(blocks: &[ResultBlock], is_error: bool) -> String {
    let content = blocks
        .iter()
        .map(|block| match block {
            ResultBlock::Text { text } => text.clone(),
            ResultBlock::Image { mime, .. } => {
                crate::content::unsupported_modality_placeholder("image", mime)
            }
            ResultBlock::Document { mime, .. } => {
                crate::content::unsupported_modality_placeholder("pdf", mime)
            }
        })
        .collect::<String>();
    if is_error {
        format!("{} {content}", crate::voice::Marker::ToolError.text())
    } else {
        content
    }
}

/// One Tool Call as a `tool_calls` entry: the dialect JSON-encodes the input
/// into the `function.arguments` string.
fn wire_tool_call(id: &str, name: &str, input: &Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(input).expect("Value serializes to JSON"),
        }
    })
}

#[cfg(test)]
#[path = "../../../tests/llm/openai_completions/request.rs"]
mod tests;
