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

/// One content block on the Anthropic wire. Every variant but ToolResult matches
/// the derive; ToolResult's block-list content is built explicitly so media
/// reaches the wire in the `source.base64` shape.
fn wire_block(block: &ContentBlock) -> Value {
    match block {
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
        other => serde_json::to_value(other).expect("content block serializes to JSON"),
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
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Role};
    use crate::llm::model::Api;

    fn model() -> Model {
        Model::new("local", "m", Api::AnthropicMessages, 64_000, 16_000)
    }

    fn build(system: &str, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Value {
        build_request(&LlmRequest::new(system, messages, tools), &model())
    }

    #[test]
    fn wire_tool_converts_spec_to_wire_format() {
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "Reads a file.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        };
        assert_eq!(
            wire_tool(&spec),
            json!({
                "name": "read_file",
                "description": "Reads a file.",
                "input_schema": {"type": "object", "properties": {}}
            })
        );
    }

    #[test]
    fn wire_message_text_block() {
        let msg = Message::user(vec![ContentBlock::text("hello")]);
        assert_eq!(
            wire_message(&msg),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "hello"}]
            })
        );
    }

    #[test]
    fn wire_message_tool_use_block() {
        let msg = Message::assistant(vec![ContentBlock::tool_use(
            "tu_1",
            "read_file",
            json!({"path": "x"}),
        )]);
        assert_eq!(
            wire_message(&msg),
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "read_file",
                    "input": {"path": "x"}
                }]
            })
        );
    }

    #[test]
    fn wire_message_tool_result_block() {
        // ADR-0059: tool_result content is a block ARRAY, a single Text block in
        // the common case - the explicit visitor, not the derive.
        let msg = Message::user(vec![ContentBlock::tool_result("tu_1", "done", false)]);
        assert_eq!(
            wire_message(&msg),
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": false,
                    "content": [{ "type": "text", "text": "done" }]
                }]
            })
        );
    }

    #[test]
    fn wire_tool_result_media_rides_as_anthropic_source_base64() {
        // ADR-0002/0059: an image block reaches the wire as the Anthropic
        // `source.base64` shape (image), a PDF as `document`; our internal
        // `{type,data}` form never rides.
        let blocks = vec![
            ResultBlock::text("here it is"),
            ResultBlock::Image {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
            ResultBlock::Document {
                mime: "application/pdf".into(),
                data: "BBBB".into(),
            },
        ];
        assert_eq!(
            wire_tool_result_content(&blocks),
            json!([
                { "type": "text", "text": "here it is" },
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" }
                },
                {
                    "type": "document",
                    "source": { "type": "base64", "media_type": "application/pdf", "data": "BBBB" }
                }
            ])
        );
    }

    #[test]
    fn wire_message_multiple_text_blocks() {
        let msg = Message::user(vec![ContentBlock::text("hi"), ContentBlock::text("there")]);
        assert_eq!(
            wire_message(&msg)["content"],
            json!([
                {"type": "text", "text": "hi"},
                {"type": "text", "text": "there"}
            ])
        );
    }

    #[test]
    fn build_assembles_complete_request() {
        let messages = vec![Message::user(vec![ContentBlock::text("hi")])];
        let tools = vec![ToolSpec {
            name: "list_files".into(),
            description: "Lists files.".into(),
            input_schema: json!({}),
        }];

        let req = build("You are.", messages, tools);

        assert_eq!(req["model"], json!("m"));
        assert_eq!(req["system"], json!("You are."));
        assert_eq!(req["max_tokens"], json!(16_000));
        assert_eq!(req["stream"], json!(true));

        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["name"], json!("list_files"));

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], json!("text"));
        assert_eq!(blocks[0]["text"], json!("hi"));
    }

    #[test]
    fn the_wire_model_is_the_bare_id_and_max_tokens_the_models_cap() {
        // The scope (`local/`) is a Suspenders fact; the host sees its own id.
        let slashed = Model::new(
            "local",
            "qwen/Qwen3.6-27B-MTP-GGUF",
            Api::AnthropicMessages,
            64_000,
            8_000,
        );
        let req = build_request(&LlmRequest::new("s", vec![], vec![]), &slashed);
        assert_eq!(req["model"], json!("qwen/Qwen3.6-27B-MTP-GGUF"));
        assert_eq!(req["max_tokens"], json!(8_000));
    }

    #[test]
    fn empty_tools_omit_the_key() {
        let req = build("system", vec![], vec![]);
        assert!(req.as_object().unwrap().get("tools").is_none());
        assert_eq!(req["messages"], json!([]));
    }

    #[test]
    fn nil_temperature_omits_key_configured_one_rides() {
        let req = build("s", vec![], vec![]);
        assert!(req.as_object().unwrap().get("temperature").is_none());

        let with_temp = build_request(
            &LlmRequest::new("s", vec![], vec![]).with_temperature(Some(0.7)),
            &model(),
        );
        assert_eq!(with_temp["temperature"], json!(0.7));
    }

    #[test]
    fn no_think_carries_kwargs_false_byte_identical_to_absent() {
        let armed = build_request(
            &LlmRequest::new("s", vec![], vec![]).with_no_think(true),
            &model(),
        );
        assert_eq!(
            armed["chat_template_kwargs"],
            json!({"enable_thinking": false})
        );

        // no_think:false has no chat_template_kwargs key at all - byte-identical
        // to a normal request.
        let plain = build("s", vec![], vec![]);
        assert!(
            plain
                .as_object()
                .unwrap()
                .get("chat_template_kwargs")
                .is_none()
        );
    }

    #[test]
    fn thinking_budget_arms_extended_thinking_when_set_and_not_no_think() {
        let armed = build_request(
            &LlmRequest::new("s", vec![], vec![]).with_thinking_budget(Some(32_000)),
            &model(),
        );
        assert_eq!(
            armed["thinking"],
            json!({ "type": "enabled", "budget_tokens": 32_000 })
        );
    }

    #[test]
    fn nil_thinking_budget_omits_the_thinking_key() {
        // Unset budget: no thinking param at all.
        let plain = build("s", vec![], vec![]);
        assert!(plain.as_object().unwrap().get("thinking").is_none());
    }

    #[test]
    fn no_think_suppresses_the_thinking_budget_even_when_set() {
        // no_think means "answer directly, no reasoning": the two are mutually
        // exclusive, so a no-think request carries chat_template_kwargs and NO
        // thinking param, even with a budget set (the checkNextSpeaker query).
        let req = build_request(
            &LlmRequest::new("s", vec![], vec![])
                .with_no_think(true)
                .with_thinking_budget(Some(32_000)),
            &model(),
        );
        assert!(req.as_object().unwrap().get("thinking").is_none());
        assert_eq!(
            req["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
    }

    #[test]
    fn role_serializes_lowercase() {
        let msg = Message::new(Role::Assistant, vec![]);
        assert_eq!(wire_message(&msg)["role"], json!("assistant"));
    }

    #[test]
    fn provenance_never_rides_the_wire() {
        let msg = Message::assistant_from(
            vec![ContentBlock::text("hi")],
            crate::content::Provenance::new("anthropic", "claude-fable-5"),
        );
        assert_eq!(
            wire_message(&msg),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}]
            })
        );
    }
}
