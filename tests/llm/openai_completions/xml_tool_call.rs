use super::*;
use serde_json::json;

// Mirror the qwen test helpers: build invoke/parameter blocks from parts so the
// literal markup never accidentally trips the harness's own tooling.
fn invoke(name: &str, params: &str) -> String {
    format!("<invoke name=\"{name}\">{params}</invoke>")
}

fn param(name: &str, value: &str) -> String {
    format!("<parameter name=\"{name}\">{value}</parameter>")
}

// ---------------------------------------------------------------------------
// contains_xml_tool_calls
// ---------------------------------------------------------------------------

#[test]
fn detects_an_invoke_block() {
    assert!(contains_xml_tool_calls(&invoke(
        "read_file",
        &param("p", "v")
    )));
}

#[test]
fn returns_false_for_plain_text() {
    assert!(!contains_xml_tool_calls("just some text"));
}

#[test]
fn is_stable_across_repeated_calls() {
    let text = invoke("read_file", &param("p", "v"));
    assert!(contains_xml_tool_calls(&text));
    assert!(contains_xml_tool_calls(&text));
    assert!(contains_xml_tool_calls(&text));
}

// ---------------------------------------------------------------------------
// extract_xml_tool_calls
// ---------------------------------------------------------------------------

fn call(name: &str, input: Value) -> ParsedCall {
    ParsedCall {
        name: name.to_string(),
        input,
    }
}

#[test]
fn extracts_a_single_tool_call() {
    let text = invoke("read_file", &param("file_path", "a.ts"));
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("read_file", json!({ "file_path": "a.ts" }))]
    );
}

#[test]
fn extracts_multiple_tool_calls() {
    let text = format!(
        "{}\n{}",
        invoke("read_file", &param("file_path", "a.ts")),
        invoke("run_shell_command", &param("command", "ls")),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![
            call("read_file", json!({ "file_path": "a.ts" })),
            call("run_shell_command", json!({ "command": "ls" })),
        ]
    );
}

#[test]
fn extracts_multiple_parameters_for_one_call() {
    let text = invoke(
        "edit",
        &format!("{}{}", param("file_path", "a.ts"), param("old_string", "x")),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call(
            "edit",
            json!({ "file_path": "a.ts", "old_string": "x" })
        )]
    );
}

#[test]
fn skips_invoke_blocks_without_parameters() {
    let text = invoke("no_params", "some body but no parameters");
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn parses_structured_json_but_preserves_scalar_strings() {
    let text = invoke(
        "tool",
        &format!(
            "{}{}{}{}{}{}",
            param("count", "3"),
            param("flag", "true"),
            param("opts", "{\"a\": 1}"),
            param("list", "[1, 2]"),
            param("plain", "hello world"),
            param("nil", "null"),
        ),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call(
            "tool",
            json!({
                "count": "3",
                "flag": "true",
                "opts": { "a": 1 },
                "list": [1, 2],
                "plain": "hello world",
                "nil": "null",
            })
        )]
    );
}

#[test]
fn preserves_raw_string_for_malformed_json_values() {
    let text = invoke(
        "tool",
        &format!("{}{}", param("data", "{not valid json"), param("ok", "yes")),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call(
            "tool",
            json!({ "data": "{not valid json", "ok": "yes" })
        )]
    );
}

#[test]
fn extracts_multi_line_parameter_values() {
    let text = invoke(
        "edit",
        &format!(
            "{}{}",
            param("file_path", "/some/path/file.tsx"),
            param("old_string", "line1,\nline2,\nline3,"),
        ),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call(
            "edit",
            json!({
                "file_path": "/some/path/file.tsx",
                "old_string": "line1,\nline2,\nline3,",
            })
        )]
    );
}

#[test]
fn strips_only_delimiting_newlines_preserving_whitespace() {
    let text = invoke("edit", &param("old_string", "\n    return null;\n"));
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("edit", json!({ "old_string": "    return null;" }))]
    );
}

#[test]
fn does_not_crash_on_malformed_or_nested_xml() {
    assert_eq!(extract_xml_tool_calls("<invoke name=\"x\"><invoke"), vec![]);
    assert_eq!(extract_xml_tool_calls("</invoke><invoke>"), vec![]);
    // A nested invoke: the outer non-greedy body closes at the first
    // `</invoke>`, so it never yields a parameterless outer call - just an
    // array, no panic.
    let nested = invoke("outer", &invoke("inner", &param("p", "v")));
    let _: Vec<ParsedCall> = extract_xml_tool_calls(&nested);
}

