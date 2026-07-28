//! The port to the Scout effect: the interface the explore Tool calls through,
//! kept separate from the Scout implementation ([`crate::scout`]) so it can be
//! a dependency leaf.
//!
//! [`ToolCtx`] carries a [`ScoutFn`] capture and the explore Tool reads back a
//! [`ScoutOutcome`]; both the Tool side and the Scout side depend UP onto this
//! leaf, which lets `tool` stay free of Scout vocabulary and `scout` depend on
//! the abstraction rather than the reverse (ADR-0011, ADR-0013: the Scout is an
//! effect that lives outside the Run loop).
//!
//! [`ToolCtx`]: crate::tool::ToolCtx

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The result of one Scout run.
///
/// A clean report is `Ok`. Every failure mode carries whatever partial
/// findings text was gathered (possibly empty), which the explore Tool rides
/// after a Voice-owned marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoutOutcome {
    /// A clean findings report.
    Ok(String),
    /// The Scout's own model call failed; `partial` is what streamed before.
    LlmError { partial: String },
    /// The Scout stopped without reporting anything usable.
    Empty { partial: String },
    /// The Scout hit its hard Pass cap before reporting.
    PassCap { limit: u64, partial: String },
}

/// The `scout` capture on the Tool ctx: an effect wired to the Session that
/// dispatches a Scout for a `task` and yields its [`ScoutOutcome`]. Boxed and
/// pinned so it is object-safe and `Send`, mirroring the async run path.
pub type ScoutFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ScoutOutcome> + Send>> + Send + Sync>;
