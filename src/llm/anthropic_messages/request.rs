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
use crate::llm::LlmRequest;
use crate::llm::model::Model;
use crate::tool::ToolSpec;

/// Builds the complete Anthropic Messages API payload as JSON.
///
/// Sets the model (the Model's bare id - scoping is a Suspenders fact, not a
/// wire one), system prompt, max_tokens (the Model's output cap), streaming
/// flag, messages, tool specs, and (conditionally) temperature and the
/// no-think field. Keys the server should default are omitted, not sent empty:
/// no `"tools"` when `tools` is empty (a Compaction request offers none, and
/// so does the Scout's forced report Pass - ADR-0014), no `"temperature"` when
/// the request carries `None` (sampling stays with the server).
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

    if request.no_think {
        obj.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
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

/// Converts one message to a wire-format map: role plus content blocks, whose
/// serde shapes (the `#[serde(tag = "type")]` content-block enum) match the
/// Anthropic wire exactly. Built field by field, NOT by serializing the whole
/// [`Message`]: Provenance (ADR-0037) is a Suspenders fact and never rides
/// the wire.
pub fn wire_message(message: &Message) -> Value {
    json!({
        "role": message.role,
        "content": message.content,
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
        let msg = Message::user(vec![ContentBlock::tool_result("tu_1", "done", false)]);
        assert_eq!(
            wire_message(&msg),
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": "done",
                    "is_error": false
                }]
            })
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