#[test]
fn returns_consistent_results_across_repeated_calls() {
    let text = invoke("read_file", &param("p", "v"));
    let first = extract_xml_tool_calls(&text);
    let second = extract_xml_tool_calls(&text);
    assert_eq!(second, first);
    assert_eq!(second.len(), 1);
}

#[test]
fn is_safe_against_proto_parameter_names() {
    let text = invoke(
        "tool",
        &format!(
            "{}{}",
            param("__proto__", "{\"polluted\": true}"),
            param("safe", "yes"),
        ),
    );
    let result = extract_xml_tool_calls(&text);
    assert_eq!(result.len(), 1);
    let args = &result[0].input;
    // The `__proto__` key is stored verbatim as an ordinary Map key; there is
    // no prototype-pollution concept in Rust.
    assert_eq!(args.get("safe"), Some(&json!("yes")));
    assert_eq!(args.get("__proto__"), Some(&json!({ "polluted": true })));
    assert_eq!(args.get("polluted"), None);
}

// --- fenced code blocks ---

#[test]
fn skips_invoke_blocks_inside_fenced_code_blocks() {
    let text = format!(
        "```xml\n{}\n```",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn extracts_non_fenced_invokes_while_skipping_fenced_ones() {
    let real_call = invoke("read_file", &param("file_path", "a.ts"));
    let fenced = format!(
        "```xml\n{}\n```",
        invoke("run_shell_command", &param("command", "echo hello")),
    );
    let text = format!("{real_call}\n{fenced}");
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("read_file", json!({ "file_path": "a.ts" }))]
    );
}

#[test]
fn skips_invokes_inside_a_tilde_fence_that_contains_backtick_lines() {
    let text = format!(
        "~~~markdown\nHere is an example:\n```xml\n{}\n```\n~~~",
        invoke("run_shell_command", &param("command", "echo hello")),
    );
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn treats_a_shorter_same_delimiter_fence_as_content() {
    let text = format!(
        "````markdown\n```xml\n{}\n```\n````",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn treats_a_closing_fence_with_info_string_as_content() {
    let text = format!(
        "````markdown\n```xml\n{}\n```xml\n````",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn treats_a_closing_fence_with_trailing_text_as_content() {
    let text = format!(
        "~~~markdown\n{}\n~~~ end of examples\n~~~",
        invoke("run_shell_command", &param("command", "echo hi")),
    );
    assert_eq!(extract_xml_tool_calls(&text), vec![]);
}

#[test]
fn extracts_a_later_invoke_when_earlier_param_has_unclosed_fence() {
    let edit_with_fence = invoke(
        "edit",
        &format!(
            "{}{}",
            param("file_path", "docs.md"),
            param("old_string", "```ts\nconst x = 1;"),
        ),
    );
    let read_call = invoke("read_file", &param("file_path", "a.ts"));
    let text = format!("{edit_with_fence}\n{read_call}");
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![
            call(
                "edit",
                json!({ "file_path": "docs.md", "old_string": "```ts\nconst x = 1;" })
            ),
            call("read_file", json!({ "file_path": "a.ts" })),
        ]
    );
}

#[test]
fn extracts_a_later_invoke_when_earlier_param_has_closed_fence_pair() {
    let edit_with_fence = invoke("edit", &param("old_string", "```\ncode\n```"));
    let read_call = invoke("read_file", &param("file_path", "b.ts"));
    let text = format!("{edit_with_fence}\n{read_call}");
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![
            call("edit", json!({ "old_string": "```\ncode\n```" })),
            call("read_file", json!({ "file_path": "b.ts" })),
        ]
    );
}

#[test]
fn still_skips_prose_fence_when_parameters_also_contain_fences() {
    let text = format!(
        "```markdown\n{}\n```\n{}",
        invoke("edit", &param("old_string", "```\ninner\n```")),
        invoke("read_file", &param("file_path", "c.ts")),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("read_file", json!({ "file_path": "c.ts" }))]
    );
}

// --- entity decoding ---

#[test]
fn decodes_xml_entities_in_parameter_values() {
    let text = invoke(
        "edit",
        &format!(
            "{}{}",
            param("old_string", "if (a &lt; b) &amp;&amp; c &gt; d"),
            param("new_string", "x &apos;y&apos; &quot;z&quot;"),
        ),
    );
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call(
            "edit",
            json!({
                "old_string": "if (a < b) && c > d",
                "new_string": "x 'y' \"z\"",
            })
        )]
    );
}

