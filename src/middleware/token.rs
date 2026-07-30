//! The value threaded through one Tool Call's Middleware lifecycle (ADR-0007;
//! ADR-0042).
//!
//! Plug-inspired, not Plug: in baud the lifecycle spans two processes and
//! three points in time, so there is no single `call/2` - the token carries
//! what crosses those gaps instead. `assigns` is Middleware state threading
//! `pre_run` into `post_run`; `artifacts` is display-side data (CONTEXT.md:
//! Artifact) that rides the `:tool_result` event to Presentment and never
//! enters the Conversation. `halt` denies the Tool Call; the reason is
//! Extension-voiced and becomes the `is_error` Tool Result content.
//!
//! ## How the Token holds ctx (judgment call)
//!
//! baud's Token holds `ctx :: Baud.Tool.ctx()` (a shared, cheaply-copied map)
//! directly. The Rust [`ToolCtx`] is `Clone` and cheap, so the Token owns a
//! `ToolCtx` by value - the same ownership shape as baud, without threading a
//! lifetime through the whole
//! Middleware trait. The pipeline reads `token.ctx` for `Tools::execute` and
//! for the Result Cap; a Middleware may read it but never needs to mutate it.

use std::collections::HashMap;

use serde_json::Value;

use crate::content::{ResultBlock, result_blocks_text};
use crate::tool::ToolCtx;

/// The raw Tool Result a Middleware sees mid-pipeline: the content the model
/// would see and whether it was an error. Mirrors baud's `Token.result/0`
/// (`%{content, is_error}`) - distinct from [`crate::tools::ToolResult`] only
/// in that it is the in-flight, pre-Shaping value the `post_run` fold rewrites.
///
/// `content` is a [`ResultBlock`] list (ADR-0059): the text-editing Middleware
/// (condense, diff, run_command) read and rewrite the TEXT through
/// [`TokenResult::text`]/[`TokenResult::set_text`], and any media blocks (only
/// P3 3b's read_file produces them) pass through the fold untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResult {
    pub content: Vec<ResultBlock>,
    pub is_error: bool,
}

impl TokenResult {
    /// A single-Text-block result - the common case a text tool produces, and
    /// the shape the pipeline sets before the `post_run` fold from a text tool's
    /// return.
    pub fn text(content: impl Into<String>, is_error: bool) -> Self {
        TokenResult {
            content: vec![ResultBlock::text(content)],
            is_error,
        }
    }

    /// The text projection of the result's blocks (ADR-0059): what the
    /// text-editing Middleware read. Text blocks concatenated, media rendered as
    /// a placeholder.
    pub fn text_of(&self) -> String {
        result_blocks_text(&self.content)
    }

    /// Replaces the TEXT of the result, keeping any media blocks in place: the
    /// condense/diff Middleware rewrite the text and the media (P3 3b) rides
    /// through. With no media (the common case) this is exactly a single Text
    /// block carrying the new text.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text_block = ResultBlock::text(text);
        let mut media: Vec<ResultBlock> = self
            .content
            .drain(..)
            .filter(|b| !matches!(b, ResultBlock::Text { .. }))
            .collect();
        if media.is_empty() {
            self.content = vec![text_block];
        } else {
            // Media-led results keep media first; otherwise text leads.
            let text_first = !matches!(media.first(), Some(ResultBlock::Image { .. }))
                && !matches!(media.first(), Some(ResultBlock::Document { .. }));
            self.content = if text_first {
                std::iter::once(text_block).chain(media.drain(..)).collect()
            } else {
                media.push(text_block);
                media
            };
        }
    }
}

/// The value threaded through one Tool Call's Middleware lifecycle.
///
/// `result` is `None` until the Tool executes (set by the pipeline before the
/// `post_run` fold). `assigns` and `artifacts` are keyed by `String` (baud
/// keys them by atom); `Value` holds arbitrary Middleware state, mirroring baud's
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

    /// Stores Middleware state under `key`, threading `pre_run` into `post_run`.
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

    /// Denies the Tool Call. The reason is the Extension's own wording and
    /// becomes the `is_error` Tool Result content; the Tool never executes and
    /// the remaining `pre_run` stages are skipped.
    // qual:api (Middleware contract, baud parity; no built-in Extension halts yet)
    pub fn halt(mut self, reason: impl Into<String>) -> Self {
        self.halted = true;
        self.halt_reason = Some(reason.into());
        self
    }
}
