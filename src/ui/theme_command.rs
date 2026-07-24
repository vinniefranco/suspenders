//! The `/theme` Slash Command - its POLICY pure and tested, its I/O in the
//! thin adapter (ADR-0038, ADR-0001's pure-core/adapter split).
//!
//! `/theme` lists the built-ins (`dark`, `light`) then the user themes from a
//! FRESH directory read on every open; a broken user file is listed
//! unselectable with its parse reason (ADR-0038: typos surface instead of
//! silently no-oping). Moving the highlight previews that theme live - the
//! whole screen repaints in it - Enter keeps it and persists a sparse `theme`
//! key, Escape reverts exactly to the theme active before the selector
//! opened. The DECISIONS - row order and the unselectable-with-reason rule
//! ([`theme_rows`]), which highlight previews what ([`preview_name`]),
//! re-selecting the current theme as a no-op ([`pick`]), the env-shadow
//! warning and persist-failure note ([`applied_line`]) - live in the pure
//! parts; the swappable state itself (the active Theme, the previews cache)
//! is the Theme domain's [`ActiveTheme`], and [`run`]/[`choose`] are the
//! impure orchestration over it. The generic router that reaches this module
//! lives in [`super::command`].
//!
//! ## How the live preview works (ADR-0038)
//!
//! The highlight already lives in the pure core (the Composer's selector
//! state), so preview needs no new state machine: every frame the render
//! adapter derives the previewed name ([`preview_name`] over the Composer's
//! Ready-selector highlight) and asks [`ActiveTheme::render_theme`] which
//! Theme to draw with - the highlighted row's resolved Theme while the
//! `/theme` selector is open and Ready, the active Theme otherwise. Escape
//! closes the overlay in the pure core, so the very next frame renders the
//! active Theme again: the exact revert falls out of the derivation. A
//! highlight parked on an unselectable row (a broken theme when the filter
//! leaves nothing pickable) previews nothing - the active Theme keeps
//! rendering, showing exactly what Escape would keep.

use crate::event::Event;
use crate::session::{SessionConfig, SessionError};
use crate::ui::AdapterCtx;
use crate::ui::selector::SelectorRow;
use crate::ui::theme::{self, ActiveTheme, SparseTheme, ThemeError};

use super::screen::Screen;

/// The command's registered name - what [`super::command::handled`] routes
/// here and what [`preview_name`] matches the open selector against, so a
/// registry rename cannot silently kill the live preview.
pub(crate) const NAME: &str = "theme";

/// The Transcript info line an applied `/theme` pick emits, mirroring
/// `/model`'s (ADR-0033/0038). Pure message construction over the two facts
/// the impure orchestration gathered - whether `SUSPENDERS_THEME` shadows the
/// sticky file, and how the persist went:
/// - a persist error surfaces (the live swap still stands): `theme → {chosen}
///   (not saved: {e})`
/// - persisted, but the env shadows it: `theme → {chosen} (SUSPENDERS_THEME
///   is set and will override this next launch)`
/// - persisted cleanly: `theme → {chosen}`
fn applied_line(chosen: &str, env_shadowed: bool, persist: &Result<(), SessionError>) -> String {
    match persist {
        Err(e) => format!("theme → {chosen} (not saved: {e})"),
        Ok(()) if env_shadowed => {
            format!("theme → {chosen} (SUSPENDERS_THEME is set and will override this next launch)")
        }
        Ok(()) => format!("theme → {chosen}"),
    }
}

