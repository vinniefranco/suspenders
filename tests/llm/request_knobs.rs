use super::*;
use serde_json::json;

fn req() -> LlmRequest {
    LlmRequest::new("s", vec![], vec![])
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "d".into(),
        input_schema: json!({}),
    }
}

/// A stand-in dialect grammar: just the tool's name, so the tests assert the
/// spine's decision (insert or omit) without re-testing either real grammar.
fn name_only(spec: &ToolSpec) -> Value {
    json!(spec.name)
}

// --- insert_tools ---

#[test]
fn empty_tools_omit_the_key() {
    let mut obj = Map::new();
    insert_tools(&mut obj, &[], name_only);
    assert!(obj.get("tools").is_none());
}

#[test]
fn tools_ride_through_the_dialect_grammar_in_order() {
    let mut obj = Map::new();
    insert_tools(&mut obj, &[spec("read_file"), spec("grep")], name_only);
    assert_eq!(obj["tools"], json!(["read_file", "grep"]));
}

// --- insert_temperature ---

#[test]
fn nil_temperature_omits_the_key() {
    let mut obj = Map::new();
    insert_temperature(&mut obj, &req());
    assert!(obj.get("temperature").is_none());
}

#[test]
fn configured_temperature_rides() {
    let mut obj = Map::new();
    insert_temperature(&mut obj, &req().with_temperature(Some(0.7)));
    assert_eq!(obj["temperature"], json!(0.7));
}

// --- thinking precedence ---

#[test]
fn neither_flag_leaves_thinking_with_the_server() {
    assert_eq!(thinking(&req()), Thinking::Server);
}

#[test]
fn a_budget_arms_extended_thinking() {
    assert_eq!(
        thinking(&req().with_thinking_budget(Some(32_000))),
        Thinking::Budget(32_000)
    );
}

#[test]
fn no_think_wins_over_a_set_budget() {
    // Mutually exclusive by decision: a no-think request suppresses the
    // budget even when both are set (the checkNextSpeaker side-query).
    assert_eq!(
        thinking(&req().with_no_think(true).with_thinking_budget(Some(32_000))),
        Thinking::Suppress
    );
}

#[test]
fn the_no_think_field_is_the_exact_kwargs_value() {
    let mut obj = Map::new();
    insert_no_think(&mut obj);
    assert_eq!(
        Value::Object(obj),
        json!({ "chat_template_kwargs": { "enable_thinking": false } })
    );
}
