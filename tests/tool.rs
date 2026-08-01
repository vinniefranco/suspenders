
use super::*;

impl ToolCtx {
    /// A ctx over the full built-in tool registry and a denying Approver, for
    /// tests. The single test construction site, so a future ctx field touches
    /// one place rather than every tool test helper.
    pub fn for_test(root: PathBuf, result_cap: usize) -> ToolCtx {
        ToolCtx {
            root,
            result_cap,
            command_timeout_ms: 120_000,
            input_modalities: Modalities::default(),
            // Tests get Project-Root-only confinement by default; the memory
            // trust-path tests opt in explicitly by setting this field.
            memory_root: None,
            // Tests default the session dir to the OS temp dir; the run_command
            // background tests only read this to name the capture file.
            session_dir: std::env::temp_dir(),
            caps: caps::Capabilities::for_test(),
        }
    }

    /// A test ctx whose captured Model accepts the given input modalities, so
    /// read_file's read-time media path (P3 3b) is exercisable: an image/PDF
    /// rides as a media block only when the matching modality is true.
    pub fn for_test_with_modalities(
        root: PathBuf,
        result_cap: usize,
        input_modalities: Modalities,
    ) -> ToolCtx {
        ToolCtx {
            input_modalities,
            ..ToolCtx::for_test(root, result_cap)
        }
    }
}

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
