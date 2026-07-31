use super::*;
use crate::content::{ContentBlock, Role};
use crate::llm::model::Api;

fn make_tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

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
    let msg = Message::assistant(vec![make_tool_use(
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
        name: "list_directory".into(),
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
    assert_eq!(tools_arr[0]["name"], json!("list_directory"));

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