/// The `discovery -> rows` builder for `/theme`'s selector (ADR-0038):
/// built-ins first (`dark`, `light`), then the user themes in discovery order
/// (sorted by name). A valid theme is a pickable row - value and label its
/// name, the active one marked "(current)" like `/model`. A broken user file
/// stays LISTED but unpickable, its whole-file rejection reason riding as
/// the dimmed hint - a note, not a header, because the theme list has no
/// groups and a broken file must not adopt the rows after it under the
/// Selector's group-aware filtering (notes render muted and take the cursor,
/// but Enter refuses them - the same affordance as `/model`'s unavailable
/// notes).
fn theme_rows(
    discovered: &[(String, Result<SparseTheme, ThemeError>)],
    current: &str,
) -> Vec<SelectorRow> {
    let mut rows: Vec<SelectorRow> = theme::BUILT_INS
        .iter()
        .map(|name| theme_row(name, current))
        .collect();
    for (name, parsed) in discovered {
        rows.push(match parsed {
            Ok(_) => theme_row(name, current),
            Err(reason) => SelectorRow::note(name.clone(), Some(reason.to_string())),
        });
    }
    rows
}

fn theme_row(name: &str, current: &str) -> SelectorRow {
    let hint = (name == current).then(|| "(current)".to_string());
    SelectorRow::new(name, name, hint)
}

/// The pure pick policy, identical to `/model`'s: a re-selection of the
/// current theme is no pick at all (`None`) - no swap, no write, no warning.
fn pick(current: &str, value: String) -> Option<String> {
    (value != current).then_some(value)
}

/// Which theme name the open selector previews, if any (ADR-0038).
/// `highlight` is the Composer's Ready-selector highlight
/// ([`crate::ui::composer::Composer::selector_highlight`]): the command that
/// opened the selector and the row under the cursor. Only a PICKABLE row of
/// THIS command previews - a broken theme's note is a cursor stop the
/// highlight can rest on, but it previews nothing, as does any other
/// command's selector (`None` covers the menu, Loading/Failed, and no
/// overlay at all).
pub(crate) fn preview_name<'a>(highlight: Option<(&'a str, &'a SelectorRow)>) -> Option<&'a str> {
    match highlight {
        Some((command, row)) if command == NAME && row.pickable() => Some(row.value.as_str()),
        _ => None,
    }
}

/// Populates `/theme`'s selector (ADR-0038): a fresh directory read via
/// [`ActiveTheme::open`], its discovery shaped by the pure [`theme_rows`] and
/// posted as a SelectorReady through the same injection channel as `/model`'s
/// fetch - no spawned task, because the read is local and instant, but the
/// same event flow, so the pure core's generation guard and Loading overlay
/// behave identically.
pub(super) fn run(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    themes: &mut ActiveTheme,
    generation: u64,
) -> Screen {
    let discovered = themes.open();
    let rows = theme_rows(&discovered, themes.active_name());
    let _ = ctx
        .selector_tx
        .send(Event::selector_ready(generation, rows));
    screen
}

