//! The adapter-side Slash Command router - the SINGLE seam a committed command
//! name crosses to reach its adapter work (ADR-0032/0033). The pure core emits
//! a command-agnostic [`Effect::Command`](crate::ui::transcript::Effect::Command)
//! (and [`SelectorChosen`](crate::ui::transcript::Effect::SelectorChosen)); this
//! module classifies the opaque name and routes it to the owning module (today
//! only [`super::model_command`]).
//!
//! ## Drift is impossible by construction
//!
//! [`handled`] is the ONE place a command-name string literal lives on the
//! adapter side; [`is_handled`] is derived from it, so the two cannot disagree.
//! [`run`] and [`choose`] match [`Handled`] EXHAUSTIVELY (no `_` arm), so adding
//! a `Handled::Theme` variant is a COMPILE error until both are handled. The
//! colocated coverage test drives the real classifier over every
//! [`slash::COMMANDS`](crate::ui::slash::COMMANDS) entry, so a registered
//! command without a `handled` mapping fails the test rather than silently
//! becoming a no-op-with-info-line at runtime.

use crate::ui::AdapterCtx;

use super::model_command;
use super::transcript::Transcript;

/// The Slash Commands the adapter knows how to run (today: just [`Handled::Model`]).
/// Not named `Command` - that collides with [`crate::agent::Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    /// `/model` - [`super::model_command`].
    Model,
}

/// The SINGLE name→command mapping. The only place command-name string literals
/// live on the adapter side, so [`is_handled`], [`run`], and [`choose`] cannot
/// disagree about what `"model"` means.
fn handled(name: &str) -> Option<Handled> {
    match name {
        "model" => Some(Handled::Model),
        _ => None,
    }
}

/// Whether the adapter has a handler for `name`. Derived from [`handled`], so it
/// can never drift past the router.
pub fn is_handled(name: &str) -> bool {
    handled(name).is_some()
}

/// Routes a committed Slash Command to its adapter work (ADR-0032/0033). An
/// unrecognized name is a visible no-op-with-info-line, not a silent drop. The
/// [`Handled`] match is exhaustive, so a new command is a compile error here
/// until it is handled.
pub(super) async fn run(transcript: Transcript, ctx: &AdapterCtx<'_>, name: &str) -> Transcript {
    match handled(name) {
        Some(Handled::Model) => model_command::run(transcript, ctx).await,
        None => transcript.info(format!("/{name}: no handler")),
    }
}

/// Routes a chosen selector row back to the command that opened the selector
/// (ADR-0033). Mirrors [`run`]'s exhaustiveness and unrecognized-name coverage:
/// an unknown command is a visible info line, never a dropped selection.
pub(super) async fn choose(
    transcript: Transcript,
    ctx: &AdapterCtx<'_>,
    command: &str,
    value: String,
) -> Transcript {
    match handled(command) {
        Some(Handled::Model) => model_command::choose(transcript, ctx, value).await,
        None => transcript.info(format!("/{command}: no handler")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::slash;

    #[test]
    fn model_is_handled_and_an_unknown_name_is_not() {
        assert!(is_handled("model"));
        assert!(!is_handled("theme"));
        assert!(!is_handled(""));
    }

    // Adding a COMMANDS entry without a `handled` mapping would otherwise fail
    // silently (ADR-0032's extension seam): assert every registered command is
    // handled. This drives the real classifier - the same one `run`/`choose`
    // match exhaustively - so a registry entry cannot outrun its adapter arm.
    #[test]
    fn every_registry_command_is_handled() {
        for c in slash::COMMANDS {
            assert!(is_handled(c.name), "unhandled command: {}", c.name);
        }
    }
}
