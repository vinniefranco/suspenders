use super::*;

// --- parse_slot (the single-slot parse funnel) --------------------------

#[test]
fn parse_slot_passes_none_through_as_none() {
    assert_eq!(parse_slot("added", None).unwrap(), None);
}

#[test]
fn parse_slot_parses_a_valid_color_string() {
    assert_eq!(
        parse_slot("added", Some("green".to_string())).unwrap(),
        Some(Color::Green)
    );
    assert_eq!(
        parse_slot("added", Some("#ff0000".to_string())).unwrap(),
        Some(Color::Rgb(255, 0, 0))
    );
}

#[test]
fn parse_slot_wraps_a_bad_value_as_invalid_naming_the_slot() {
    let err = parse_slot("removed", Some("mauve".to_string())).unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.starts_with("colors.removed:"), "{reason}");
    assert!(reason.contains("\"mauve\" is not a color"), "{reason}");
}

// --- missing_slot (the built-in totality error) -------------------------

#[test]
fn missing_slot_produces_an_invalid_error_naming_the_slot() {
    let err = missing_slot("syntax");
    assert_eq!(err, ThemeError::Invalid("missing slot \"syntax\"".into()));
}

// --- Color parsing lives in the `color` leaf's own tests. ---------------

// --- SparseTheme::parse (strict per-file, ADR-0038) ---------------------

#[test]
fn a_three_line_theme_is_valid_and_sparse() {
    let sparse = SparseTheme::parse("[colors]\nadded = \"#00ff00\"\nheading = \"magenta\"\n")
        .expect("sparse themes parse");
    assert_eq!(sparse.added, Some(Color::Rgb(0, 255, 0)));
    assert_eq!(sparse.heading, Some(Color::Magenta));
    assert_eq!(sparse.removed, None, "unstated slots stay unstated");
    assert_eq!(sparse.syntax, None);
}

#[test]
fn an_empty_file_is_a_valid_all_default_theme() {
    assert_eq!(
        SparseTheme::parse("").expect("empty is valid"),
        SparseTheme::default()
    );
}

#[test]
fn malformed_toml_rejects_the_file() {
    let err = SparseTheme::parse("[colors\nadded = ").unwrap_err();
    assert!(matches!(err, ThemeError::Invalid(_)), "{err}");
}

#[test]
fn an_unknown_top_level_key_rejects_the_file() {
    let err = SparseTheme::parse("bold = true\n").unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.contains("unknown field `bold`"), "{reason}");
}

#[test]
fn an_unknown_color_slot_rejects_the_file() {
    let err = SparseTheme::parse("[colors]\nadedd = \"green\"\n").unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.contains("unknown field `adedd`"), "{reason}");
}

#[test]
fn an_unparsable_color_rejects_the_file_naming_its_slot() {
    let err = SparseTheme::parse("[colors]\nadded = \"greenish\"\n").unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.starts_with("colors.added:"), "{reason}");
    assert!(reason.contains("\"greenish\" is not a color"), "{reason}");
}

#[test]
fn a_non_string_color_value_rejects_the_file() {
    let err = SparseTheme::parse("[colors]\nadded = 3\n").unwrap_err();
    assert!(matches!(err, ThemeError::Invalid(_)), "{err}");
}

#[test]
fn a_bundled_syntax_theme_name_is_accepted() {
    let sparse = SparseTheme::parse("syntax = \"Solarized (light)\"\n").expect("bundled name");
    assert_eq!(sparse.syntax.as_deref(), Some("Solarized (light)"));
}

#[test]
fn an_unknown_syntax_theme_name_rejects_the_file_listing_what_exists() {
    let err = SparseTheme::parse("syntax = \"dracula\"\n").unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(
        reason.contains("\"dracula\" is not a bundled syntax theme"),
        "{reason}"
    );
    assert!(reason.contains("base16-ocean.dark"), "{reason}");
}

// --- resolution (sparse over the dark floor) ----------------------------

#[test]
fn an_empty_sparse_theme_over_dark_is_exactly_dark() {
    // Dark totality, from the other side: were any slot missing in
    // dark.toml, dark() itself would panic; this pins that resolution
    // adds nothing on top of a total floor.
    assert_eq!(SparseTheme::default().over(dark()), *dark());
}

