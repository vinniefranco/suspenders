//! The `/model` Slash Command - its POLICY pure and tested, its I/O in the thin
//! adapter (ADR-0033, ADR-0001's pure-core/adapter split).
//!
//! `/model` lists the models the server offers, lets the user pick one, swaps
//! the Active Model live this Session, and persists the choice sticky for the
//! next launch. The DECISIONS - re-selecting the current model is a no-op, an
//! `SUSPENDERS_MODEL` env value shadows the sticky write, a persist failure is
//! surfaced but the live swap still stands - sit with the impure orchestration
//! ([`run`], [`choose`]) that does the env read, the `agent.set_model`/
//! `persist_model` I/O, and the off-loop network fetch; the one pure part
//! ([`applied_line`]) only formats. The generic command router that routes a
//! committed command to this module lives in [`super::command`].

use crate::event::Event;
use crate::llm::model::split_scoped;
use crate::session::{SessionConfig, SessionError};
use crate::ui::AdapterCtx;
use crate::ui::selector::SelectorRow;

use super::screen::Screen;

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

/// One `ids -> rows` builder shared by every path that populates `/model`'s
/// selector (ADR-0033): server order preserved, the currently-Active Model
/// marked "(current)". The server lists bare model ids while the Active Model
/// is scoped (ADR-0037), so the mark compares against the id part.
fn model_rows(ids: Vec<String>, current: &str) -> Vec<SelectorRow> {
    let current_id = split_scoped(current).map(|(_, id)| id).unwrap_or(current);
    ids.into_iter()
        .map(|id| {
            let hint = (id == current_id).then(|| "(current)".to_string());
            SelectorRow::new(id.clone(), id, hint)
        })
        .collect()
}

/// Populates `/model`'s selector (ADR-0033). ALWAYS a live fetch (a localhost
/// call): the endpoint is the Session's fixed `base_url`, a fixed fact, so there
/// is nothing to cache-invalidate and a live list is what a server whose model
/// set changed would show. The fetch spawns a task that awaits
/// `agent.list_models()` OFF the select loop (ADR-0011) - on success it posts
/// SelectorReady, on failure SelectorFailed - through `ctx.selector_tx`; the
/// injected event arrives at the loop's `selector_rx` arm and flips the Loading
/// overlay. The overlay stays Loading until it arrives. `generation` is the
/// activation counter the committing `Effect::Command` carried: both fill
/// events echo it, so the core only fills the overlay that asked.
pub(super) async fn run(screen: Screen, ctx: &AdapterCtx<'_>, generation: u64) -> Screen {
    let current = ctx.agent.active_model().await;
    let agent = ctx.agent.clone();
    let tx = ctx.selector_tx.clone();
    tokio::spawn(async move {
        let event = match agent.list_models().await {
            Ok(ids) => Event::selector_ready(generation, model_rows(ids, &current)),
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
    apply_pick(screen, ctx, scoped_pick(&current, value)).await
}

/// The pure pick policy (ADR-0033, ADR-0037): the selector lists the Active
/// Model's Provider's bare ids (the full multi-provider selector is a later
/// stage), so a pick is scoped to that Provider - and a re-selection of the
/// current model is no pick at all (`None`). An unscoped `current` cannot
/// happen (the Active Model is always scoped); the pick then passes through
/// bare for `set_model` to reject with a real reason.
fn scoped_pick(current: &str, value: String) -> Option<String> {
    let scoped = match split_scoped(current) {
        Ok((provider, _)) => format!("{provider}/{value}"),
        Err(_) => value,
    };
    (scoped != current).then_some(scoped)
}

// The impure application of a pick: swap the Active Model live, persist the
// choice sticky, and describe what happened. No pick (a re-selection of the
// current model) changes nothing - no swap, no write, no warning (ADR-0033).
// A rejected swap (an unresolvable id) leaves the Active Model as-is; nothing
// persists and the reason surfaces as the info line.
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
            applied_line("qwen/y", false, &err),
            "model → qwen/y (not saved: disk full)"
        );
        // The env-shadow branch never masks a persist error: the error wins.
        assert_eq!(
            applied_line("qwen/y", true, &err),
            "model → qwen/y (not saved: disk full)"
        );
    }

    #[test]
    fn a_shadowing_env_warns_the_sticky_write_will_be_overridden() {
        assert_eq!(
            applied_line("qwen/y", true, &Ok(())),
            "model → qwen/y (SUSPENDERS_MODEL is set and will override this next launch)"
        );
    }

    #[test]
    fn a_clean_persist_is_just_the_bare_line() {
        assert_eq!(applied_line("qwen/y", false, &Ok(())), "model → qwen/y");
    }

    // --- scoped_pick (the pure pick policy) ---------------------------------

    #[test]
    fn a_pick_is_scoped_to_the_active_models_provider() {
        assert_eq!(
            scoped_pick("local/old-model", "new-model".into()),
            Some("local/new-model".to_string())
        );
        // Bare ids may themselves contain slashes; the scope is the Provider
        // part of the CURRENT scoped id, split on its first slash only.
        assert_eq!(
            scoped_pick("local/qwen/Qwen3.6-27B-MTP-GGUF", "qwen/other".into()),
            Some("local/qwen/other".to_string())
        );
    }

    #[test]
    fn re_selecting_the_current_model_is_no_pick() {
        assert_eq!(
            scoped_pick(
                "local/qwen/Qwen3.6-27B-MTP-GGUF",
                "qwen/Qwen3.6-27B-MTP-GGUF".into()
            ),
            None
        );
    }

    #[test]
    fn an_unscoped_current_passes_the_pick_through_bare() {
        // Cannot happen (the Active Model is always scoped); the bare pick
        // then reaches set_model, which rejects it with a real reason.
        assert_eq!(scoped_pick("", "m".into()), Some("m".to_string()));
    }
}
