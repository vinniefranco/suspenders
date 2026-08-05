//! Recovering Tool Calls a model emitted as Anthropic/Claude-style XML in the
//! content channel.
//!
//! This is a SECOND text-emitted dialect, distinct from the Hermes/Qwen-coder
//! `<function=NAME>`/`<tool_call>` markup that [`super::text_tool_call`]
//! recovers. Some models (prompted or fine-tuned on the Claude tool-use format)
//! serialize a call as an `<invoke name="NAME">` block whose arguments are
//! `<parameter name="KEY">VALUE</parameter>` pairs, dropped into `delta.content`
//! instead of the structured channel. When the structured fold in
//! `super::stream` comes back empty and the Hermes recovery does not hit, this
//! module is the last resort: a pure parse over the accumulated content string,
//! no I/O, no transport reference.
//!
//! ```text
//! <invoke name="read_file">
//! <parameter name="file_path">a.ts</parameter>
//! </invoke>
//! ```
//!
//! ## Guards that keep documentation from becoming a call
//!
//! The dialect is the one models are most likely to *document* (it is the
//! canonical Claude tool-use format), so recovery is deliberately conservative:
//!
//! - **Fenced code blocks** (CommonMark §4.5): an `<invoke>` inside a ```` ``` ````
//!   or `~~~` fence is an example, not a call, and is skipped. Fence detection
//!   ignores fence-like lines *inside* invoke ranges so a triple-backtick line
//!   in an `edit` old_string never corrupts fence state.
//! - **Prose dominance**: if the surrounding prose (all invoke blocks removed,
//!   3+ newlines collapsed, trimmed) is more than 80% of the text, the model is
//!   documenting the format and nothing is recovered.
//! - **Parameterless invokes** are skipped and preserved as plain text.
//! - **Conservative scalars**: only a value whose trimmed form starts with `{`
//!   or `[` is JSON-parsed; every other value stays a string, so a file named
//!   `null` or a port `"8080"` is preserved verbatim.
//!
//! Faithful port of qwen-code's `core/xml-tool-call-fallback.ts` (v0.21.4).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::text_tool_call::ParsedCall;

/// `<invoke name="NAME">BODY</invoke>` - the attribute value may be single or
/// double quoted (Rust `regex` has no backreferences, so, exactly as qwen's own
/// pattern, the open and close quote are not required to match). `(?s)` makes
/// `.` span newlines; the body is non-greedy so nested/adjacent invokes bound
/// tightly.
static INVOKE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<invoke\s+name=["']([^"']+)["']>(.*?)</invoke>"#).unwrap());

/// `<parameter name="KEY">VALUE</parameter>` - same quoting and dotall rules as
/// [`INVOKE_PATTERN`].
static PARAMETER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<parameter\s+name=["']([^"']+)["']>(.*?)</parameter>"#).unwrap()
});

/// The outcome of an XML recovery: whether any call was recovered, the recovered
/// calls, and the remaining text (surrounding prose with the recovered invoke
/// blocks stripped). Mirrors qwen's `tryRecoverXmlToolCalls` return shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    pub recovered: bool,
    pub calls: Vec<ParsedCall>,
    pub remaining_text: String,
}

impl Recovery {
    /// The "nothing recovered" outcome: no calls, the content handed back
    /// unchanged as `remaining_text`. Both early returns in
    /// [`try_recover_xml_tool_calls`] (no invoke blocks, or prose dominates)
    /// share this shape.
    fn none(text: &str) -> Self {
        Recovery {
            recovered: false,
            calls: Vec::new(),
            remaining_text: text.to_string(),
        }
    }
}

/// Prose-dominance ceiling: when the non-invoke prose (UTF-16 code units) is more
/// than this fraction of the content, the model is documenting the format rather
/// than emitting a call, so nothing is recovered. qwen's `0.8` intent guard.
const PROSE_DOMINANCE_MAX: f64 = 0.8;

/// Detects whether `text` contains an `<invoke name="...">` block.
pub fn contains_xml_tool_calls(text: &str) -> bool {
    INVOKE_PATTERN.is_match(text)
}

