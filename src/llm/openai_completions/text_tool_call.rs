//! Recovering Tool Calls a model emitted as text in the content channel.
//!
//! Small local models (Qwen3-Coder chief among them) do not always fill the
//! structured `delta.tool_calls[]` channel the dialect defines. When the
//! request offers tools, they instead serialize the call as Hermes/Qwen-coder
//! markup INSIDE `delta.content`. This module's line-anchored pre-check spots
//! that markup with a self-contained scan of the accumulated content. The
//! structured fold in
//! [`super::stream`] drops that text on the floor, so a perfectly good tool
//! call vanishes into prose. This module recovers it: a pure parse over the
//! accumulated content string, no I/O, no transport reference.
//!
//! ## The two markup variants
//!
//! The XML form nests a `<function=NAME>` block, each argument a
//! `<parameter=KEY>VALUE</parameter>` pair (values may be multi-line, so a
//! single leading/trailing newline is trimmed - the format puts the value on
//! its own lines):
//!
//! ```text
//! <tool_call>
//! <function=run_shell_command>
//! <parameter=command>
//! mix test
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! The JSON form (qwen-vl / Nous) carries `{"name": ..., "arguments": {...}}`
//! between the tags. Both may appear multiple times in one response (sequential
//! Tool Calls), and either may follow a one-sentence preamble.
//!
//! ## Precedence and the pre-check
//!
//! Text parsing is a FALLBACK the caller reaches only when the structured
//! channel came back empty (structured always wins - see
//! [`super::stream::StreamState::finalize`]). Before parsing anything, a cheap
//! line-anchored pre-check guards
//! against prose that merely mentions the markup inline: a line must actually
//! START with `<tool_call` or `<function=` for the parse to run, so
//! "the `<tool_call>` was withdrawn" in a sentence never triggers it.

use serde_json::{Map, Value};

/// The outcome of a successful text parse: the leading prose (empty when there
/// is none) plus one or more recovered calls.
#[derive(Debug, Clone, PartialEq)]
pub struct TextToolParse {
    pub preamble: String,
    pub calls: Vec<ParsedCall>,
}

/// One Tool Call recovered from the content text: its name and its decoded
/// input object. The input is always a JSON object (the XML form maps each
/// parameter key to its value; the JSON form carries the `arguments` object
/// verbatim).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCall {
    pub name: String,
    pub input: Value,
}

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";

/// Extracts every Tool Call the model emitted as text markup in `text`.
///
/// Returns `None` when no tool-call markup is present - either the pre-check
/// finds no line-anchored opener, or the openers it finds enclose nothing
/// parseable. Returns `Some` with the leading preamble and one or more calls
/// otherwise. The pre-check is line-anchored (a line must START with the
/// markup), so prose merely mentioning `<tool_call>` inline never triggers a
/// parse.
pub fn extract_tool_calls(text: &str) -> Option<TextToolParse> {
    if !has_line_anchored_markup(text) {
        return None;
    }

    let mut calls = Vec::new();
    let mut cursor = 0;
    let mut first_open = None;
    while let Some(open_rel) = text[cursor..].find(TOOL_CALL_OPEN) {
        let open = cursor + open_rel;
        first_open.get_or_insert(open);
        let body_start = open + TOOL_CALL_OPEN.len();
        // A missing close means a truncated stream: take the rest as the body,
        // so a partial call still surfaces rather than silently vanishing.
        let (body, next) = match text[body_start..].find(TOOL_CALL_CLOSE) {
            Some(close_rel) => {
                let close = body_start + close_rel;
                (&text[body_start..close], close + TOOL_CALL_CLOSE.len())
            }
            None => (&text[body_start..], text.len()),
        };
        if let Some(call) = parse_call(body) {
            calls.push(call);
        }
        cursor = next;
    }

    if calls.is_empty() {
        return None;
    }
    let preamble = match first_open {
        Some(open) => text[..open].trim().to_string(),
        None => String::new(),
    };
    Some(TextToolParse { preamble, calls })
}

/// The pre-check: does a trimmed line in `text` START with the markup? Line
/// anchoring is what separates a model still trying to call a tool from prose
/// that names the markup inline.
fn has_line_anchored_markup(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("<tool_call") || trimmed.starts_with("<function=")
    })
}

/// Parses one `<tool_call>...</tool_call>` body into a [`ParsedCall`]. The body
/// is either the JSON form (`{"name": ..., "arguments": {...}}`) or the XML
/// `<function=NAME>` form; the JSON form is tried first since it is
/// unambiguous, then the XML form.
fn parse_call(body: &str) -> Option<ParsedCall> {
    parse_json_call(body).or_else(|| parse_xml_call(body))
}

/// The JSON variant: the trimmed body parses as an object carrying a string
/// `name` and (optionally) an `arguments` object. A body that is not such an
/// object is `None` (the caller falls back to the XML form).
fn parse_json_call(body: &str) -> Option<ParsedCall> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    // Arguments default to an empty object when absent; a non-object
    // `arguments` is ignored in favor of the empty map (defensive).
    let input = value
        .get("arguments")
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    Some(ParsedCall { name, input })
}

/// The XML variant: a `<function=NAME>` block whose `<parameter=KEY>VALUE`
/// pairs become the input map. A body without a `<function=` opener is `None`.
/// Each value has a single surrounding newline trimmed (the format puts the
/// value on its own lines) and, if it itself parses as a JSON scalar/array/
/// object, keeps that parsed shape - otherwise it stays a string (the safe
/// default, since most tool schemas are string-valued).
fn parse_xml_call(body: &str) -> Option<ParsedCall> {
    let open = body.find("<function=")?;
    let after = open + "<function=".len();
    let name_end = body[after..].find('>')? + after;
    let name = body[after..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut input = Map::new();
    let mut cursor = name_end;
    while let Some(param_rel) = body[cursor..].find("<parameter=") {
        let param = cursor + param_rel;
        let key_start = param + "<parameter=".len();
        let Some(key_end_rel) = body[key_start..].find('>') else {
            break;
        };
        let key_end = key_start + key_end_rel;
        let key = body[key_start..key_end].trim().to_string();
        let value_start = key_end + 1;
        // A missing `</parameter>` (truncation) takes the rest of the body.
        let (raw, next) = match body[value_start..].find("</parameter>") {
            Some(close_rel) => {
                let close = value_start + close_rel;
                (&body[value_start..close], close + "</parameter>".len())
            }
            None => (&body[value_start..], body.len()),
        };
        if !key.is_empty() {
            input.insert(key, parameter_value(raw));
        }
        cursor = next;
    }

    Some(ParsedCall {
        name,
        input: Value::Object(input),
    })
}

/// Decodes one parameter's raw text: strips a single leading and trailing
/// newline (the format writes the value on its own lines), then keeps it as a
/// JSON value if the trimmed text parses as a non-string scalar, array, or
/// object - otherwise a plain string. Bare text like `mix test` stays a string
/// (it is not valid JSON), which is the safe default for string-valued schemas.
fn parameter_value(raw: &str) -> Value {
    let trimmed = raw.strip_prefix('\n').unwrap_or(raw);
    let trimmed = trimmed.strip_suffix('\n').unwrap_or(trimmed);
    if let Ok(value) = serde_json::from_str::<Value>(trimmed.trim())
        && !value.is_string()
        && (value.is_number() || value.is_boolean() || value.is_array() || value.is_object())
    {
        return value;
    }
    Value::String(trimmed.to_string())
}

#[cfg(test)]
#[path = "../../../tests/unit/llm/openai_completions/text_tool_call.rs"]
mod tests;
