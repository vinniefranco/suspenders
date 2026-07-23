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
//! parts; [`ThemeSelection`] is the adapter-owned swappable state (the active
//! Theme plus the previews resolved once per open), and [`run`]/[`choose`]
//! are the impure orchestration. The generic router that reaches this module
//! lives in [`super::command`].
//!
//! ## How the live preview works (ADR-0038)
//!
//! The highlight already lives in the pure core (the Composer's selector
//! state), so preview needs no new state machine: every frame the render
//! adapter asks [`ThemeSelection::render_theme`] which Theme to draw with -
//! the highlighted row's resolved Theme while the `/theme` selector is open
//! and Ready, the active Theme otherwise. Escape closes the overlay in the
//! pure core, so the very next frame renders the active Theme again: the
//! exact revert falls out of the derivation. A highlight parked on an
//! unselectable row (a broken theme when the filter leaves nothing pickable)
//! previews nothing - the active Theme keeps rendering, showing exactly what
//! Escape would keep.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::event::Event;
use crate::session::{SessionConfig, SessionError};
use crate::ui::AdapterCtx;
use crate::ui::composer::{OverlayStatus, OverlayView};
use crate::ui::selector::SelectorRow;
use crate::ui::theme::{self, SparseTheme, Theme, ThemeError};

use super::screen::Screen;

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

/// The launch notice a configured theme that cannot load falls back with
/// (ADR-0038): the name, the reason, and the fact that `dark` took over -
/// phrased like the context-file skip line, never a crash or a launch block.
fn fallback_notice(configured: &str, error: &ThemeError) -> String {
    format!("theme \"{configured}\" could not be used ({error}); using the built-in dark")
}

/// The `discovery -> rows` builder for `/theme`'s selector (ADR-0038):
/// built-ins first (`dark`, `light`), then the user themes in discovery order
/// (sorted by name). A valid theme is a pickable row - value and label its
/// name, the active one marked "(current)" like `/model`. A broken user file
/// stays LISTED but unselectable, its whole-file rejection reason riding as
/// the dimmed hint (the shared Selector's non-selectable rows already render
/// muted, skip the cursor, and refuse Enter - the same affordance as
/// `/model`'s unavailable notes).
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
            Err(reason) => SelectorRow::header(name.clone(), Some(reason.to_string())),
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

/// Which theme name the open overlay previews, if any (ADR-0038): the
/// highlighted row of a READY `/theme` selector, and only when that row is
/// pickable - a broken theme (unselectable) previews nothing, as does any
/// other overlay (the Slash menu, `/model`'s selector, Loading/Failed).
fn preview_name(overlay: Option<&OverlayView>) -> Option<&str> {
    match overlay {
        Some(OverlayView::Selector {
            command,
            status: OverlayStatus::Ready,
            rows,
            highlight,
        }) if command == "theme" => rows
            .get(*highlight)
            .filter(|row| row.selectable)
            .map(|row| row.value.as_str()),
        _ => None,
    }
}

/// The adapter-owned Theme state the run loop draws from (ADR-0038): the
/// active resolved [`Theme`] (swappable - `/theme`'s Enter replaces it), and
/// the previews cache filled once per selector open so a highlight change
/// costs a map lookup, never a re-parse per frame. Ratatui-free like
/// everything under the presentation boundary; the themes directory is
/// captured once at the launch edge, like the config path.
pub struct ThemeSelection {
    active_name: String,
    active: Theme,
    themes_dir: PathBuf,
    /// Every theme the last `open` could resolve, by name: the built-ins plus
    /// each valid user file laid over the `dark` floor. Refilled on every
    /// open (a fresh directory read, like `/model`'s fetch), so previews
    /// track the files on disk as of the open.
    previews: HashMap<String, Theme>,
}

impl ThemeSelection {
    /// Resolves the configured theme name for launch (ADR-0038): the named
    /// Theme when it loads, else the built-in `dark` plus the visible notice
    /// [`fallback_notice`] states - a missing or broken configured theme
    /// never fails launch. The fallback also becomes the ACTIVE name, so
    /// `/theme` marks `dark` "(current)" and re-picking it stays a no-op.
    pub fn launch(configured: &str, themes_dir: PathBuf) -> (Self, Option<String>) {
        let (active_name, active, notice) = match theme::load(configured, &themes_dir) {
            Ok(loaded) => (configured.to_string(), loaded, None),
            Err(e) => (
                "dark".to_string(),
                theme::dark().clone(),
                Some(fallback_notice(configured, &e)),
            ),
        };
        let selection = ThemeSelection {
            active_name,
            active,
            themes_dir,
            previews: HashMap::new(),
        };
        (selection, notice)
    }

    /// The active Theme - what every frame renders with outside a `/theme`
    /// preview, and what Escape reverts to.
    pub fn active(&self) -> &Theme {
        &self.active
    }

    /// One selector open (ADR-0038): a FRESH read of the themes directory
    /// (like `/model` re-fetches), yielding the rows for the overlay and
    /// refilling the previews cache with every theme that resolves.
    fn open(&mut self) -> Vec<SelectorRow> {
        let discovered = theme::discover(&self.themes_dir);
        self.previews = resolve_previews(&discovered, &self.themes_dir);
        theme_rows(&discovered, &self.active_name)
    }

