//! The `/model` Slash Command - its POLICY pure and tested, its I/O in the thin
//! adapter (ADR-0033, ADR-0037, ADR-0001's pure-core/adapter split).
//!
//! `/model` lists every Provider's models grouped by Provider - custom
//! Providers by live discovery, built-ins from the Catalog, a credential-less
//! built-in greyed out with the key to set and its Catalog collapsed until a
//! filter matches - lets the user pick one scoped id, swaps the Active Model
//! live this Session, and persists the choice sticky for the next launch.
//! The DECISIONS - rows are scoped ids grouped by Provider,
//! re-selecting the current model is a no-op, an `SUSPENDERS_MODEL` env value
//! shadows the sticky write, a persist failure is surfaced but the live swap
//! still stands - live in the pure parts ([`model_rows`], [`pick`],
//! [`applied_line`]); the impure orchestration ([`run`], [`choose`]) does the
//! env read, the `agent.set_model`/`persist_model` I/O, and the off-loop
//! network fetch. A pick `set_model` rejects (unresolvable, or over the budget
//! check) surfaces as an info line, state unchanged. The generic command
//! router that routes a committed command to this module lives in
//! [`super::command`].

use crate::event::Event;
use crate::llm::{Availability, ProviderModels};
use crate::session::{SessionConfig, SessionError};
use crate::ui::AdapterCtx;
use crate::view_model::SelectorRow;

use super::screen::Screen;

/// The command's registered name - what [`super::command::handled`] routes
/// here, minted once beside the module that owns the command.
pub(crate) const NAME: &str = "model";

/// The Transcript info line an applied `/model` pick emits (ADR-0033). Pure
/// message construction over the two facts the impure orchestration gathered -
/// whether `SUSPENDERS_MODEL` shadows the sticky file, and how the persist
/// went - so the "env-shadow warning" and "persist-failure note" rules are
/// unit-testable without touching the filesystem or the environment:
/// - a persist error surfaces (the live swap still stands): `model → {chosen}
///   (not saved: {e})`
/// - persisted, but the env shadows it: `model → {chosen} (SUSPENDERS_MODEL is
///   set and will override this next launch)`
/// - persisted cleanly: `model → {chosen}`
pub fn applied_line(
    chosen: &str,
    env_shadowed: bool,
    persist: &Result<(), SessionError>,
) -> String {
    match persist {
        Err(e) => format!("model → {chosen} (not saved: {e})"),
        Ok(()) if env_shadowed => {
            format!("model → {chosen} (SUSPENDERS_MODEL is set and will override this next launch)")
        }
        Ok(()) => format!("model → {chosen}"),
    }
}

/// The `listings -> rows` builder for `/model`'s selector (ADR-0037): each
/// Provider's models sit under a header row naming the Provider, one
/// pickable row per model - its value AND label the scoped
/// `provider/model-id` - grouped in the listing's order (the Session's set
/// order), the currently-Active Model marked "(current)". A Provider whose
/// [`Availability`] is not `Available` shows an "unavailable" note instead
/// of vanishing - the display string derived HERE from the boundary's fact.
/// A missing-credential built-in additionally gets one collapsed row per
/// Catalog model, scoped like any pickable row, so filtering for a model
/// name reveals the greyed row - and ITS note trails the collapsed rows,
/// because the popup window ends at the highlight: the note is the cursor
/// stop that pulls the whole capped reveal into view. Unreachable and
/// no-models notes sit right under their headers (nothing collapsed to
/// anchor). Scoping happens here - the boundary lists bare ids per
/// Provider - so a pick is a ready-to-apply scoped id, and the Selector's
/// own role rules keep headers, notes, and collapsed rows unpickable.
fn model_rows(listings: &[ProviderModels], current: &str) -> Vec<SelectorRow> {
    let mut rows = Vec::new();
    for listing in listings {
        rows.push(SelectorRow::header(&listing.provider));
        match &listing.availability {
            Availability::Available => {}
            Availability::Unreachable => {
                rows.push(SelectorRow::note(
                    "  unavailable",
                    Some("unreachable".to_string()),
                ));
            }
            Availability::NoModels => {
                rows.push(SelectorRow::note(
                    "  unavailable",
                    Some("no models".to_string()),
                ));
            }
            Availability::MissingCredential { env, catalog } => {
                for id in catalog {
                    rows.push(SelectorRow::collapsed(format!("{}/{id}", listing.provider)));
                }
                rows.push(SelectorRow::note(
                    "  unavailable",
                    Some(format!("set {}", env.join(" or "))),
                ));
            }
        }
        for id in &listing.models {
            let scoped = format!("{}/{id}", listing.provider);
            let hint = (scoped == current).then(|| "(current)".to_string());
            rows.push(SelectorRow::new(scoped.clone(), scoped, hint));
        }
    }
    rows
}

