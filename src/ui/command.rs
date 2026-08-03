//! The adapter-side Slash Command router - the SINGLE seam a committed command
//! name crosses to reach its adapter work (ADR-0032/0033). The pure core emits
//! a command-agnostic [`Effect::Command`](crate::ui::screen::Effect::Command)
//! (and [`SelectorChosen`](crate::ui::screen::Effect::SelectorChosen)); this
//! module classifies the opaque name and routes it to the owning module
//! ([`super::model_command`], [`super::theme_command`]).
//!
//! ## Drift is impossible by construction
//!
//! Each command module mints its name ONCE (`model_command::NAME`,
//! `theme_command::NAME`); [`handled`] is the ONE place those names are
//! classified on the adapter side, and [`is_handled`] is derived from it, so
//! the router and any other reader of a command's name (the `/theme` live
//! preview) cannot disagree. [`run`] and [`choose`] match [`Handled`]
//! EXHAUSTIVELY (no `_` arm), so adding a variant is a COMPILE error until
//! both are handled. The colocated coverage test drives the real classifier
//! over every [`slash::COMMANDS`](crate::ui::slash::COMMANDS) entry, so a
//! registered command without a `handled` mapping fails the test rather than
//! silently becoming a no-op-with-info-line at runtime.

use crate::ui::{AdapterCtx, AdapterState};

use super::mcp_command;
use super::model_command;
use super::plan_command;
use super::screen::Screen;
use super::theme_command;

/// The Slash Commands the adapter knows how to run.
/// Not named `Command` - that collides with [`crate::agent::Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// `/model` - [`super::model_command`].
    Model,
    /// `/theme` - [`super::theme_command`].
    Theme,
    /// `/mcp` - [`super::mcp_command`] (opens the McpDialog overlay).
    Mcp,
    /// `/plan` - [`super::plan_command`] (enter/exit Plan mode; may submit a
    /// trailing prompt).
    Plan,
}

/// A committed Slash Command's payload - the three fields that always travel
/// together from the pure core's
/// [`Effect::Command`](crate::ui::screen::Effect::Command) to the router (the command
/// `name`, the raw draft remainder `rest`, and the activation `generation`).
/// Bundled as one Parameter Object so the router and the effect handler take a
/// single argument rather than threading three parallel scalars (ADR-0032's
/// command-agnostic seam - the router still classifies `name`, this only groups
/// the carried facts). `mcp`'s internal re-entry ([`for_mcp`]) mints one with no
/// `rest`.
#[derive(Debug, Clone)]
pub(super) struct Committed {
    pub name: String,
    pub rest: Option<String>,
    pub generation: u64,
}

impl Committed {
    /// The `/mcp` internal re-entry payload (ADR-0065 Phase E): the composer
    /// already opened the McpDialog, and the `McpCommand` effect re-enters the
    /// router to kick the fetch. `/mcp` takes no arg, so `rest` is `None`.
    pub(super) fn for_mcp(generation: u64) -> Self {
        Committed {
            name: mcp_command::NAME.to_string(),
            rest: None,
            generation,
        }
    }
}

/// The SINGLE name→command mapping, over each module's own minted `NAME`, so
/// [`is_handled`], [`run`], [`choose`], and every other reader of a command's
/// name resolve the same string.
fn handled(name: &str) -> Option<Handled> {
    match name {
        n if n == model_command::NAME => Some(Handled::Model),
        n if n == theme_command::NAME => Some(Handled::Theme),
        n if n == mcp_command::NAME => Some(Handled::Mcp),
        n if n == plan_command::NAME => Some(Handled::Plan),
        _ => None,
    }
}

/// Whether the adapter has a handler for `name`. Derived from [`handled`], so it
/// can never drift past the router.
#[cfg(test)]
pub fn is_handled(name: &str) -> bool {
    handled(name).is_some()
}

/// Routes a committed Slash Command to its adapter work (ADR-0032/0033). An
/// unrecognized name is a visible no-op-with-info-line, not a silent drop. The
/// [`Handled`] match is exhaustive, so a new command is a compile error here
/// until it is handled. `cmd` is the committed-command [`Committed`] payload:
/// its `rest` is the raw draft remainder past the command token (the argument
/// text `/plan <prompt>` needs), forwarded command-agnostically from the pure
/// core (ADR-0019), and its `generation` is the activation counter the effect
/// carried (a selector-opening handler must echo it on its fill events). A
/// command that takes no arg ignores `rest`. `state` is the run loop's one
/// mutable adapter-state carrier ([`AdapterState`]); `/theme` reads and swaps
/// its Theme state (ADR-0038).
///
/// RETURNS the Screen plus an OPTIONAL prompt the command asked to submit
/// (`/plan <prompt>`): the caller feeds it through the run loop's submit path.
/// Every other command returns `None` (no submit).
pub(super) async fn run(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
    cmd: Committed,
) -> (Screen, Option<String>) {
    let Committed {
        name,
        rest,
        generation,
    } = cmd;
    match handled(&name) {
        Some(Handled::Model) => (model_command::run(screen, ctx, generation).await, None),
        Some(Handled::Theme) => (
            theme_command::run(screen, ctx, &mut state.themes, generation),
            None,
        ),
        // `/mcp` opens the McpDialog overlay (ADR-0065 Phase E): the Composer
        // already opened it to a Loading state on commit; this kicks the async
        // `mcp_views()` fetch that fills it.
        Some(Handled::Mcp) => (mcp_command::run(screen, ctx, generation).await, None),
        // `/plan` enters/exits Plan mode (ADR-0067) and may return a trailing
        // prompt to submit - the one command that carries `rest`.
        Some(Handled::Plan) => plan_command::run(screen, ctx, rest.as_deref()).await,
        // The info line's Commit is re-derived by dispatch's trailing freeze
        // (ADR-0046), so drop it here - this seam returns only the Screen.
        None => (screen.info(format!("/{name}: no handler")).0, None),
    }
}

/// Routes a chosen selector row back to the command that opened the selector
/// (ADR-0033). Mirrors [`run`]'s exhaustiveness and unrecognized-name coverage:
/// an unknown command is a visible info line, never a dropped selection.
pub(super) async fn choose(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
    command: &str,
    value: String,
) -> Screen {
    match handled(command) {
        Some(Handled::Model) => model_command::choose(screen, ctx, value).await,
        Some(Handled::Theme) => theme_command::choose(screen, ctx, &mut state.themes, value),
        // `/mcp` is not a flat selector (its rows live in the McpDialog overlay,
        // which routes its own actions through `Effect::McpAction`, ADR-0065): a
        // `SelectorChosen` for it can never arise. Leave the Screen untouched.
        Some(Handled::Mcp) => screen,
        // `/plan` is fire-and-run (it never opens a selector), so a
        // `SelectorChosen` for it can never arise. Leave the Screen untouched.
        Some(Handled::Plan) => screen,
        // Info line's Commit re-derived by dispatch's trailing freeze (ADR-0046).
        None => screen.info(format!("/{command}: no handler")).0,
    }
}

#[cfg(test)]
#[path = "../../tests/ui/command.rs"]
mod tests;