    /// The Theme this frame renders with (ADR-0038's live preview): the
    /// highlighted row's resolved Theme while the `/theme` selector is open
    /// and Ready, the active Theme otherwise - so closing the overlay
    /// (Escape included) IS the revert, with no bookkeeping to unwind.
    pub fn render_theme(&self, overlay: Option<&OverlayView>) -> &Theme {
        preview_name(overlay)
            .and_then(|name| self.previews.get(name))
            .unwrap_or(&self.active)
    }

    // Makes `name` the active Theme: from the open's previews (every
    // pickable row resolved there), else loaded fresh - a file that broke
    // between the open and the pick surfaces its reason instead of applying.
    fn apply(&mut self, name: &str) -> Result<(), ThemeError> {
        self.active = match self.previews.get(name) {
            Some(resolved) => resolved.clone(),
            None => theme::load(name, &self.themes_dir)?,
        };
        self.active_name = name.to_string();
        Ok(())
    }
}

// Every theme `discovered` (plus the built-ins) that resolves, by name -
// resolution work done ONCE per open, so the per-frame preview is a lookup.
fn resolve_previews(
    discovered: &[(String, Result<SparseTheme, ThemeError>)],
    themes_dir: &std::path::Path,
) -> HashMap<String, Theme> {
    let mut previews: HashMap<String, Theme> = theme::BUILT_INS
        .iter()
        .map(|name| {
            let built_in = theme::load(name, themes_dir).expect("built-ins load by name");
            (name.to_string(), built_in)
        })
        .collect();
    for (name, parsed) in discovered {
        if let Ok(sparse) = parsed {
            previews.insert(name.clone(), sparse.over(theme::dark()));
        }
    }
    previews
}

/// Populates `/theme`'s selector (ADR-0038): a fresh directory read via
/// [`ThemeSelection::open`], posted as a SelectorReady through the same
/// injection channel as `/model`'s fetch - no spawned task, because the read
/// is local and instant, but the same event flow, so the pure core's
/// generation guard and Loading overlay behave identically.
pub(super) fn run(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    themes: &mut ThemeSelection,
    generation: u64,
) -> Screen {
    let rows = themes.open();
    let _ = ctx
        .selector_tx
        .send(Event::selector_ready(generation, rows));
    screen
}

