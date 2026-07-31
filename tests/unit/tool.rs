use super::*;

// ---- validate/2 + type_name ----

use serde_json::json;

#[test]
fn type_name_calls_a_string_a_string() {
    assert_eq!(type_name(&json!("hi")), "string");
}

#[test]
fn type_name_calls_signed_and_unsigned_integers_integer() {
    // Both sides of the `is_i64() || is_u64()` guard: a negative fits only
    // i64, u64::MAX fits only u64 - each alone must still read "integer".
    assert_eq!(type_name(&json!(-3)), "integer");
    let unsigned = serde_json::Value::Number(serde_json::Number::from(u64::MAX));
    assert_eq!(type_name(&unsigned), "integer");
}

#[test]
fn type_name_calls_a_fractional_number_a_float() {
    assert_eq!(type_name(&json!(1.5)), "float");
}

#[test]
fn type_name_calls_a_bool_a_boolean() {
    assert_eq!(type_name(&json!(true)), "boolean");
}

#[test]
fn type_name_calls_an_object_a_map() {
    // Elixir vocabulary on purpose (baud heritage): an object is a "map".
    assert_eq!(type_name(&json!({"a": 1})), "map");
}

#[test]
fn type_name_calls_an_array_a_list() {
    assert_eq!(type_name(&json!([1, 2])), "list");
}

#[test]
fn type_name_calls_null_an_atom() {
    // baud reported `nil` - an atom - so the wording survives the port.
    assert_eq!(type_name(&json!(null)), "atom");
}

#[test]
fn validate_reports_the_offending_value_with_its_type_name() {
    // The malformed-input message the model sees: the value inspected,
    // then its type name in parentheses.
    let schema = json!({
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    });
    let mut input = serde_json::Map::new();
    input.insert("path".to_string(), json!(42));
    assert_eq!(
        validate(&schema, &input),
        Err("field \"path\" should be a string, got: 42 (integer)".to_string())
    );
}
