use super::*;
use serde_json::json;

// The exact multi-parameter run_command fixture the loop tests use
// (`run::loop_` at the markup fixtures).
const RUN_COMMAND: &str = "<tool_call>\n<function=run_shell_command>\n<parameter=command>\nmix test\n</parameter>\n</function>\n</tool_call>";

#[test]
fn parses_the_exact_run_command_fixture() {
    let parse = extract_tool_calls(RUN_COMMAND).expect("markup parses");
    assert_eq!(parse.preamble, "");
    assert_eq!(
        parse.calls,
        vec![ParsedCall {
            name: "run_shell_command".into(),
            input: json!({ "command": "mix test" }),
        }]
    );
}

#[test]
fn preamble_precedes_a_single_call() {
    let text = format!("I need to update the file:\n\n{RUN_COMMAND}");
    let parse = extract_tool_calls(&text).expect("markup parses");
    assert_eq!(parse.preamble, "I need to update the file:");
    assert_eq!(parse.calls.len(), 1);
    assert_eq!(parse.calls[0].name, "run_shell_command");
}

#[test]
fn multiple_sequential_calls_each_surface() {
    let second = "<tool_call>\n<function=list_directory>\n<parameter=path>\n.\n</parameter>\n</function>\n</tool_call>";
    let text = format!("{RUN_COMMAND}\n{second}");
    let parse = extract_tool_calls(&text).expect("markup parses");
    assert_eq!(parse.calls.len(), 2);
    assert_eq!(parse.calls[0].name, "run_shell_command");
    assert_eq!(parse.calls[0].input, json!({ "command": "mix test" }));
    assert_eq!(parse.calls[1].name, "list_directory");
    assert_eq!(parse.calls[1].input, json!({ "path": "." }));
}

#[test]
fn json_in_tags_variant_is_recovered() {
    let text =
        "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"mix.exs\"}}</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    assert_eq!(
        parse.calls,
        vec![ParsedCall {
            name: "read_file".into(),
            input: json!({ "path": "mix.exs" }),
        }]
    );
}

#[test]
fn json_variant_without_arguments_maps_to_an_empty_object() {
    let text = "<tool_call>{\"name\": \"list_directory\"}</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    assert_eq!(parse.calls[0].input, json!({}));
}

#[test]
fn multi_line_parameter_values_keep_their_interior_newlines() {
    let text = "<tool_call>\n<function=edit>\n<parameter=content>\nline one\nline two\n</parameter>\n</function>\n</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    // Only the single surrounding newline is trimmed; the interior stays.
    assert_eq!(
        parse.calls[0].input,
        json!({ "content": "line one\nline two" })
    );
}

#[test]
fn a_json_looking_parameter_value_keeps_its_parsed_shape() {
    let text = "<tool_call>\n<function=set_flag>\n<parameter=enabled>\ntrue\n</parameter>\n<parameter=count>\n3\n</parameter>\n</function>\n</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    assert_eq!(parse.calls[0].input, json!({ "enabled": true, "count": 3 }));
}

#[test]
fn a_bare_word_parameter_stays_a_string_not_misparsed_as_json() {
    // `null`, `true`, numbers - a value that trims to bare text stays a
    // string unless it is genuine non-string JSON. A plain word is a
    // string.
    let text = "<tool_call>\n<function=run_shell_command>\n<parameter=command>\nls\n</parameter>\n</function>\n</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    assert_eq!(parse.calls[0].input, json!({ "command": "ls" }));
}

#[test]
fn a_parameterless_function_yields_an_empty_input() {
    let text = "<tool_call>\n<function=list_directory>\n</function>\n</tool_call>";
    let parse = extract_tool_calls(text).expect("markup parses");
    assert_eq!(
        parse.calls,
        vec![ParsedCall {
            name: "list_directory".into(),
            input: json!({}),
        }]
    );
}

#[test]
fn text_with_no_markup_returns_none() {
    assert_eq!(extract_tool_calls("Just a plain answer, no tools."), None);
    assert_eq!(extract_tool_calls(""), None);
}

#[test]
fn prose_mentioning_the_markup_inline_returns_none() {
    // The markup names appear only INSIDE a sentence, never at the start of
    // a line - a conclusion, not a call (the line-anchor rule).
    let text = "Done. I could not run mix test - the <tool_call> was withdrawn.";
    assert_eq!(extract_tool_calls(text), None);
}

#[test]
fn a_function_opener_alone_on_a_line_also_triggers_parsing() {
    // Some emissions lead with <function= directly (no <tool_call> wrapper
    // on its own line). The pre-check accepts it; the parse still needs a
    // <tool_call> opener to bound the body, so a bare function without the
    // wrapper yields no calls - None, not a panic.
    let text = "<function=run_shell_command>";
    assert_eq!(extract_tool_calls(text), None);
}

#[test]
fn a_truncated_call_missing_its_close_still_surfaces() {
    // A stream that died mid-markup: the opener is line-anchored, the close
    // never arrived. The partial body still yields the call rather than
    // vanishing.
    let text = "<tool_call>\n<function=run_shell_command>\n<parameter=command>\nmix test";
    let parse = extract_tool_calls(text).expect("partial markup parses");
    assert_eq!(parse.calls[0].name, "run_shell_command");
    assert_eq!(parse.calls[0].input, json!({ "command": "mix test" }));
}
