//! The value threaded through one Tool Call's Plugin lifecycle (ADR-0007).
//!
//! Plug-inspired, not Plug: in baud the lifecycle spans two processes and
//! three points in time, so there is no single `call/2` - the token carries
//! what crosses those gaps instead. `assigns` is plugin state threading
//! `pre_run` into `post_run`; `artifacts` is display-side data (CONTEXT.md:
//! Artifact) that rides the `:tool_result` event to Presentment and never
//! enters the Conversation. `halt` denies the Tool Call; the reason is
//! plugin-voiced and becomes the `is_error` Tool Result content.
//!
//! ## How the Token holds ctx (judgment call)
//!
//! baud's Token holds `ctx :: Baud.Tool.ctx()` (a shared, cheaply-copied map)
//! directly. The Rust [`ToolCtx`] is `Clone` (its `scout` capture is an
//! `Arc`-backed effect), so the Token owns a `ToolCtx` by value - the same
//! ownership shape as baud, without threading a lifetime through the whole
//! Plugin trait. The pipeline reads `token.ctx` for `Tools::execute` and for
//! the Result Cap; a plugin may read it but never needs to mutate it.

use std::collections::HashMap;

use serde_json::Value;

use crate::tool::ToolCtx;

/// The raw Tool Result a Plugin sees mid-pipeline: the content the model
/// would see and whether it was an error. Mirrors baud's `Token.result/0`
/// (`%{content, is_error}`) - distinct from [`crate::tools::ToolResult`] only
/// in that it is the in-flight, pre-Shaping value the `post_run` fold rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResult {
    pub content: String,
    pub is_error: bool,
}

/// The value threaded through one Tool Call's Plugin lifecycle.
///
/// `result` is `None` until the Tool executes (set by the pipeline before the
/// `post_run` fold). `assigns` and `artifacts` are keyed by `String` (baud
/// keys them by atom); `Value` holds arbitrary plugin state, mirroring baud's
/// `term()` values.
#[derive(Debug, Clone)]
pub struct Token {
    pub tool: String,
    pub input: Value,
    pub ctx: ToolCtx,
    pub result: Option<TokenResult>,
    pub assigns: HashMap<String, Value>,
    pub artifacts: HashMap<String, Value>,
    pub halted: bool,
    pub halt_reason: Option<String>,
}

impl Token {
    /// A fresh token for one Tool Call, before any stage ran.
    pub fn new(tool: impl Into<String>, input: Value, ctx: ToolCtx) -> Self {
        Token {
            tool: tool.into(),
            input,
            ctx,
            result: None,
            assigns: HashMap::new(),
            artifacts: HashMap::new(),
            halted: false,
            halt_reason: None,
        }
    }

    /// Stores plugin state under `key`, threading `pre_run` into `post_run`.
    /// Returns the token so it threads through a fold, mirroring baud's
    /// `Token.assign/3`.
    pub fn assign(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.assigns.insert(key.into(), value.into());
        self
    }

    /// Attaches an Artifact (CONTEXT.md): display-side data that rides the
    /// `:tool_result` event to Presentment. Never enters the Conversation.
    pub fn put_artifact(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.artifacts.insert(key.into(), value.into());
        self
    }

    /// Denies the Tool Call. The reason is the plugin's own wording and
    /// becomes the `is_error` Tool Result content; the Tool never executes and
    /// the remaining `pre_run` stages are skipped.
    pub fn halt(mut self, reason: impl Into<String>) -> Self {
        self.halted = true;
        self.halt_reason = Some(reason.into());
        self
    }
}