#[test]
fn a_stated_slot_wins_and_the_rest_falls_back() {
    let sparse = SparseTheme::parse("[colors]\nadded = \"#123456\"\n").expect("valid");
    let resolved = sparse.over(dark());
    assert_eq!(resolved.added, Color::Rgb(0x12, 0x34, 0x56));
    assert_eq!(resolved.removed, dark().removed, "unstated → the floor");
    assert_eq!(resolved.syntax, dark().syntax);
}

#[test]
fn a_stated_syntax_wins_over_the_floors() {
    let sparse = SparseTheme::parse("syntax = \"InspiredGitHub\"\n").expect("valid");
    assert_eq!(sparse.over(dark()).syntax, "InspiredGitHub");
}

// --- built-ins ----------------------------------------------------------

#[test]
fn dark_is_total_and_reproduces_todays_palette() {
    // Totality: the embedded file states every slot (total() would Err).
    let theme = SparseTheme::parse(DARK_TOML)
        .expect("dark parses")
        .total()
        .expect("dark states every slot");
    assert_eq!(theme, *dark());

    // The exact palette from ui::components - ANSI names where the code
    // used named colors, exact Rgb where it used Rgb (ADR-0038: the
    // default stays byte-identical and terminal-respecting).
    assert_eq!(theme.syntax, "base16-ocean.dark");
    assert_eq!(theme.added, Color::Green);
    assert_eq!(theme.removed, Color::Red);
    assert_eq!(theme.added_bg, Color::Rgb(18, 41, 27));
    assert_eq!(theme.removed_bg, Color::Rgb(51, 26, 29));
    assert_eq!(theme.context, Color::DarkGray);
    assert_eq!(theme.muted, Color::DarkGray);
    assert_eq!(theme.error, Color::Red);
    // The Phase 7 qwen roles (ADR-0008): the designed QwenDark hexes.
    assert_eq!(theme.foreground, Color::Rgb(0xbf, 0xbd, 0xb6));
    assert_eq!(theme.accent, Color::Rgb(0xD2, 0xA6, 0xFF));
    assert_eq!(theme.success, Color::Rgb(0xAA, 0xD9, 0x4C));
    assert_eq!(theme.warning, Color::Rgb(0xFF, 0xD7, 0x00));
    assert_eq!(theme.thinking, Color::DarkGray);
    assert_eq!(theme.thinking_header, Color::DarkGray);
    assert_eq!(theme.heading, Color::Cyan);
    assert_eq!(theme.bullet, Color::Cyan);
    assert_eq!(theme.quote, Color::DarkGray);
    assert_eq!(theme.link, Color::Blue);
    assert_eq!(theme.code, Color::Yellow);
    assert_eq!(theme.code_block, Color::Rgb(185, 215, 180));
    assert_eq!(theme.code_block_bg, Color::Rgb(25, 25, 35));
    assert_eq!(theme.popup_border, Color::Cyan);
}

#[test]
fn light_is_total_valid_and_picks_a_light_syntax_theme() {
    // light() only needs to be a valid sparse file, but the shipped one
    // states every slot - a light theme leaning on dark fallbacks would
    // be unreadable, so totality is pinned here too.
    let theme = SparseTheme::parse(LIGHT_TOML)
        .expect("light parses")
        .total()
        .expect("light states every slot");
    assert_eq!(theme, *light());
    assert_eq!(theme.syntax, "base16-ocean.light");
    assert_ne!(
        theme.code_block_bg,
        dark().code_block_bg,
        "light is its own polarity"
    );
    // The Phase 7 qwen roles take light-polarity counterparts (ADR-0008),
    // distinct from QwenDark's bright hues.
    assert_eq!(theme.foreground, Color::Rgb(0x24, 0x29, 0x2f));
    assert_eq!(theme.accent, Color::Rgb(0x88, 0x39, 0xef));
    assert_eq!(theme.success, Color::Rgb(0x1a, 0x7f, 0x37));
    assert_eq!(theme.warning, Color::Rgb(0x9a, 0x67, 0x00));
    assert_ne!(
        theme.accent,
        dark().accent,
        "light accent is its own polarity"
    );
}

