//! The authoring contract Suspenders tools share.
//!
//! A tool exposes a spec (Anthropic tool format; `input_schema` is a JSON
//! Schema map) and a run function that gets the decoded input plus the
//! [`ToolCtx`] carrying the Session's Project Root, the Result Cap (applied by
//! the Tools dispatch, not by the tools), and the command timeout. The ctx is
//! built by the Session; nothing in a tool reads the cwd or config.
//!
//! ## The authoring contract
//!
//! * **Input arrives validated.** [`validate`] runs against the tool's schema
//!   before every call: required fields present, string-typed fields are
//!   strings, unknown fields rejected.
//! * **Model-supplied paths go through [`with_path`]**, which confines them to
//!   the Project Root.
//! * **Failed file operations are worded by [`file_error`]**, which formats the
//!   POSIX reason and appends closest-match suggestions on ENOENT.
//! * **Errors return, never raise.**
//! * **Size is not a tool concern** - `tools::shaping` cuts every result.

use std::path::PathBuf;

pub mod path;

/// A tool's spec in Anthropic tool format: a name, a description, and a JSON
/// Schema `input_schema` (an open edge, so it stays a `serde_json::Value`).
/// Mirrors baud's `Baud.Tool.spec/0` shape. Serializes to exactly its wire
/// shape, so the Conversation's tool-spec overhead estimate counts what a
/// request carries without reaching into an adapter.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// The authoring contract a Suspenders tool implements (baud's `Baud.Tool`
/// behaviour). `spec` is the Anthropic tool format; `run` gets the decoded
/// input (an open edge - a `serde_json::Value`) plus the [`ToolCtx`].
///
/// `run` is async so the object-safe registry can hold `Box<dyn Tool>` (via
/// `async-trait`); most tools implement a sync heuristic core and make `run` a
/// thin wrapper. Errors return (`Err`), never raise - `Tools::execute` maps an
/// `Err` to an `is_error` Tool Result.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn run(&self, input: &serde_json::Value, ctx: &ToolCtx) -> Result<String, String>;
}

/// The ctx every Tool Call executes with: the Session's Project Root, the
/// Result Cap, and the command timeout.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub root: PathBuf,
    pub result_cap: usize,
    pub command_timeout_ms: u64,
}

/// Validates the model-supplied input against a tool's JSON Schema. Returns
/// `Ok(())` or `Err(reason)` with a precise description (unknown fields,
/// missing required fields, wrong types).
pub fn validate(
    input_schema: &serde_json::Value,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let required: Vec<String> = input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let empty = serde_json::Map::new();
    let properties = input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    let known: Vec<&String> = properties.keys().collect();

    // 1. Unknown fields first.
    let unknown: Vec<&String> = input.keys().filter(|k| !known.contains(k)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown field(s): {}. Valid fields: {}",
            quote_join(unknown.iter().map(|s| s.as_str())),
            quote_join(known.iter().map(|s| s.as_str())),
        ));
    }

    // 2. Missing required fields.
    let missing: Vec<&String> = required
        .iter()
        .filter(|r| !input.contains_key(*r))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required field(s): {}. Required: {}",
            quote_join(missing.iter().map(|s| s.as_str())),
            quote_join(required.iter().map(|s| s.as_str())),
        ));
    }

    // 3. Type mismatches (string fields that aren't strings).
    let type_errors: Vec<String> = properties
        .iter()
        .filter(|(name, prop)| {
            input.contains_key(*name)
                && prop.get("type").and_then(|t| t.as_str()) == Some("string")
                && !input.get(*name).map(|v| v.is_string()).unwrap_or(false)
        })
        .filter_map(|(name, _)| {
            // `contains_key` above guarantees `get` returns `Some`; use
            // `filter_map` to propagate that without `unwrap`.
            let val = input.get(name)?;
            Some(format!(
                "field {:?} should be a string, got: {} ({})",
                name,
                inspect(val),
                type_name(val)
            ))
        })
        .collect();

    if type_errors.is_empty() {
        Ok(())
    } else {
        Err(type_errors.join("; "))
    }
}

fn quote_join<'a, I: Iterator<Item = &'a str>>(items: I) -> String {
    items
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn type_name(value: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match value {
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "float",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "map",
        Value::Array(_) => "list",
        Value::Null => "atom",
    }
}

fn inspect(value: &serde_json::Value) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
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
}
