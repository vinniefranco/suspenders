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
    let current = ctx.agent.active_model().await;
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
    let current = ctx.agent.active_model().await;
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
        return screen.info(format!("model → {scoped} (not applied: {reason})"));
    }
    let persist = SessionConfig::persist_model(&ctx.config_path, &scoped);
    let env_shadowed = std::env::var("SUSPENDERS_MODEL").is_ok();
    screen.info(applied_line(&scoped, env_shadowed, &persist))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- applied_line (the three message branches) -------------------------

    #[test]
    fn a_persist_error_is_surfaced_and_the_swap_still_stands() {
        let err = Err(SessionError("disk full".into()));
        assert_eq!(
            applied_line("local/qwen/y", false, &err),
            "model → local/qwen/y (not saved: disk full)"
        );
        // The env-shadow branch never masks a persist error: the error wins.
        assert_eq!(
            applied_line("local/qwen/y", true, &err),
            "model → local/qwen/y (not saved: disk full)"
        );
    }

    #[test]
    fn a_shadowing_env_warns_the_sticky_write_will_be_overridden() {
        assert_eq!(
            applied_line("local/qwen/y", true, &Ok(())),
            "model → local/qwen/y (SUSPENDERS_MODEL is set and will override this next launch)"
        );
    }

    #[test]
    fn a_clean_persist_is_just_the_bare_line() {
        assert_eq!(
            applied_line("local/qwen/y", false, &Ok(())),
            "model → local/qwen/y"
        );
    }

    // --- model_rows (the multi-Provider selector rows, ADR-0037) ------------

    use crate::view_model::RowRole;

    fn listing(provider: &str, models: &[&str]) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
            availability: Availability::Available,
        }
    }

    #[test]
    fn rows_are_scoped_ids_under_a_header_per_provider_in_listing_order() {
        let rows = model_rows(
            &[
                listing("local", &["qwen/Qwen3.6-27B-MTP-GGUF"]),
                listing("anthropic", &["claude-fable-5", "claude-haiku-4-5"]),
            ],
            "local/qwen/Qwen3.6-27B-MTP-GGUF",
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "local",
                "local/qwen/Qwen3.6-27B-MTP-GGUF",
                "anthropic",
                "anthropic/claude-fable-5",
                "anthropic/claude-haiku-4-5",
            ]
        );
        // Headers are unpickable; model rows are pickable and their values
        // ARE the scoped ids - a pick needs no re-scoping.
        let pickable: Vec<bool> = rows.iter().map(|r| r.pickable()).collect();
        assert_eq!(pickable, vec![false, true, false, true, true]);
        assert!(
            rows.iter()
                .filter(|r| r.pickable())
                .all(|r| r.value == r.label)
        );
    }

    #[test]
    fn the_active_model_is_marked_current_by_its_scoped_id() {
        let rows = model_rows(
            &[
                listing("local", &["m"]),
                // The same bare id at ANOTHER Provider must not be marked.
                listing("other", &["m"]),
            ],
            "local/m",
        );
        assert_eq!(rows[1].hint.as_deref(), Some("(current)"));
        assert_eq!(rows[3].hint, None);
    }

    #[test]
    fn an_unavailable_provider_shows_a_note_under_its_header() {
        // A down custom host no longer vanishes: its header stays, with a
        // note whose hint is derived from the boundary's availability fact -
        // right under the header, since nothing collapsed needs anchoring.
        let rows = model_rows(
            &[ProviderModels {
                provider: "local".into(),
                models: vec![],
                availability: Availability::Unreachable,
            }],
            "anthropic/claude-fable-5",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "local");
        assert_eq!(rows[0].role, RowRole::Header);
        assert_eq!(rows[1].label, "  unavailable");
        assert_eq!(rows[1].role, RowRole::Note);
        assert_eq!(rows[1].hint.as_deref(), Some("unreachable"));
        assert!(rows.iter().all(|r| !r.pickable()), "nothing is pickable");
    }

    #[test]
    fn an_empty_listing_shows_a_no_models_note() {
        let rows = model_rows(
            &[ProviderModels {
                provider: "local".into(),
                models: vec![],
                availability: Availability::NoModels,
            }],
            "anthropic/claude-fable-5",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].role, RowRole::Note);
        assert_eq!(rows[1].hint.as_deref(), Some("no models"));
    }

    #[test]
    fn a_credential_less_builtin_lists_its_collapsed_catalog_then_the_note() {
        // A built-in whose environment key is unset appears greyed out: its
        // header, one collapsed row per Catalog model - scoped like a
        // pickable row, so a model-name filter reveals it - and LAST the
        // note whose hint names the key to export: the trailing note is the
        // cursor stop that anchors the popup window below the capped reveal.
        // No row of it is pickable.
        let rows = model_rows(
            &[ProviderModels {
                provider: "openrouter".into(),
                models: vec![],
                availability: Availability::MissingCredential {
                    env: vec!["OPENROUTER_API_KEY".into()],
                    catalog: vec!["qwen3.5-27b".into(), "kimi-k2".into()],
                },
            }],
            "anthropic/claude-fable-5",
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "openrouter");
        assert_eq!(rows[0].role, RowRole::Header);
        assert_eq!(rows[1].label, "openrouter/qwen3.5-27b");
        assert_eq!(rows[1].role, RowRole::Collapsed);
        assert_eq!(rows[2].label, "openrouter/kimi-k2");
        assert_eq!(rows[2].role, RowRole::Collapsed);
        assert_eq!(rows[3].label, "  unavailable");
        assert_eq!(rows[3].role, RowRole::Note);
        assert_eq!(rows[3].hint.as_deref(), Some("set OPENROUTER_API_KEY"));
        assert!(rows.iter().all(|r| !r.pickable()), "nothing is pickable");
    }

    #[test]
    fn two_env_keys_read_as_either_or_in_the_hint() {
        let rows = model_rows(
            &[ProviderModels {
                provider: "vertex".into(),
                models: vec![],
                availability: Availability::MissingCredential {
                    env: vec!["A_KEY".into(), "B_KEY".into()],
                    catalog: vec![],
                },
            }],
            "anthropic/claude-fable-5",
        );
        assert_eq!(rows[1].hint.as_deref(), Some("set A_KEY or B_KEY"));
    }

    // --- pick (the pure pick policy) ----------------------------------------

    #[test]
    fn a_pick_is_the_scoped_row_value_itself() {
        assert_eq!(
            pick("local/old-model", "anthropic/claude-fable-5".into()),
            Some("anthropic/claude-fable-5".to_string())
        );
    }

    #[test]
    fn re_selecting_the_current_model_is_no_pick() {
        assert_eq!(
            pick(
                "local/qwen/Qwen3.6-27B-MTP-GGUF",
                "local/qwen/Qwen3.6-27B-MTP-GGUF".into()
            ),
            None
        );
    }
}
