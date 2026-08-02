
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
        name: "list_directory".into(),
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
    // A successful result is byte-identical to before; an error result
    // carries the Voice's in-band marker - this dialect's role:"tool"
    // message has no error slot (ADR-0037).
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
            json!({ "role": "tool", "tool_call_id": "call_2", "content": "[tool error] oops" }),
        ]
    );
}

#[test]
fn an_error_result_is_prefixed_with_the_voices_marker() {
    let mut out = Vec::new();
    wire_messages(
        &Message::user(vec![ContentBlock::tool_result(
            "call_1",
            "No such file: mix.exs",
            true,
        )]),
        &mut out,
    );
    assert_eq!(
        out[0]["content"],
        json!(format!(
            "{} No such file: mix.exs",
            crate::voice::Marker::ToolError.text()
        ))
    );
}

#[test]
fn a_successful_result_carries_no_marker() {
    let mut out = Vec::new();
    wire_messages(
        &Message::user(vec![ContentBlock::tool_result("call_1", "ok", false)]),
        &mut out,
    );
    assert_eq!(
        out,
        vec![json!({ "role": "tool", "tool_call_id": "call_1", "content": "ok" })]
    );
}

#[test]
fn user_tail_text_follows_the_tool_messages() {
    // The Run's results tail (text riding the results, e.g. Steering or a
    // run-close marker) stays after the tool messages, as it does in the
    // typed message.
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
fn nil_top_p_omits_key_configured_one_rides() {
    let req = build("s", vec![], vec![]);
    assert!(req.as_object().unwrap().get("top_p").is_none());

    let with_top_p = build_request(
        &LlmRequest {
            top_p: Some(0.8),
            ..LlmRequest::new("s", vec![], vec![])
        },
        &model(),
    );
    assert_eq!(with_top_p["top_p"], json!(0.8));
}

#[test]
fn nil_top_k_omits_key_configured_one_rides() {
    let req = build("s", vec![], vec![]);
    assert!(req.as_object().unwrap().get("top_k").is_none());

    let with_top_k = build_request(
        &LlmRequest {
            top_k: Some(20),
            ..LlmRequest::new("s", vec![], vec![])
        },
        &model(),
    );
    assert_eq!(with_top_k["top_k"], json!(20));
}

#[test]
fn thinking_budget_is_ignored_on_the_openai_wire() {
    // The OpenAI path gets reasoning via reasoning_content automatically;
    // the thinking budget is simply not part of this dialect. A request
    // carrying it produces no thinking-related key.
    let req = build_request(
        &LlmRequest::new("s", vec![], vec![]).with_thinking_budget(Some(32_000)),
        &model(),
    );
    assert!(req.as_object().unwrap().get("thinking").is_none());
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
