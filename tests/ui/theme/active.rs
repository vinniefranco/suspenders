use super::*;
use crate::ui::theme::{self, Color};
use std::path::Path;

fn write_theme(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("test theme writes");
}

#[test]
fn launch_resolves_the_configured_theme_without_a_notice() {
    let (active, notice) = ActiveTheme::launch("light", "/no/such/dir".into());
    assert_eq!(notice, None);
    assert_eq!(active.active_name(), "light");
    assert_eq!(active.active(), theme::light());
}

#[test]
fn launch_falls_back_to_dark_with_a_notice_naming_theme_and_reason() {
    let (active, notice) = ActiveTheme::launch("ghost", "/no/such/dir".into());
    assert_eq!(
        notice.as_deref(),
        Some(
            "theme \"ghost\" could not be used (no theme named \"ghost\"); using the built-in dark"
        )
    );
    // The fallback IS the active theme - /theme marks dark current and
    // re-picking dark stays a no-op.
    assert_eq!(active.active_name(), "dark");
    assert_eq!(active.active(), theme::dark());
}

#[test]
fn launch_surfaces_a_broken_configured_themes_parse_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "typo.toml", "[colors]\nadded = \"greenish\"\n");
    let (active, notice) = ActiveTheme::launch("typo", dir.path().to_path_buf());
    let notice = notice.expect("a broken theme falls back with a notice");
    assert!(notice.starts_with("theme \"typo\" could not be used (colors.added:"));
    assert!(notice.ends_with("using the built-in dark"));
    assert_eq!(active.active(), theme::dark());
}

#[test]
fn open_reads_the_directory_fresh_and_fills_the_previews() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
    write_theme(dir.path(), "typo.toml", "[colors]\nadedd = \"green\"\n");
    let (mut active, _) = ActiveTheme::launch("dark", dir.path().to_path_buf());

    let discovered = active.open();
    let names: Vec<&str> = discovered.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["mine", "typo"], "the fresh directory read");

    // Previews resolved once at the open: built-ins plus the valid file
    // over the dark floor; the broken file resolves nothing.
    assert_eq!(
        active.previews.get("mine").expect("resolved").added,
        Color::Rgb(16, 16, 16)
    );
    assert!(active.previews.contains_key("dark"));
    assert!(active.previews.contains_key("light"));
    assert!(!active.previews.contains_key("typo"));

    // A LATER open sees new files - the directory is read per open.
    write_theme(dir.path(), "aurora.toml", "");
    active.open();
    assert!(active.previews.contains_key("aurora"));
}

#[test]
fn render_theme_previews_the_named_theme_and_reverts_without_one() {
    let (mut active, _) = ActiveTheme::launch("dark", "/no/such/dir".into());
    active.open();

    // A previewed highlight renders light; the active stays dark.
    assert_eq!(active.render_theme(Some("light")), theme::light());
    assert_eq!(active.active(), theme::dark());

    // The overlay closed (Escape or a resolved pick): the very next frame
    // is the active Theme again - the exact revert (ADR-0038). A name
    // outside the previews (never resolvable) also renders the active.
    assert_eq!(active.render_theme(None), theme::dark());
    assert_eq!(active.render_theme(Some("ghost")), theme::dark());
}

#[test]
fn apply_swaps_the_active_theme() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
    let (mut active, _) = ActiveTheme::launch("dark", dir.path().to_path_buf());
    active.open();

    active.apply("mine").expect("a valid theme applies");
    assert_eq!(active.active_name(), "mine");
    assert_eq!(active.active().added, Color::Rgb(16, 16, 16));
    // Unstated slots resolved over the dark floor.
    assert_eq!(active.active().removed, theme::dark().removed);
}

#[test]
fn apply_loads_fresh_from_disk_not_the_open_time_previews() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
    let (mut active, _) = ActiveTheme::launch("dark", dir.path().to_path_buf());
    active.open();

    // The file changed between the open and the pick: Enter applies what
    // is on disk NOW, not the cached preview.
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#202020\"\n");
    active.apply("mine").expect("the re-read file applies");
    assert_eq!(active.active().added, Color::Rgb(32, 32, 32));
}

#[test]
fn apply_of_a_file_broken_after_the_open_refuses_with_its_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
    let (mut active, _) = ActiveTheme::launch("dark", dir.path().to_path_buf());
    active.open();
    assert!(active.previews.contains_key("mine"), "cached at the open");

    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"greenish\"\n");
    let err = active.apply("mine").unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.starts_with("colors.added:"), "{reason}");
    assert_eq!(active.active_name(), "dark", "nothing swapped");
    assert_eq!(active.active(), theme::dark());
}

#[test]
fn apply_of_an_unresolvable_name_refuses_and_keeps_the_active_theme() {
    let (mut active, _) = ActiveTheme::launch("dark", "/no/such/dir".into());
    let err = active.apply("ghost").unwrap_err();
    assert_eq!(err, ThemeError::NotFound("ghost".into()));
    assert_eq!(active.active_name(), "dark", "nothing swapped");
}