/// Populates `/model`'s selector (ADR-0033, ADR-0037). ALWAYS a live fetch:
/// the Agent's `list_models` walks the Session's whole Provider set - live
/// `GET /models` discovery per custom Provider, the Catalog for built-ins.
/// The fetch spawns a task that awaits it OFF the select loop
/// (ADR-0011) - on success it posts SelectorReady, on failure SelectorFailed -
/// through `ctx.selector_tx`; the injected event arrives at the loop's
/// `selector_rx` arm and flips the Loading overlay. The overlay stays Loading
/// until it arrives. `generation` is the activation counter the committing
/// `Effect::Command` carried: both fill events echo it, so the core only fills
/// the overlay that asked.
pub(super) async fn run(screen: Screen, ctx: &AdapterCtx<'_>, generation: u64) -> Screen {
    // A dead Agent yields None; fall back to no current model (nothing marked
    // "(current)" in the selector) rather than panicking the loop.
    let current = ctx.agent.active_model().await.unwrap_or_default();
    let agent = ctx.agent.clone();
    let tx = ctx.selector_tx.clone();
    tokio::spawn(async move {
        let event = match agent.list_models().await {
            Ok(listings) => Event::selector_ready(generation, model_rows(&listings, &current)),
            Err(reason) => Event::selector_failed(generation, reason),
        };
        let _ = tx.send(event);
    });
    screen
}

/// Interprets a `/model` pick (ADR-0033). Re-selecting the current model is a
/// no-op - return the Screen untouched, no side effects. Otherwise this
/// impure part does the I/O - swap the Active Model live, persist it by the
/// sparse config write, read `SUSPENDERS_MODEL` - then hands those facts to the
/// pure [`applied_line`] for the info line. A persist failure is surfaced but
/// the live swap still stands.
pub(super) async fn choose(screen: Screen, ctx: &AdapterCtx<'_>, value: String) -> Screen {
    let current = ctx.agent.active_model().await.unwrap_or_default();
    apply_pick(screen, ctx, pick(&current, value)).await
}

/// The pure pick policy (ADR-0033, ADR-0037): rows carry ready-scoped ids, so
/// a pick is the value itself - and a re-selection of the current model is no
/// pick at all (`None`): no swap, no write, no warning.
fn pick(current: &str, value: String) -> Option<String> {
    (value != current).then_some(value)
}

// The impure application of a pick: swap the Active Model live, persist the
// choice sticky (the scoped id, by the ADR-0033 sparse write), and describe
// what happened. No pick (a re-selection of the current model) changes
// nothing. A rejected swap - an unresolvable id, or a Model whose budget
// check fails (ADR-0037) - leaves the Active Model as-is; nothing persists
// and the reason surfaces as the info line.
async fn apply_pick(screen: Screen, ctx: &AdapterCtx<'_>, pick: Option<String>) -> Screen {
    let Some(scoped) = pick else { return screen };
    if let Err(reason) = ctx.agent.set_model(scoped.clone()).await {
        // Info line's Commit re-derived by dispatch's trailing freeze (ADR-0046).
        return screen
            .info(format!("model → {scoped} (not applied: {reason})"))
            .0;
    }
    let persist = SessionConfig::persist_model(&ctx.config_path, &scoped);
    let env_shadowed = std::env::var("SUSPENDERS_MODEL").is_ok();
    screen.info(applied_line(&scoped, env_shadowed, &persist)).0
}

#[cfg(test)]
#[path = "../../tests/unit/ui/model_command.rs"]
mod tests;