#[test]
fn the_phase_7_qwen_roles_parse_to_their_designed_hexes() {
    // The four semantic slots carved in Phase 7 (ADR-0008) enter as HEX,
    // not legacy ANSI - so a drift in either toml is caught right here at
    // the slot boundary.
    let dark = dark();
    assert_eq!(dark.foreground, Color::Rgb(0xbf, 0xbd, 0xb6));
    assert_eq!(dark.accent, Color::Rgb(0xD2, 0xA6, 0xFF));
    assert_eq!(dark.success, Color::Rgb(0xAA, 0xD9, 0x4C));
    assert_eq!(dark.warning, Color::Rgb(0xFF, 0xD7, 0x00));
}

#[test]
fn a_missing_slot_fails_totality_naming_it() {
    let err = SparseTheme::parse("[colors]\nadded = \"green\"\n")
        .expect("valid sparse")
        .total()
        .unwrap_err();
    assert_eq!(err, ThemeError::Invalid("missing slot \"syntax\"".into()));
}

// --- discovery ----------------------------------------------------------

fn write_theme(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("test file writes");
}

#[test]
fn discovery_lists_toml_stems_sorted_with_per_file_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "zebra.toml", "[colors]\nadded = \"green\"\n");
    write_theme(
        dir.path(),
        "broken.toml",
        "[colors]\nadded = \"greenish\"\n",
    );
    write_theme(dir.path(), "notes.txt", "not a theme");

    let found = discover(dir.path());
    let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["broken", "zebra"], "sorted, non-toml ignored");
    let broken = found[0].1.as_ref().unwrap_err();
    assert!(
        broken.to_string().starts_with("colors.added:"),
        "a broken file carries its reason: {broken}"
    );
    assert_eq!(
        found[1].1.as_ref().expect("zebra is valid").added,
        Some(Color::Green)
    );
}

#[test]
fn a_user_file_named_like_a_built_in_is_refused_as_a_shadow() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "dark.toml", "[colors]\nadded = \"blue\"\n");

    let found = discover(dir.path());
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0],
        (
            "dark".to_string(),
            Err(ThemeError::ShadowsBuiltIn("dark".into()))
        )
    );
}

#[test]
fn a_missing_themes_directory_discovers_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(discover(&dir.path().join("no-such-dir")).is_empty());
}

// --- load (by configured name) ------------------------------------------

#[test]
fn built_ins_load_by_name_without_touching_the_directory() {
    let nowhere = Path::new("/no/such/dir");
    assert_eq!(load("dark", nowhere).expect("built-in"), *dark());
    assert_eq!(load("light", nowhere).expect("built-in"), *light());
}

#[test]
fn every_built_ins_name_resolves_through_the_built_in_mapping() {
    // The drift guard: a name added to BUILT_INS without a built_in() arm
    // would be listed and reserved but unloadable - this catches it.
    let nowhere = Path::new("/no/such/dir");
    for name in BUILT_INS {
        assert_eq!(
            load(name, nowhere).as_ref().ok(),
            built_in(name),
            "BUILT_INS entry \"{name}\" must resolve via built_in()"
        );
        assert!(built_in(name).is_some(), "{name} has no built_in() arm");
    }
}

#[test]
fn a_built_in_name_wins_over_a_user_file_of_the_same_stem() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "dark.toml", "[colors]\nadded = \"blue\"\n");
    assert_eq!(
        load("dark", dir.path()).expect("the built-in loads"),
        *dark(),
        "the shadowing file is never read"
    );
}

#[test]
fn a_user_theme_loads_resolved_over_the_dark_floor() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
    let theme = load("mine", dir.path()).expect("valid user theme");
    assert_eq!(theme.added, Color::Rgb(16, 16, 16));
    assert_eq!(theme.removed, dark().removed);
}

#[test]
fn a_missing_theme_is_a_typed_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = load("ghost", dir.path()).unwrap_err();
    assert_eq!(err, ThemeError::NotFound("ghost".into()));
    assert_eq!(err.to_string(), "no theme named \"ghost\"");
}

#[test]
fn a_broken_theme_loads_as_invalid_with_its_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_theme(dir.path(), "typo.toml", "[colors]\nadedd = \"green\"\n");
    let err = load("typo", dir.path()).unwrap_err();
    let ThemeError::Invalid(reason) = &err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(reason.contains("unknown field `adedd`"), "{reason}");
}

// --- the error's display (shown verbatim by /theme) ---------------------

#[test]
fn shadow_errors_read_as_a_sentence() {
    assert_eq!(
        ThemeError::ShadowsBuiltIn("light".into()).to_string(),
        "shadows the built-in \"light\" theme"
    );
}
