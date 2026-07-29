//! The Middleware contract (CONTEXT.md): the execution-path half of an
//! Extension, wrapping the Tool Call lifecycle. ADR-0007 records the
//! lifecycle; ADR-0042 split the former combined trait into Middleware
//! (execution) and [`crate::presenter::Presenter`] (display).
//! [`crate::extensions`] runs the pipeline.
//!
//! Both stages carry a **default = identity** body, so a Middleware overrides
//! only the stages it cares about; one that does not override a stage passes
//! the token through unchanged.
//!
//! * `pre_run` - before the Tool executes. May replace the token's input,
//!   [`Token::halt`] the call, or capture state into `assigns`. Runs before the
//!   Approval gate, so the user always approves the Middleware-adjusted command.
//! * `post_run` - after execution, before Shaping. May transform
//!   `token.result` (the content the model sees) and attach Artifacts.
//!
//! Failure isolation is fail-open (ADR-0007): a crashing stage is skipped and
//! reported as an info line in the Transcript; the model never sees it and the
//! Run never fails because of it. That isolation lives in the pipeline
//! (`catch_unwind`), not here.

pub mod token;

use serde_json::Value;

pub use token::{Token, TokenResult};

/// The execution-path contract a Suspenders Extension implements (ADR-0042).
/// Both stages default to identity, so a Middleware overrides only the stages
/// it cares about.
///
/// `opts` is the extension's registration options, carried as a
/// `serde_json::Value` - the open edge for per-extension config.
pub trait Middleware: Send + Sync {
    /// Runs before the Tool executes. Default: identity.
    fn pre_run(&self, token: Token, _opts: &Value) -> Token {
        token
    }

    /// Runs after execution, before Shaping. Default: identity.
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        token
    }
}