/// Interprets a `/theme` pick (ADR-0038). Re-selecting the current theme is a
/// no-op. Otherwise this impure part does the I/O - swap the active Theme,
/// persist the name by the sparse config write, read `SUSPENDERS_THEME` -
/// then hands those facts to the pure [`applied_line`]. A persist failure is
/// surfaced but the live swap still stands, exactly like `/model`.
pub(super) fn choose(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    themes: &mut ThemeSelection,
    value: String,
) -> Screen {
    let Some(name) = pick(&themes.active_name, value) else {
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
    use std::path::Path;

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
        assert!(rows.iter().all(|r| r.selectable));
        assert!(
            rows.iter()
                .filter(|r| r.selectable)
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
    fn a_broken_theme_is_listed_unselectable_with_its_reason() {
        // ADR-0038: strict per-file, resilient app - the file is refused
        // whole, but /theme shows it with the reason instead of hiding it.
        let rows = theme_rows(
            &[broken("typo", "colors.added: bad"), valid("zebra")],
            "dark",
        );
        assert_eq!(rows[2].label, "typo");
        assert!(!rows[2].selectable, "Enter can never pick a broken theme");
        assert_eq!(rows[2].hint.as_deref(), Some("colors.added: bad"));
        assert!(rows[3].selectable, "the valid neighbor stays pickable");
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

    // --- preview_name (what an open overlay previews) ------------------------

    fn theme_overlay(
        status: OverlayStatus,
        rows: Vec<SelectorRow>,
        highlight: usize,
    ) -> OverlayView {
        OverlayView::Selector {
            command: "theme".into(),
            status,
            rows,
            highlight,
        }
    }

    #[test]
    fn the_highlighted_ready_theme_row_previews() {
        let overlay = theme_overlay(
            OverlayStatus::Ready,
            vec![
                SelectorRow::new("dark", "dark", None),
                SelectorRow::new("light", "light", None),
            ],
            1,
        );
        assert_eq!(preview_name(Some(&overlay)), Some("light"));
    }

    #[test]
    fn an_unselectable_highlight_previews_nothing() {
        // A filter that leaves only a broken row parks the highlight on it;
        // the active Theme keeps rendering (what Escape would keep).
        let overlay = theme_overlay(
            OverlayStatus::Ready,
            vec![SelectorRow::header(
                "typo",
                Some("colors.added: bad".into()),
            )],
            0,
        );
        assert_eq!(preview_name(Some(&overlay)), None);
    }

    #[test]
    fn other_overlays_and_states_preview_nothing() {
        assert_eq!(preview_name(None), None, "no overlay: the active Theme");
        // Loading/Failed have no rows to preview.
        assert_eq!(
            preview_name(Some(&theme_overlay(OverlayStatus::Loading, vec![], 0))),
            None
        );
        assert_eq!(
            preview_name(Some(&theme_overlay(
                OverlayStatus::Failed("nope".into()),
                vec![],
                0
            ))),
            None
        );
        // Another command's selector never drives the theme.
        let model = OverlayView::Selector {
            command: "model".into(),
            status: OverlayStatus::Ready,
            rows: vec![SelectorRow::new("local/m", "local/m", None)],
            highlight: 0,
        };
        assert_eq!(preview_name(Some(&model)), None);
        // The Slash menu itself previews nothing either.
        let menu = OverlayView::Menu {
            rows: vec![SelectorRow::new("theme", "theme", None)],
            highlight: 0,
        };
        assert_eq!(preview_name(Some(&menu)), None);
    }

    // --- ThemeSelection (launch fallback, open, preview, apply) --------------

    fn write_theme(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("test theme writes");
    }

    #[test]
    fn launch_resolves_the_configured_theme_without_a_notice() {
        let (selection, notice) = ThemeSelection::launch("light", "/no/such/dir".into());
        assert_eq!(notice, None);
        assert_eq!(selection.active_name, "light");
        assert_eq!(selection.active(), theme::light());
    }

    #[test]
    fn launch_falls_back_to_dark_with_a_notice_naming_theme_and_reason() {
        let (selection, notice) = ThemeSelection::launch("ghost", "/no/such/dir".into());
        assert_eq!(
            notice.as_deref(),
            Some(
                "theme \"ghost\" could not be used (no theme named \"ghost\"); using the built-in dark"
            )
        );
        // The fallback IS the active theme - /theme marks dark current and
        // re-picking dark stays a no-op.
        assert_eq!(selection.active_name, "dark");
        assert_eq!(selection.active(), theme::dark());
    }

    #[test]
    fn launch_surfaces_a_broken_configured_themes_parse_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "typo.toml", "[colors]\nadded = \"greenish\"\n");
        let (selection, notice) = ThemeSelection::launch("typo", dir.path().to_path_buf());
        let notice = notice.expect("a broken theme falls back with a notice");
        assert!(notice.starts_with("theme \"typo\" could not be used (colors.added:"));
        assert!(notice.ends_with("using the built-in dark"));
        assert_eq!(selection.active(), theme::dark());
    }

    #[test]
    fn open_reads_the_directory_fresh_and_fills_the_previews() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
        write_theme(dir.path(), "typo.toml", "[colors]\nadedd = \"green\"\n");
        let (mut selection, _) = ThemeSelection::launch("dark", dir.path().to_path_buf());

        let rows = selection.open();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["dark", "light", "mine", "typo"]);
        assert_eq!(rows[0].hint.as_deref(), Some("(current)"));
        assert!(!rows[3].selectable, "the broken file is listed, unpickable");

        // Previews resolved once at the open: built-ins plus the valid file
        // over the dark floor; the broken file resolves nothing.
        assert_eq!(
            selection.previews.get("mine").expect("resolved").added,
            crate::ui::theme::Color::Rgb(16, 16, 16)
        );
        assert!(!selection.previews.contains_key("typo"));

        // A LATER open sees new files - the directory is read per open.
        write_theme(dir.path(), "aurora.toml", "");
        let rows = selection.open();
        assert!(rows.iter().any(|r| r.label == "aurora"));
        assert!(selection.previews.contains_key("aurora"));
    }

    #[test]
    fn render_theme_previews_the_highlight_and_reverts_when_the_overlay_closes() {
        let (mut selection, _) = ThemeSelection::launch("dark", "/no/such/dir".into());
        let rows = selection.open();

        // Highlight on light: the frame renders light, the active stays dark.
        let overlay = theme_overlay(OverlayStatus::Ready, rows, 1);
        assert_eq!(selection.render_theme(Some(&overlay)), theme::light());
        assert_eq!(selection.active(), theme::dark());

        // The overlay closed (Escape or a resolved pick): the very next frame
        // is the active Theme again - the exact revert (ADR-0038).
        assert_eq!(selection.render_theme(None), theme::dark());
    }

    #[test]
    fn apply_swaps_the_active_theme_from_the_previews() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
        let (mut selection, _) = ThemeSelection::launch("dark", dir.path().to_path_buf());
        selection.open();

        selection.apply("mine").expect("a previewed theme applies");
        assert_eq!(selection.active_name, "mine");
        assert_eq!(
            selection.active().added,
            crate::ui::theme::Color::Rgb(16, 16, 16)
        );
        // Unstated slots resolved over the dark floor.
        assert_eq!(selection.active().removed, theme::dark().removed);
    }

    #[test]
    fn apply_of_an_unresolvable_name_refuses_and_keeps_the_active_theme() {
        let (mut selection, _) = ThemeSelection::launch("dark", "/no/such/dir".into());
        let err = selection.apply("ghost").unwrap_err();
        assert_eq!(err, ThemeError::NotFound("ghost".into()));
        assert_eq!(selection.active_name, "dark", "nothing swapped");
    }
}
