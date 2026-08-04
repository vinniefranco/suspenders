//! Pure wire-format conversion for the Anthropic Messages API.
//!
//! Converts the typed [`LlmRequest`] and the captured [`Model`] into the
//! string-keyed JSON this wire protocol expects. This module never references
//! the transport - it produces a plain `serde_json::Value` that the adapter
//! sends, nothing added downstream.
//!
//! There is exactly ONE request-construction entry point, [`build_request`],
//! and only the adapter calls it (ADR-0037): every caller outside the llm
//! module speaks the typed [`LlmRequest`] seam, never wire JSON.

use serde_json::{Map, Value, json};

use crate::content::Message;
use crate::content::ToolSpec;
use crate::content::{ContentBlock, ResultBlock};
use crate::llm::LlmRequest;
use crate::llm::model::Model;

/// Builds the complete Anthropic Messages API payload as JSON.
///
/// Sets the model (the Model's bare id - scoping is a Suspenders fact, not a
/// wire one), system prompt, max_tokens (the Model's output cap), streaming
/// flag, messages, tool specs, and (conditionally) temperature, the no-think
/// field, and the extended-thinking budget (mutually exclusive with no-think).
/// Keys the server should default are omitted, not sent empty:
/// no `"tools"` when `tools` is empty (a Compaction request offers none),
/// no `"temperature"` when the request carries `None` (sampling stays with the
/// server).
pub fn build_request(request: &LlmRequest, model: &Model) -> Value {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(model.id));
    obj.insert("system".into(), json!(request.system));
    obj.insert("max_tokens".into(), json!(model.max_tokens));
    obj.insert("stream".into(), json!(true));
    obj.insert(
        "messages".into(),
        Value::Array(request.messages.iter().map(wire_message).collect()),
    );

    if !request.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(request.tools.iter().map(wire_tool).collect()),
        );
    }

    if let Some(temp) = request.temperature {
        obj.insert("temperature".into(), json!(temp));
    }

    // no_think and the thinking budget are mutually exclusive: no_think means
    // "answer directly, no reasoning" (the checkNextSpeaker side-query), so it
    // suppresses the thinking param. Otherwise a Some budget arms extended
    // thinking, which keeps the local reasoning model producing a Thinking
    // block THEN a Tool Call every turn (qwen-code parity).
    //
    // NB: qwen-code sends budget_tokens (32000) larger than max_tokens (8000)
    // and llama.cpp accepts it - so no max_tokens>budget guard here. A real
    // Claude endpoint would reject budget>max_tokens, but the target here is
    // the local reasoning model.
    if request.no_think {
        obj.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
        );
    } else if let Some(budget) = request.thinking_budget {
        obj.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": budget }),
        );
    }

    Value::Object(obj)
}

/// Converts one tool spec to a wire-format map. `input_schema` is already
/// string-keyed JSON Schema and passes through unchanged.
pub fn wire_tool(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}

/// Converts one message to a wire-format map: role plus content blocks. Text,
/// tool_use, and thinking blocks match the `#[serde(tag = "type")]` derive
/// exactly, but a ToolResult's `content` is our internal [`ResultBlock`] list
/// (ADR-0059) whose media variants serialize as `{type:"image",data}` - NOT the
/// Anthropic `source.base64` shape. So ToolResult is special-cased through the
/// explicit [`wire_tool_result_content`] visitor (ADR-0002); the other blocks
/// pass through the derive. Built field by field, NOT by serializing the whole
/// [`Message`]: Provenance (ADR-0037) is a Suspenders fact and never rides the
/// wire.
pub fn wire_message(message: &Message) -> Value {
    let content: Vec<Value> = message.content.iter().map(wire_block).collect();
    json!({
        "role": message.role,
        "content": content,
    })
}

/// One content block on the Anthropic wire, built explicitly per variant (the
/// same discipline ToolResult always used, now uniform): each block's wire
/// shape is spelled out so the conversion is total and infallible - no
/// serialize step that could fail and force a panic arm. ToolResult's block-list
/// content routes through [`wire_tool_result_content`] so media reaches the wire
/// in the `source.base64` shape.
fn wire_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { text } => json!({ "type": "thinking", "text": text }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "is_error": is_error,
            "content": wire_tool_result_content(content),
        }),
        // First-class user-message media (ADR-0068, At Expansion): the same
        // `source.base64` shape a media Tool Result uses, built through the
        // shared [`base64_media`] visitor. The wire-build-time degrade pass
        // ([`crate::llm::transform`]) has already replaced media the target
        // Model cannot accept with a Text placeholder, so a block reaching here
        // is one the Model supports.
        ContentBlock::Image { mime, data } => base64_media("image", mime, data),
        ContentBlock::Document { mime, data } => base64_media("document", mime, data),
    }
}

/// The Anthropic `tool_result.content` array (ADR-0002, ADR-0059): each
/// [`ResultBlock`] as a wire block. A Text block is `{type:"text",text}`; media
/// rides as `{type,source:{type:"base64",media_type,data}}` - image as `image`,
/// PDF as `document`. The wire-build-time degrade pass ([`crate::llm::transform`])
/// has already replaced any media the target Model cannot accept with a Text
/// placeholder, so a media block reaching here is one the Model supports.
pub fn wire_tool_result_content(blocks: &[ResultBlock]) -> Value {
    Value::Array(
        blocks
            .iter()
            .map(|block| match block {
                ResultBlock::Text { text } => json!({ "type": "text", "text": text }),
                ResultBlock::Image { mime, data } => base64_media("image", mime, data),
                ResultBlock::Document { mime, data } => base64_media("document", mime, data),
            })
            .collect(),
    )
}

// One base64 media block in the Anthropic `source` shape.
fn base64_media(block_type: &str, mime: &str, data: &str) -> Value {
    json!({
        "type": block_type,
        "source": { "type": "base64", "media_type": mime, "data": data },
    })
}

#[cfg(test)]
#[path = "../../../tests/llm/anthropic_messages/request.rs"]
mod tests;
