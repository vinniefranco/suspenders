//! The Plugin contract (CONTEXT.md): units of extension that wrap the Tool
//! Call lifecycle. ADR-0007 records the shape; [`crate::plugins`] runs the
//! pipeline.
//!
//! In baud all three callbacks are optional (`@optional_callbacks`); a plugin
//! implements only the stages it cares about. Rust has no optional trait
//! methods, so both stages carry a **default = identity** body: a plugin that
//! does not override a stage passes the token through unchanged, exactly like
//! a baud plugin that does not export it.
//!
//! * `pre_run` - before the Tool executes. May replace the token's input,
//!   [`Token::halt`] the call, or capture state into `assigns`. Runs after the
//!   Duplicate Nudge check and before the Approval gate, so the user always
//!   approves the plugin-adjusted command.
//! * `post_run` - after execution, before Shaping. May transform
//!   `token.result` (the content the model sees) and attach Artifacts.
//!
//! * `present` - the PURE Presentment stage inside the Transcript store. Given
//!   a [`crate::ui::transcript::TranscriptItem`] and the append's Artifacts, it
//!   returns the item to display - unchanged (default = identity) or replaced
//!   (e.g. a one-line Tool Result summary rewritten into a diff `Block`). No
//!   IO: it runs in the view and folds, in registration order, over EVERY item
//!   the store appends (ADR-0034 widened this from the two tool items) - Info
//!   and Voice lines, User promotions, and settled streaming text included.
//!   Only a Tool Result carries its Artifacts; every other append arrives with
//!   an empty map. A plugin therefore matches the item kinds it means to
//!   reshape and passes everything else through - the identity default already
//!   does.
//!
//! Failure isolation is fail-open (ADR-0007): a crashing stage is skipped and
//! reported as an info line in the Transcript; the model never sees it and the
//! Turn never fails because of it. That isolation lives in the pipeline
//! (`catch_unwind`), not here.

pub mod token;

use std::collections::HashMap;

use serde_json::Value;

use crate::ui::transcript::TranscriptItem;

pub use token::{Token, TokenResult};

/// The authoring contract a Suspenders Plugin implements (baud's `Baud.Plugin`
/// behaviour). Both stages default to identity, so a plugin overrides only the
/// stages it cares about.
///
/// `opts` is the plugin's registration options (baud's `keyword()`), carried
/// as a `serde_json::Value` - the open edge for per-plugin config.
pub trait Plugin: Send + Sync {
    /// Runs before the Tool executes. Default: identity.
    fn pre_run(&self, token: Token, _opts: &Value) -> Token {
        token
    }

    /// Runs after execution, before Shaping. Default: identity.
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        token
    }

    /// The PURE Presentment stage: folds over EVERY [`TranscriptItem`] the
    /// Transcript store appends (ADR-0034) - not just Tool Calls/Results -
    /// reading the append's Artifacts (an empty map for every non-Tool-Result
    /// append), and returns the item to display. Match the kinds you mean to
    /// reshape; pass everything else through. Default: identity. No IO.
    /// Mirrors baud's `present/3`.
    fn present(
        &self,
        item: TranscriptItem,
        _artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        item
    }
}