/// Decodes the five predefined XML entities in a parameter value so recovered
/// args match the literal text the model intended. Models emitting this dialect
/// commonly escape `<`/`&` inside code payloads; without decoding, an `edit`
/// old_string never matches the file and a `write_file` content writes literal
/// `&lt;` into the user's source. `&amp;` is decoded LAST so `&amp;lt;`
/// correctly becomes `&lt;`. The scan is skipped entirely when the value has no
/// `&`.
fn decode_xml_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parses a raw parameter value. Only values that look like structured JSON
/// (objects or arrays) are parsed; scalar strings are preserved as-is to avoid
/// coercing e.g. a file named `null` or a string port `8080`. A JSON parse
/// failure keeps the original (pre-trim) raw value as a string.
fn parse_parameter_value(value: &str) -> Value {
    let trimmed = value.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) => return parsed,
            Err(_) => return Value::String(value.to_string()),
        }
    }
    Value::String(value.to_string())
}

/// Strips a single newline immediately after the open tag and immediately before
/// the close tag - the convention this XML dialect follows - while preserving
/// the remaining whitespace that tools treat as significant (e.g. the
/// indentation that makes an `edit` old_string unique). A blanket trim corrupted
/// such arguments.
fn strip_delimiting_newlines(value: &str) -> &str {
    let value = value.strip_prefix('\n').unwrap_or(value);
    value.strip_suffix('\n').unwrap_or(value)
}

/// Precomputes the character (byte) ranges spanned by invoke blocks so fence
/// detection can skip their interiors. Without this, a fence-like line inside a
/// parameter value (e.g. an `edit` whose `old_string` contains a triple-backtick
/// line) opens a fence that is never closed, causing later invoke blocks to be
/// silently skipped.
fn compute_invoke_ranges(text: &str) -> Vec<(usize, usize)> {
    INVOKE_PATTERN
        .find_iter(text)
        .map(|m| (m.start(), m.end()))
        .collect()
}

/// The opener of a fenced code block: its delimiter character and the length of
/// the opening run.
struct OpenFence {
    delim: char,
    len: usize,
}

/// Matches a fence line: up to three leading spaces, then a run of 3+ backticks
/// OR 3+ tildes. Group 1 is the whole leading-space-plus-delimiter prefix (its
/// length is where the info string begins); group 2, when present, is the
/// backtick run (so its presence distinguishes a backtick fence from a tilde
/// fence).
static FENCE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}((`{3,})|~{3,})").unwrap());

/// Returns true when `index` falls inside an unclosed fenced code block.
///
/// Tracks delimiter type and length so a fence is only closed by a run of the
/// same delimiter that is at least as long as the opener, consistent with
/// CommonMark §4.5 (a shorter same-delimiter run is content, not a close). A
/// closing fence must also be whitespace-only after the delimiter run -
/// CommonMark forbids an info string on a closing fence.
///
/// Lines inside `invoke_ranges` are skipped: they are parameter values, not
/// prose, so fence-like content there must not affect fence state.
fn position_inside_fence(text: &str, index: usize, invoke_ranges: &[(usize, usize)]) -> bool {
    let mut open_fence: Option<OpenFence> = None;
    let mut line_start = 0usize;
    // `split('\n')` on the JS side yields the exact lines (no trailing-newline
    // handling needed): the slice up to `index` is what we walk.
    for line in text[..index].split('\n') {
        let line_end = line_start + line.len();
        let inside_invoke = invoke_ranges
            .iter()
            .any(|&(start, end)| line_start >= start && line_end <= end);
        // Advance past this line and its `\n` for the next iteration.
        line_start = line_end + 1;
        if inside_invoke {
            continue;
        }
        let Some(caps) = FENCE_PATTERN.captures(line) else {
            continue;
        };
        // Groups 0 (whole match) and 1 (the delimiter run) are mandatory in
        // FENCE_PATTERN, so a match guarantees both; skip defensively rather
        // than unwrap, matching the no-panic idiom of the sibling recovery.
        let (Some(whole), Some(prefix)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let delim = if caps.get(2).is_some() { '`' } else { '~' };
        let len = prefix.as_str().chars().count();
        match &open_fence {
            None => open_fence = Some(OpenFence { delim, len }),
            Some(open) => {
                if open.delim == delim && len >= open.len && line[whole.end()..].trim().is_empty() {
                    open_fence = None;
                }
            }
        }
    }
    open_fence.is_some()
}

