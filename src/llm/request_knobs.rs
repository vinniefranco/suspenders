//! Shared request-shaping decisions - the knob spine both wire builders walk.
//!
//! Each Api adapter owns its dialect grammar end to end (field names, nesting,
//! message shapes - ADR-0037), but WHEN a knob applies and WHAT value it
//! carries are boundary-wide decisions that must not drift between dialects:
//! omit-when-empty tools, omit-when-`None` temperature, and the no-think /
//! thinking-budget precedence. This module states each of those decisions
//! once; `anthropic_messages::request` and `openai_completions::request` call
//! in and keep only their grammar. A knob a single dialect emits (`top_p`,
//! `top_k`, the extended-thinking param's shape) stays in that adapter - a
//! real per-dialect difference is not forced shared.

use serde_json::{Map, Value, json};

use crate::content::ToolSpec;
use crate::llm::LlmRequest;

/// Inserts `"tools"` only when the request offers any (a Compaction request
/// offers none) - the key the server should default is omitted, not sent
/// empty. Both dialects name the key `tools`; each passes its own per-tool
/// grammar as `wire_tool`.
pub fn insert_tools(
    obj: &mut Map<String, Value>,
    tools: &[ToolSpec],
    wire_tool: fn(&ToolSpec) -> Value,
) {
    if !tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(tools.iter().map(wire_tool).collect()),
        );
    }
}

/// Inserts `"temperature"` only when the request carries one - `None` leaves
/// sampling with the server (omitted, not sent empty). Both dialects name the
/// key `temperature`.
pub fn insert_temperature(obj: &mut Map<String, Value>, request: &LlmRequest) {
    if let Some(temp) = request.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
}

/// The resolved thinking knob: what one request says about the model's
/// Thinking, with the precedence decided once for every dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// `no_think` set: tell the server to skip Thinking for this request
    /// (the checkNextSpeaker side-query answers directly). Wins over any
    /// budget - the two are mutually exclusive by decision, not by wire.
    Suppress,
    /// A budget armed and no suppression: extended thinking with this many
    /// tokens, for the dialect that has a thinking param to arm.
    Budget(u64),
    /// Neither: thinking stays with the server's own defaults.
    Server,
}

/// Resolves the request's `no_think` / `thinking_budget` pair into the one
/// [`Thinking`] decision. Each adapter matches the decision into its own
/// grammar: both emit the no-think field on [`Thinking::Suppress`], only the
/// Anthropic dialect has a thinking param for [`Thinking::Budget`] (the OpenAI
/// path gets reasoning via `reasoning_content` and ignores it).
pub fn thinking(request: &LlmRequest) -> Thinking {
    if request.no_think {
        Thinking::Suppress
    } else if let Some(budget) = request.thinking_budget {
        Thinking::Budget(budget)
    } else {
        Thinking::Server
    }
}

/// Inserts the no-think field: `chat_template_kwargs.enable_thinking: false`,
/// the llama.cpp/vLLM chat-template extension. Both dialects carry it
/// verbatim - the value is typed once here so it cannot drift.
pub fn insert_no_think(obj: &mut Map<String, Value>) {
    obj.insert(
        "chat_template_kwargs".into(),
        json!({ "enable_thinking": false }),
    );
}

#[cfg(test)]
#[path = "../../tests/llm/request_knobs.rs"]
mod tests;
