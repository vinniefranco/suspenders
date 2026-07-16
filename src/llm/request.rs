//! Pure wire-format conversion for the Anthropic Messages API.
//!
//! Converts the project's typed content blocks and tool specs into the
//! string-keyed JSON the wire protocol expects. This module never references
//! the transport - it produces a plain `serde_json::Value` that the `Llm`
//! boundary sends, nothing added downstream. Its interface is one pure
//! function: [`build`].
//!
//! By factoring request-building out of the boundary, the module is tested
//! without a mock server: feed in the project's shapes, assert the output.
//!
//! There is exactly ONE public request-construction entry point,
//! [`build_request`], taking a typed [`LlmRequest`] and a [`Connection`]. Every
//! caller - the Turn, the Scout, and Compaction - routes through it; the
//! string-argument [`build`] is a private helper of this module, so the wire
//! format has a single typed seam and tests assert it through that seam.

use serde_json::{Map, Value, json};

use crate::content::Message;
use crate::session::connection::Connection;
use crate::tool::ToolSpec;

/// A typed request as the caller assembles it. [`build_request`] renders it -
/// together with a [`Connection`] - to the complete Anthropic wire payload.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// The break-glass no-think rescue flag (DESIGN.md: Empty-response Nudge)
    /// and the Scout default (ADR-0014). When `true`, the payload gains
    /// `"chat_template_kwargs": {"enable_thinking": false}`.
    pub no_think: bool,
}

impl LlmRequest {
    pub fn new(system: impl Into<String>, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Self {
        LlmRequest {
            system: system.into(),
            messages,
            tools,
            no_think: false,
        }
    }

    pub fn with_no_think(mut self, no_think: bool) -> Self {
        self.no_think = no_think;
        self
    }
}

/// Builds the complete Anthropic Messages API payload as JSON.
///
/// Sets the model, system prompt, max_tokens, streaming flag, messages, tool
/// specs, and (conditionally) temperature and the no-think field. Keys the
/// server should default are omitted, not sent empty: no `"tools"` when
/// `tools` is empty (a Compaction request offers none, and so does the Scout's
/// forced report Pass - ADR-0014), no `"temperature"` when the connection
/// carries `None` (sampling stays with the server).
///
/// Private to this module: the only public entry point is [`build_request`],
/// so no caller reaches past the typed [`LlmRequest`] seam.
fn build(
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    connection: &Connection,
    no_think: bool,
) -> Value {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(connection.model));
    obj.insert("system".into(), json!(system));
    obj.insert("max_tokens".into(), json!(connection.max_tokens));
    obj.insert("stream".into(), json!(true));
    obj.insert(
        "messages".into(),
        Value::Array(messages.iter().map(wire_message).collect()),
    );

    if !tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(tools.iter().map(wire_tool).collect()),
        );
    }

    if let Some(temp) = connection.temperature {
        obj.insert("temperature".into(), json!(temp));
    }

    if no_think {
        obj.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": false }),
        );
    }

    Value::Object(obj)
}

/// Renders an [`LlmRequest`] to wire JSON.
pub fn build_request(request: &LlmRequest, connection: &Connection) -> Value {
    build(
        &request.system,
        &request.messages,
        &request.tools,
        connection,
        request.no_think,
    )
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

/// Converts one message to a wire-format map. The typed [`Message`] already
/// serializes to the Anthropic wire shape via serde (the `#[serde(tag =
/// "type")]` content-block enum matches text/tool_use/tool_result/thinking).
pub fn wire_message(message: &Message) -> Value {
    serde_json::to_value(message).expect("Message serializes to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Role};

    fn connection() -> Connection {
        Connection::new("http://test:4000/v1", "tk", "m", 16_000)
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

        let req = build("You are.", &messages, &tools, &connection(), false);

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
    fn empty_tools_omit_the_key() {
        let req = build("system", &[], &[], &connection(), false);
        assert!(req.as_object().unwrap().get("tools").is_none());
        assert_eq!(req["messages"], json!([]));
    }

    #[test]
    fn nil_temperature_omits_key_configured_one_rides() {
        let req = build("s", &[], &[], &connection(), false);
        assert!(req.as_object().unwrap().get("temperature").is_none());

        let with_temp_conn = connection().with_temperature(Some(0.7));
        let with_temp = build("s", &[], &[], &with_temp_conn, false);
        assert_eq!(with_temp["temperature"], json!(0.7));
    }

    #[test]
    fn no_think_carries_kwargs_false_byte_identical_to_absent() {
        let armed = build("s", &[], &[], &connection(), true);
        assert_eq!(
            armed["chat_template_kwargs"],
            json!({"enable_thinking": false})
        );

        // no_think:false has no chat_template_kwargs key at all - byte-identical
        // to a normal request.
        let plain = build("s", &[], &[], &connection(), false);
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
}