/// Extracts XML-style tool calls from plain text content.
///
/// Invoke blocks inside fenced code blocks (```` ``` ```` or `~~~`) are skipped:
/// they document the format rather than emitting a tool call. Parameterless
/// invoke blocks are skipped as well (conservative). Returns an empty vector
/// when none are found.
pub fn extract_xml_tool_calls(text: &str) -> Vec<ParsedCall> {
    let mut results = Vec::new();
    let invoke_ranges = compute_invoke_ranges(text);

    for m in INVOKE_PATTERN.captures_iter(text) {
        // Groups 0/1/2 are mandatory in INVOKE_PATTERN; a match guarantees all
        // three, so skip defensively rather than unwrap (no-panic idiom).
        let (Some(whole), Some(name), Some(body)) = (m.get(0), m.get(1), m.get(2)) else {
            continue;
        };
        if position_inside_fence(text, whole.start(), &invoke_ranges) {
            continue;
        }

        let tool_name = name.as_str();

        // A serde_json Map stores keys verbatim: there is no prototype-pollution
        // concept in Rust, so a `__proto__` parameter name is just a literal key.
        let mut args = Map::new();
        for param in PARAMETER_PATTERN.captures_iter(body.as_str()) {
            let (Some(key), Some(raw)) = (param.get(1), param.get(2)) else {
                continue;
            };
            let value = decode_xml_entities(strip_delimiting_newlines(raw.as_str()));
            args.insert(key.as_str().to_string(), parse_parameter_value(&value));
        }

        if !tool_name.is_empty() && !args.is_empty() {
            results.push(ParsedCall {
                name: tool_name.to_string(),
                input: Value::Object(args),
            });
        }
    }

    results
}

/// Attempts to recover tool calls from XML-formatted text content.
///
/// If XML tool calls are found and dominate the content, returns the recovered
/// calls and the remaining text (with recovered XML blocks removed).
/// Parameterless invoke blocks are preserved as plain text since
/// [`extract_xml_tool_calls`] intentionally skips them.
pub fn try_recover_xml_tool_calls(text: &str) -> Recovery {
    let extracted = extract_xml_tool_calls(text);

    if extracted.is_empty() {
        return Recovery::none(text);
    }

    // Intent guard: only recover when XML blocks dominate the content.
    // Substantial surrounding prose suggests the model is documenting or echoing
    // the format, not emitting a tool call. Measure against ALL invoke blocks
    // (including parameterless ones the extraction skips).
    let prose_only = collapse_blank_lines(&INVOKE_PATTERN.replace_all(text, ""))
        .trim()
        .to_string();
    // Ratio in UTF-16 code units, not UTF-8 bytes: qwen measures JS string
    // `.length` (`proseOnly.length / text.length`), and a byte ratio would weight
    // multi-byte prose (CJK reasoning text - common from Qwen models - is 3 bytes
    // but one code unit) heavily enough to flip the 0.8 recover/decline threshold.
    if !text.is_empty()
        && (utf16_len(&prose_only) as f64) / (utf16_len(text) as f64) > PROSE_DOMINANCE_MAX
    {
        return Recovery::none(text);
    }

    let invoke_ranges = compute_invoke_ranges(text);

    // Strip only invoke blocks that contain parameters and are outside fenced
    // code blocks (the ones we actually recovered). Parameterless and fenced
    // blocks are preserved as plain text.
    let stripped = INVOKE_PATTERN.replace_all(text, |caps: &regex::Captures<'_>| {
        // Group 0 (the whole match) always exists inside a replace callback;
        // fall back to the empty replacement rather than unwrap (no-panic idiom).
        let Some(block) = caps.get(0) else {
            return String::new();
        };
        if position_inside_fence(text, block.start(), &invoke_ranges) {
            return block.as_str().to_string();
        }
        if PARAMETER_PATTERN.is_match(block.as_str()) {
            String::new()
        } else {
            block.as_str().to_string()
        }
    });
    let without_wrapper = FUNCTION_CALLS_WRAPPER.replace_all(&stripped, "");
    let remaining_text = collapse_blank_lines(&without_wrapper).trim().to_string();

    Recovery {
        recovered: true,
        calls: extracted,
        remaining_text,
    }
}

/// An empty `<function_calls></function_calls>` wrapper (any whitespace between
/// the tags): stripped from the remaining text so a model that wrapped its
/// invokes in the Claude container leaves no residue.
static FUNCTION_CALLS_WRAPPER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<function_calls>\s*</function_calls>").unwrap());

/// Collapses any run of 3+ newlines to exactly two, mirroring qwen's
/// `.replace(/\n{3,}/g, '\n\n')`.
static BLANK_LINES_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

fn collapse_blank_lines(text: &str) -> std::borrow::Cow<'_, str> {
    BLANK_LINES_PATTERN.replace_all(text, "\n\n")
}

/// The length of `text` in UTF-16 code units - JS string `.length`, the unit
/// qwen's prose-dominance ratio measures. Matches for astral characters too
/// (a surrogate pair counts as two, exactly as JS sees it).
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
#[path = "../../../tests/llm/openai_completions/xml_tool_call.rs"]
mod tests;