#[test]
fn decodes_amp_last_so_amp_lt_becomes_literal_lt() {
    let text = invoke("tool", &param("v", "&amp;lt;"));
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("tool", json!({ "v": "&lt;" }))]
    );
}

#[test]
fn leaves_values_without_entities_unchanged() {
    let text = invoke("tool", &param("v", "plain text"));
    assert_eq!(
        extract_xml_tool_calls(&text),
        vec![call("tool", json!({ "v": "plain text" }))]
    );
}

#[test]
fn supports_single_quoted_attribute_values() {
    let text = "<invoke name='read_file'><parameter name='file_path'>a.ts</parameter></invoke>";
    assert_eq!(
        extract_xml_tool_calls(text),
        vec![call("read_file", json!({ "file_path": "a.ts" }))]
    );
}

// ---------------------------------------------------------------------------
// try_recover_xml_tool_calls
// ---------------------------------------------------------------------------

#[test]
fn reports_no_recovery_when_there_are_no_tool_calls() {
    let result = try_recover_xml_tool_calls("plain text only");
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, "plain text only");
}

#[test]
fn recovers_calls_from_xml_content() {
    let result = try_recover_xml_tool_calls(&invoke("read_file", &param("file_path", "a.ts")));
    assert!(result.recovered);
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "read_file");
    assert_eq!(result.calls[0].input, json!({ "file_path": "a.ts" }));
}

#[test]
fn preserves_short_surrounding_text_in_remaining_text() {
    let text = format!(
        "Sure.\n{}",
        invoke("read_file", &param("file_path", "a.ts"))
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.remaining_text, "Sure.");
}

#[test]
fn returns_empty_remaining_text_when_content_is_only_xml() {
    let result = try_recover_xml_tool_calls(&invoke("read_file", &param("file_path", "a.ts")));
    assert!(result.recovered);
    assert_eq!(result.remaining_text, "");
}

#[test]
fn does_not_recover_when_substantial_prose_surrounds_the_xml() {
    let prose = "Here is how you use the tool. First you open the file, then you read it. \
        The invoke block below shows the format. Remember to always check the path. \
        This is a documentation example for the read_file tool call format. \
        You should never execute these examples directly. They are for illustration \
        purposes only. The actual tool calls are made through the structured API.";
    let text = format!(
        "{prose}\n{}",
        invoke("read_file", &param("file_path", "a.ts"))
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
}

#[test]
fn prose_ratio_is_measured_in_utf16_code_units_not_bytes() {
    // CJK prose is 3 bytes/char but one UTF-16 code unit. qwen measures the
    // prose-dominance ratio in JS `.length` (UTF-16), so this call is recovered;
    // a UTF-8 byte ratio would weight the CJK reasoning 3x, cross 0.8, and wrongly
    // decline. Qwen models reason in CJK often enough for this to be a real case.
    let prose = "读取这个文件的内容。".repeat(15); // 150 CJK chars
    let call = invoke("read_file", &param("file_path", "a.ts"));
    let text = format!("{prose}\n{call}");

    // Precondition: the BYTE ratio exceeds the threshold (the pre-fix behavior
    // would have declined here), so the test proves the UTF-16 parity fix.
    assert!(
        (prose.len() as f64) / (text.len() as f64) > 0.8,
        "precondition: byte ratio should exceed 0.8"
    );

    let result = try_recover_xml_tool_calls(&text);
    assert!(
        result.recovered,
        "UTF-16 ratio is below 0.8, so the call must be recovered"
    );
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "read_file");
}

