
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
        crate::view_model::RowRole::Note,
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
