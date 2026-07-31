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

use super::model_command;
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
}

/// The SINGLE name→command mapping, over each module's own minted `NAME`, so
/// [`is_handled`], [`run`], [`choose`], and every other reader of a command's
/// name resolve the same string.
fn handled(name: &str) -> Option<Handled> {
    match name {
        n if n == model_command::NAME => Some(Handled::Model),
        n if n == theme_command::NAME => Some(Handled::Theme),
        _ => None,
    }
}

/// Routes a committed Slash Command to its adapter work (ADR-0032/0033). An
/// unrecognized name is a visible no-op-with-info-line, not a silent drop. The
/// [`Handled`] match is exhaustive, so a new command is a compile error here
/// until it is handled. `generation` is the activation counter the effect
/// carried; a selector-opening handler must echo it on its fill events.
/// `state` is the run loop's one mutable adapter-state carrier
/// ([`AdapterState`]); `/theme` reads and swaps its Theme state (ADR-0038).
pub(super) async fn run(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
    name: &str,
    generation: u64,
) -> Screen {
    match handled(name) {
        Some(Handled::Model) => model_command::run(screen, ctx, generation).await,
        Some(Handled::Theme) => theme_command::run(screen, ctx, &mut state.themes, generation),
        // The info line's Commit is re-derived by dispatch's trailing freeze
        // (ADR-0046), so drop it here - this seam returns only the Screen.
        None => screen.info(format!("/{name}: no handler")).0,
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
        // Info line's Commit re-derived by dispatch's trailing freeze (ADR-0046).
        None => screen.info(format!("/{command}: no handler")).0,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/command.rs"]
mod tests;