#[test]
fn recovers_when_reasoning_prose_precedes_the_xml() {
    let reasoning = "I need to fix the authentication token validation in the middleware. \
        The current implementation does not check the expiry date, which means \
        expired tokens are still accepted. This is a security vulnerability that \
        could allow unauthorized access. I will update the validateToken function \
        to check the exp claim and reject tokens that have expired. The fix involves \
        adding a date comparison after the signature verification step. I also need \
        to make sure the error message is clear about why the token was rejected. \
        Let me look at the current implementation and make the necessary changes. \
        The file is located in the src/middleware directory. I will use the edit tool \
        to replace the old validation logic with the new one that includes expiry \
        checking. This should be a straightforward change that does not affect other \
        parts of the codebase. The test suite should still pass after this change. \
        I have verified that no other middleware depends on the old behavior. \
        The change is backward compatible because valid tokens will still be accepted.";
    let edit_block = invoke(
        "edit",
        &format!(
            "{}{}{}",
            param("file_path", "/project/src/middleware/auth.ts"),
            param(
                "old_string",
                "function validateToken(token: string): boolean {\n  \
                 const decoded = jwt.verify(token, SECRET);\n  \
                 return decoded !== null;\n}",
            ),
            param(
                "new_string",
                "function validateToken(token: string): boolean {\n  \
                 const decoded = jwt.verify(token, SECRET);\n  \
                 if (!decoded || !decoded.exp) return false;\n  \
                 return Date.now() < decoded.exp * 1000;\n}",
            ),
        ),
    );
    let text = format!("{reasoning}\n{edit_block}");
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "edit");
    let input = &result.calls[0].input;
    assert!(input.get("file_path").is_some());
    assert!(input.get("old_string").is_some());
    assert!(input.get("new_string").is_some());
    assert_eq!(result.remaining_text, reasoning);
}

#[test]
fn preserves_parameterless_invoke_blocks_as_plain_text() {
    let parameterless = invoke("think", "Let me reason about this problem");
    let parameterized = invoke("read_file", &param("file_path", "a.ts"));
    let text = format!("{parameterized}\n{parameterless}");
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.calls.len(), 1);
    assert!(result.remaining_text.contains(&parameterless));
}

#[test]
fn does_not_recover_an_invoke_example_inside_a_fenced_code_block() {
    let text = format!(
        "```xml\n{}\n```",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, text);
}

#[test]
fn recovers_a_real_invoke_while_excluding_a_fenced_example_after_it() {
    let real_call = invoke("read_file", &param("file_path", "a.ts"));
    let fenced = format!(
        "```xml\n{}\n```",
        invoke("run_shell_command", &param("command", "echo hello")),
    );
    let text = format!("{real_call}\n{fenced}");
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].name, "read_file");
    assert_eq!(result.calls[0].input, json!({ "file_path": "a.ts" }));
    assert!(result.remaining_text.contains("```xml"));
    assert!(result.remaining_text.contains("echo hello"));
}

#[test]
fn does_not_recover_an_invoke_inside_a_tilde_fence_containing_backtick_lines() {
    let text = format!(
        "~~~markdown\nExample:\n```xml\n{}\n```\n~~~",
        invoke("run_shell_command", &param("command", "echo hi")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, text);
}

#[test]
fn does_not_recover_an_invoke_nested_in_a_longer_same_delimiter_fence() {
    let text = format!(
        "````markdown\n```xml\n{}\n```\n````",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, text);
}

#[test]
fn does_not_recover_an_invoke_when_closing_fence_carries_info_string() {
    let text = format!(
        "````markdown\n```xml\n{}\n```xml\n````",
        invoke("run_shell_command", &param("command", "rm -rf /tmp/x")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, text);
}

#[test]
fn does_not_recover_an_invoke_when_closing_fence_has_trailing_text() {
    let text = format!(
        "~~~markdown\n{}\n~~~ end of examples\n~~~",
        invoke("run_shell_command", &param("command", "echo hi")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(!result.recovered);
    assert_eq!(result.calls, vec![]);
    assert_eq!(result.remaining_text, text);
}

#[test]
fn recovers_both_invokes_when_first_has_a_fence_like_parameter_value() {
    let edit_with_fence = invoke("edit", &param("old_string", "```\nunclosed fence"));
    let read_call = invoke("read_file", &param("file_path", "a.ts"));
    let text = format!("{edit_with_fence}\n{read_call}");
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.calls.len(), 2);
    assert_eq!(result.calls[0].name, "edit");
    assert_eq!(result.calls[1].name, "read_file");
}

#[test]
fn strips_an_empty_function_calls_wrapper_from_remaining_text() {
    let text = format!(
        "<function_calls>\n{}\n</function_calls>",
        invoke("read_file", &param("file_path", "a.ts")),
    );
    let result = try_recover_xml_tool_calls(&text);
    assert!(result.recovered);
    assert_eq!(result.remaining_text, "");
}
