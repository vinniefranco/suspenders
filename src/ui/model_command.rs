//! The `/model` Slash Command — its POLICY pure and tested, its I/O in the thin
//! adapter (ADR-0033, ADR-0001's pure-core/adapter split).
//!
//! `/model` lists the models the server offers, lets the user pick one, swaps
//! the Active Model live this Session, and persists the choice sticky for the
//! next launch. The DECISIONS — re-selecting the current model is a no-op, an
//! `SUSPENDERS_MODEL` env value shadows the sticky write, a persist failure is
//! surfaced but the live swap still stands — sit with the impure orchestration
//! ([`run`], [`choose`]) that does the env read, the `agent.set_model`/
//! `persist_model` I/O, and the off-loop network fetch; the one pure part
//! ([`applied_line`]) only formats. The generic command router that routes a
//! committed command to this module lives in [`super::command`].

use crate::event::Event;
use crate::session::{SessionConfig, SessionError};
use crate::ui::AdapterCtx;
use crate::ui::selector::SelectorRow;

use super::transcript::Transcript;

/// The Transcript info line an applied `/model` pick emits (ADR-0033). Pure
/// message construction over the two facts the impure orchestration gathered —
/// whether `SUSPENDERS_MODEL` shadows the sticky file, and how the persist
/// went — so the "env-shadow warning" and "persist-failure note" rules are
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
/// marked "(current)".
fn model_rows(ids: Vec<String>, current: &str) -> Vec<SelectorRow> {
    ids.into_iter()
        .map(|id| {
            let hint = (id == current).then(|| "(current)".to_string());
            SelectorRow::new(id.clone(), id, hint)
        })
        .collect()
}

/// Populates `/model`'s selector (ADR-0033). ALWAYS a live fetch (a localhost
/// call): the endpoint is the Session's fixed `base_url`, a fixed fact, so there
/// is nothing to cache-invalidate and a live list is what a server whose model
/// set changed would show. The fetch spawns a task that awaits
/// `agent.list_models()` OFF the select loop (ADR-0011) — on success it posts
/// SelectorReady, on failure SelectorFailed — through `ctx.selector_tx`; the
/// injected event arrives at the loop's `selector_rx` arm and flips the Loading
/// overlay. The overlay stays Loading until it arrives.
pub(super) async fn run(transcript: Transcript, ctx: &AdapterCtx<'_>) -> Transcript {
    let current = ctx.agent.active_model().await;
    let agent = ctx.agent.clone();
    let tx = ctx.selector_tx.clone();
    tokio::spawn(async move {
        let event = match agent.list_models().await {
            Ok(ids) => Event::selector_ready(model_rows(ids, &current)),
            Err(reason) => Event::selector_failed(reason),
        };
        let _ = tx.send(event);
    });
    transcript
}

/// Interprets a `/model` pick (ADR-0033). Re-selecting the current model is a
/// no-op — return the Transcript untouched, no side effects. Otherwise this
/// impure part does the I/O — swap the Active Model live, persist it by the
/// sparse config write, read `SUSPENDERS_MODEL` — then hands those facts to the
/// pure [`applied_line`] for the info line. A persist failure is surfaced but
/// the live swap still stands.
pub(super) async fn choose(
    transcript: Transcript,
    ctx: &AdapterCtx<'_>,
    value: String,
) -> Transcript {
    // Re-selecting the current model changes nothing (no swap, no write, no
    // warning — ADR-0033).
    if value == ctx.agent.active_model().await {
        return transcript;
    }
    ctx.agent.set_model(value.clone()).await;
    let persist = SessionConfig::persist_model(&ctx.config_path, &value);
    let env_shadowed = std::env::var("SUSPENDERS_MODEL").is_ok();
    transcript.info(applied_line(&value, env_shadowed, &persist))
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
}
