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

use crate::content::{ContentBlock, Message, Role};
use crate::llm::LlmRequest;
use crate::llm::model::Model;
use crate::tool::ToolSpec;

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
/// fanned-out messages, tool specs, and (conditionally) temperature and the
/// no-think field. Keys the server should default are omitted, not sent
/// empty: no `"tools"` when `tools` is empty, no `"temperature"` when the
/// request carries `None`.
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
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(wire_tool_call(id, name, input));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            })),
            ContentBlock::Thinking { .. } => {}
        }
    }

    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut wire = Map::new();
    wire.insert("role".into(), json!(role));
    // Content is omitted (not sent empty) when tool_calls carry the message.
    if !text.is_empty() || tool_calls.is_empty() {
        wire.insert("content".into(), json!(text));
    }
    if !tool_calls.is_empty() {
        wire.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    // An assistant message always emits - its text and tool_calls are one
    // wire unit. A user message emits only when text remains: its Tool
    // Results have already fanned out as role:"tool" messages above.
    if message.role == Role::Assistant || !text.is_empty() {
        out.push(Value::Object(wire));
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
mod tests {
    use super::*;
    use crate::llm::model::Api;

    fn model() -> Model {
        Model::new("groq", "m", Api::OpenaiCompletions, 64_000, 16_000)
    }

    fn build(system: &str, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Value {
        build_request(&LlmRequest::new(system, messages, tools), &model())
    }

    #[test]
    fn build_assembles_complete_request() {
        let messages = vec![Message::user(vec![ContentBlock::text("hi")])];
        let tools = vec![ToolSpec {
            name: "list_files".into(),
            description: "Lists files.".into(),
            input_schema: json!({"type": "object"}),
        }];

        let req = build("You are.", messages, tools);

        assert_eq!(req["model"], json!("m"));
        assert_eq!(req["max_tokens"], json!(16_000));
        assert_eq!(req["stream"], json!(true));
        assert_eq!(req["stream_options"], json!({ "include_usage": true }));

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], json!({ "role": "system", "content": "You are." }));
        assert_eq!(msgs[1], json!({ "role": "user", "content": "hi" }));
    }

    #[test]
    fn wire_tool_nests_the_spec_under_function_with_parameters() {
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "Reads a file.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        };
        assert_eq!(
            wire_tool(&spec),
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Reads a file.",
                    "parameters": {"type": "object", "properties": {}},
                }
            })
        );
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_json_encoded_arguments() {
        let mut out = Vec::new();
        wire_messages(
            &Message::assistant(vec![ContentBlock::tool_use(
                "call_1",
                "read_file",
                json!({ "path": "x" }),
            )]),
            &mut out,
        );
        assert_eq!(
            out,
            vec![json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"x\"}" }
                }]
            })]
        );
        // Content is omitted, not sent empty, when only tool calls ride.
        assert!(out[0].as_object().unwrap().get("content").is_none());
    }

    #[test]
    fn assistant_text_and_tool_use_ride_one_wire_message() {
        let mut out = Vec::new();
        wire_messages(
            &Message::assistant(vec![
                ContentBlock::text("Reading it."),
                ContentBlock::tool_use("call_1", "read_file", json!({})),
            ]),
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], json!("assistant"));
        assert_eq!(out[0]["content"], json!("Reading it."));
        assert_eq!(out[0]["tool_calls"][0]["id"], json!("call_1"));
    }

    #[test]
    fn tool_results_fan_out_as_role_tool_messages() {
        let mut out = Vec::new();
        wire_messages(
            &Message::user(vec![
                ContentBlock::tool_result("call_1", "contents", false),
                ContentBlock::tool_result("call_2", "oops", true),
            ]),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                json!({ "role": "tool", "tool_call_id": "call_1", "content": "contents" }),
                json!({ "role": "tool", "tool_call_id": "call_2", "content": "oops" }),
            ]
        );
    }

    #[test]
    fn user_tail_text_follows_the_tool_messages() {
        // The Turn's results tail (a Nudge riding the results) stays after
        // the tool messages, as it does in the typed message.
        let mut out = Vec::new();
        wire_messages(
            &Message::user(vec![
                ContentBlock::tool_result("call_1", "ok", false),
                ContentBlock::text("Wrap up."),
            ]),
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], json!("tool"));
        assert_eq!(out[1], json!({ "role": "user", "content": "Wrap up." }));
    }

    #[test]
    fn an_assistant_carrying_tool_calls_precedes_its_tool_messages() {
        let req = build(
            "s",
            vec![
                Message::user(vec![ContentBlock::text("read mix.exs")]),
                Message::assistant(vec![
                    ContentBlock::text("Reading it."),
                    ContentBlock::tool_use("call_1", "read_file", json!({ "path": "mix.exs" })),
                ]),
                Message::user(vec![ContentBlock::tool_result(
                    "call_1",
                    "defmodule",
                    false,
                )]),
            ],
            vec![],
        );
        let roles: Vec<&str> = req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);
    }

    #[test]
    fn thinking_blocks_are_dropped_defensively() {
        let mut out = Vec::new();
        wire_messages(
            &Message::assistant(vec![
                ContentBlock::Thinking {
                    text: "pondering".into(),
                },
                ContentBlock::text("Answer"),
            ]),
            &mut out,
        );
        assert_eq!(
            out,
            vec![json!({ "role": "assistant", "content": "Answer" })]
        );
    }

    #[test]
    fn the_wire_model_is_the_bare_id_and_max_tokens_the_models_cap() {
        // The scope (`groq/`) is a Suspenders fact; the host sees its own id.
        let slashed = Model::new(
            "or",
            "deepseek/deepseek-chat",
            Api::OpenaiCompletions,
            64_000,
            8_000,
        );
        let req = build_request(&LlmRequest::new("s", vec![], vec![]), &slashed);
        assert_eq!(req["model"], json!("deepseek/deepseek-chat"));
        assert_eq!(req["max_tokens"], json!(8_000));
    }

    #[test]
    fn empty_tools_omit_the_key() {
        let req = build("s", vec![], vec![]);
        assert!(req.as_object().unwrap().get("tools").is_none());
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
}