/// Interprets a `/theme` pick (ADR-0038). Re-selecting the current theme is a
/// no-op. Otherwise this impure part does the I/O - swap the active Theme by
/// a FRESH load ([`ActiveTheme::apply`]; a file broken since the open refuses
/// with its reason, nothing swapped or persisted), persist the name by the
/// sparse config write, read `SUSPENDERS_THEME` - then hands those facts to
/// the pure [`applied_line`]. A persist failure is surfaced but the live swap
/// still stands, exactly like `/model`.
pub(super) fn choose(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    themes: &mut ActiveTheme,
    value: String,
) -> Screen {
    let Some(name) = pick(themes.active_name(), value) else {
        return screen;
    };
    if let Err(reason) = themes.apply(&name) {
        return screen.info(format!("theme → {name} (not applied: {reason})"));
    }
    let persist = SessionConfig::persist_theme(&ctx.config_path, &name);
    let env_shadowed = std::env::var("SUSPENDERS_THEME").is_ok();
    screen.info(applied_line(&name, env_shadowed, &persist))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- applied_line (the three message branches, mirroring /model) --------

    #[test]
    fn a_persist_error_is_surfaced_and_the_swap_still_stands() {
        let err = Err(SessionError("disk full".into()));
        assert_eq!(
            applied_line("gruvbox", false, &err),
            "theme → gruvbox (not saved: disk full)"
        );
        // The env-shadow branch never masks a persist error: the error wins.
        assert_eq!(
            applied_line("gruvbox", true, &err),
            "theme → gruvbox (not saved: disk full)"
        );
    }

    #[test]
    fn a_shadowing_env_warns_the_sticky_write_will_be_overridden() {
        assert_eq!(
            applied_line("light", true, &Ok(())),
            "theme → light (SUSPENDERS_THEME is set and will override this next launch)"
        );
    }

    #[test]
    fn a_clean_persist_is_just_the_bare_line() {
        assert_eq!(applied_line("light", false, &Ok(())), "theme → light");
    }

    // --- theme_rows (order, current-marking, broken-row listing) -------------

    fn valid(name: &str) -> (String, Result<SparseTheme, ThemeError>) {
        (name.to_string(), Ok(SparseTheme::default()))
    }

    fn broken(name: &str, reason: &str) -> (String, Result<SparseTheme, ThemeError>) {
        (
            name.to_string(),
            Err(ThemeError::Invalid(reason.to_string())),
        )
    }

    #[test]
    fn rows_list_built_ins_first_then_user_themes_in_discovery_order() {
        let rows = theme_rows(&[valid("aurora"), valid("zebra")], "dark");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["dark", "light", "aurora", "zebra"]);
        assert!(rows.iter().all(|r| r.pickable()));
        assert!(
            rows.iter()
                .filter(|r| r.pickable())
                .all(|r| r.value == r.label),
            "a pick is the theme name itself"
        );
    }

    #[test]
    fn the_active_theme_is_marked_current() {
        let rows = theme_rows(&[valid("aurora")], "aurora");
        assert_eq!(rows[0].hint, None, "dark is not current here");
        assert_eq!(rows[1].hint, None);
        assert_eq!(rows[2].hint.as_deref(), Some("(current)"));
    }

    #[test]
    fn a_broken_theme_is_listed_unpickable_with_its_reason() {
        // ADR-0038: strict per-file, resilient app - the file is refused
        // whole, but /theme shows it with the reason instead of hiding it.
        let rows = theme_rows(
            &[broken("typo", "colors.added: bad"), valid("zebra")],
            "dark",
        );
        assert_eq!(rows[2].label, "typo");
        assert!(!rows[2].pickable(), "Enter can never pick a broken theme");
        assert!(
            rows[2].is_stop(),
            "the cursor may rest on it, so its reason is reachable"
        );
        assert_eq!(rows[2].hint.as_deref(), Some("colors.added: bad"));
        assert_eq!(
            rows[2].role,
            crate::ui::selector::RowRole::Note,
            "a note, not a header: a broken file starts no group, so the \
             valid neighbors never travel with it under filtering"
        );
        assert!(rows[3].pickable(), "the valid neighbor stays pickable");
    }

    // --- pick (the pure pick policy) -----------------------------------------

    #[test]
    fn a_pick_is_the_rows_value_itself() {
        assert_eq!(pick("dark", "light".into()), Some("light".to_string()));
    }

    #[test]
    fn re_selecting_the_current_theme_is_no_pick() {
        assert_eq!(pick("light", "light".into()), None);
    }

    // --- preview_name (what an open selector highlight previews) -------------

    #[test]
    fn the_highlighted_pickable_theme_row_previews() {
        let row = SelectorRow::new("light", "light", None);
        assert_eq!(preview_name(Some((NAME, &row))), Some("light"));
    }

    #[test]
    fn an_unpickable_highlight_previews_nothing() {
        // A broken theme's note is a cursor stop, so the highlight can rest
        // on it; the active Theme keeps rendering (what Escape would keep).
        let row = SelectorRow::note("typo", Some("colors.added: bad".into()));
        assert_eq!(preview_name(Some((NAME, &row))), None);
    }

    #[test]
    fn other_commands_and_no_highlight_preview_nothing() {
        // The menu, Loading/Failed, and no overlay all arrive as None (the
        // Composer only surfaces a Ready selector's highlight).
        assert_eq!(preview_name(None), None);
        // Another command's selector never drives the theme.
        let row = SelectorRow::new("local/m", "local/m", None);
        assert_eq!(preview_name(Some(("model", &row))), None);
    }
}
