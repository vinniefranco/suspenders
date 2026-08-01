
use super::*;
use crate::view_model::RowRole;

impl CommandSelector {
    // Whether a selector is open at all (anything but the Null-Object `Idle`) -
    // a test observability seam over the private status.
    fn is_open(&self) -> bool {
        !matches!(self.status, DialogStatus::Idle)
    }
}

// =======================================================================
// The Composer fold (state, first refusal, overlays, history).
// =======================================================================

fn fresh() -> Composer {
    Composer::new(vec![])
}

fn with_history(entries: &[&str]) -> Composer {
    Composer::new(entries.iter().map(|s| s.to_string()).collect())
}

// A Composer holding `value` with the cursor at char index `cursor` -
// direct field access (same module) stands in for the typing that
// produced it.
fn with_draft(value: &str, cursor: usize) -> Composer {
    let mut c = fresh();
    set_draft(&mut c, value, cursor);
    c
}

fn set_draft(c: &mut Composer, value: &str, cursor: usize) {
    c.value = value.to_string();
    c.cursor = cursor;
}

// A slash draft with the cursor at the end - the menu/selector openers.
fn slashing(draft: &str) -> Composer {
    with_draft(draft, draft.chars().count())
}

// Folds `key` when idle, asserting the Composer consumed it without a
// notice; returns the effects. (Named apart from the module's `consumed`
// outcome constructor - this one FOLDS, that one builds.)
fn fold_consumed(c: &mut Composer, key: Key) -> Vec<Effect> {
    match c.handle_key(UngatedKey::for_test(key), Status::Idle) {
        KeyOutcome::Consumed {
            effects,
            notice: None,
        } => effects,
        other => panic!("expected a plain consume, got {other:?}"),
    }
}

fn press(c: &mut Composer, keys: Vec<Key>) {
    for key in keys {
        fold_consumed(c, key);
    }
}

fn typed(text: &str) -> Vec<Key> {
    text.chars().map(Key::Char).collect()
}

fn overlay(c: &Composer) -> Option<OverlayView> {
    c.view().overlay
}

// Delivers an event, asserting the Composer consumed it with no effects.
fn deliver(c: &mut Composer, event: Event) {
    match c.apply_event(event) {
        EventOutcome::Consumed(effects) => assert_eq!(effects, vec![]),
        EventOutcome::Refused(event) => panic!("expected a consume, got refusal of {event:?}"),
    }
}

// --- Slash Command menu (ADR-0032) --------------------------------------

// The palette's suggestion VALUES (System B) for the open Menu overlay.
fn menu_values(c: &Composer) -> Vec<String> {
    match overlay(c) {
        Some(OverlayView::Menu { suggestions, .. }) => {
            suggestions.iter().map(|s| s.value.clone()).collect()
        }
        other => panic!("expected the menu, got {other:?}"),
    }
}

#[test]
fn a_leading_slash_opens_the_palette_showing_every_command() {
    match overlay(&slashing("/")) {
        Some(OverlayView::Menu {
            suggestions,
            active,
            ..
        }) => {
            assert_eq!(
                suggestions
                    .iter()
                    .map(|s| s.value.clone())
                    .collect::<Vec<_>>(),
                // Shortest-first on the empty query (the ladder tiebreak):
                // `mcp` (3) leads `model`/`theme` (5), which keep registry order.
                vec!["mcp", "model", "theme"]
            );
            assert_eq!(active, 0);
        }
        other => panic!("expected the palette on '/', got {other:?}"),
    }
}

#[test]
fn a_non_slash_draft_has_no_overlay() {
    assert_eq!(overlay(&with_draft("fix the bug", 11)), None);
    assert_eq!(overlay(&fresh()), None);
}

#[test]
fn typing_ranks_the_palette_by_the_command_token() {
    // "/mod" prefix-matches only "model".
    assert_eq!(menu_values(&slashing("/mod")), vec!["model"]);
    // A token that matches nothing leaves an empty (but open) palette.
    match overlay(&slashing("/zzz")) {
        Some(OverlayView::Menu { suggestions, .. }) => assert!(suggestions.is_empty()),
        other => panic!("expected the open empty palette, got {other:?}"),
    }
}

#[test]
fn up_down_move_the_palette_highlight_wrapping() {
    // Three commands: the arrows move between them and WRAP (System B nav).
    let mut c = slashing("/");
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(menu_highlight(&c), 1);
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(menu_highlight(&c), 2);
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(menu_highlight(&c), 0, "wraps to the first");
    assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
    assert_eq!(menu_highlight(&c), 2, "wraps to the last");

    // Typing narrows the palette to one row: the highlight clamps onto it.
    let mut c = slashing("/");
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(menu_highlight(&c), 1);
    for key in [Key::Char('m'), Key::Char('o'), Key::Char('d')] {
        assert_eq!(fold_consumed(&mut c, key), vec![]);
    }
    assert_eq!(menu_highlight(&c), 0, "clamped to the one ranked row");
}

fn menu_highlight(c: &Composer) -> usize {
    match overlay(c) {
        Some(OverlayView::Menu { active, .. }) => active,
        other => panic!("expected the menu, got {other:?}"),
    }
}

// `/model` opens a selector (ADR-0033), so committing it does NOT clear
// the draft the way a fire-and-run command would. It normalizes the draft
// to `"/model "`, sets a Loading overlay, and emits ONE Effect::Command -
// the selector-activation path is exercised separately below.
#[test]
fn enter_commits_the_highlighted_command() {
    let mut c = slashing("/model");
    let effects = fold_consumed(&mut c, Key::Enter);
    assert_eq!(
        effects,
        vec![Effect::Command {
            name: "model".into(),
            generation: 1,
        }]
    );
    // Selector-opening: draft normalized, NOT cleared; overlay is Loading.
    assert_eq!(c.view().draft, "/model ");
    assert_eq!(c.view().cursor, 7);
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
}

#[test]
fn committing_a_partial_token_uses_the_highlighted_full_command_name() {
    // "/mod" filters to the one command; Enter commits "model", not "mod".
    let mut c = slashing("/mod");
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Command {
            name: "model".into(),
            generation: 1,
        }]
    );
}

#[test]
fn enter_on_an_unknown_command_yields_a_notice_and_no_effects() {
    let mut c = slashing("/nope");
    match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Idle) {
        KeyOutcome::Consumed { effects, notice } => {
            assert_eq!(effects, vec![], "no Turn, no command effect");
            assert_eq!(notice, Some("unknown command: /nope".into()));
        }
        other => panic!("expected a consumed notice, got {other:?}"),
    }
    assert_eq!(c.view().draft, "", "draft cleared");
    assert_eq!(overlay(&c), None);
}

#[test]
fn escape_closes_the_menu_by_clearing_the_draft() {
    let mut c = slashing("/model");
    assert_eq!(fold_consumed(&mut c, Key::Escape), vec![]);
    assert_eq!(c.view().draft, "");
    assert_eq!(overlay(&c), None);
}

#[test]
fn typing_and_backspace_fall_through_to_the_draft_while_slashing() {
    // A char extends the draft (and refilters the menu).
    let mut c = slashing("/mode");
    assert_eq!(fold_consumed(&mut c, Key::Char('l')), vec![]);
    assert_eq!(c.view().draft, "/model");
    match overlay(&c) {
        Some(OverlayView::Menu { suggestions, .. }) => assert_eq!(
            suggestions
                .iter()
                .map(|s| s.value.clone())
                .collect::<Vec<_>>(),
            vec!["model"]
        ),
        other => panic!("expected the menu, got {other:?}"),
    }

    // Backspace erases back toward the slash; the menu stays open.
    assert_eq!(fold_consumed(&mut c, Key::Backspace), vec![]);
    assert_eq!(c.view().draft, "/mode");
    assert!(overlay(&c).is_some());

    // Backspacing away the slash closes the menu; the remaining text is a
    // normal draft again.
    let mut c = slashing("/");
    fold_consumed(&mut c, Key::Backspace);
    assert_eq!(c.view().draft, "");
    assert_eq!(overlay(&c), None);
}

#[test]
fn a_slash_draft_never_submits_or_steers_even_while_running() {
    // Idle: Enter commits a command, never a Submit.
    let mut c = slashing("/model");
    assert!(matches!(
        fold_consumed(&mut c, Key::Enter).as_slice(),
        [Effect::Command { .. }]
    ));

    // Running: the leading `/` still opens the menu and Enter commits the
    // command - it is NOT Steering text.
    let mut c = slashing("/model");
    assert!(overlay(&c).is_some(), "menu opens while running");
    match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
        KeyOutcome::Consumed { effects, .. } => assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
                generation: 1,
            }]
        ),
        other => panic!("expected a consumed commit, got {other:?}"),
    }
}

// --- Slash Command selector overlay (ADR-0033) --------------------------

// A model row for the injected SelectorReady events (value = label).
fn model_row(id: &str) -> SelectorRow {
    SelectorRow::new(id, id, None)
}

// The generation the one Command effect of a commit carried - the echo
// the fill events must repeat to land.
fn command_generation(effects: &[Effect]) -> u64 {
    match effects {
        [Effect::Command { generation, .. }] => *generation,
        other => panic!("expected one Command effect, got {other:?}"),
    }
}

// The overlay after committing `/model` and delivering rows. The draft is
// left at `"/model "` (rest = Some("")), the sub-state.
fn model_selector_ready(rows: Vec<SelectorRow>) -> Composer {
    let mut c = slashing("/model");
    let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
    deliver(&mut c, Event::selector_ready(generation, rows));
    c
}

// The highlighted index of a selector overlay.
fn highlight_of(c: &Composer) -> usize {
    match overlay(c) {
        Some(OverlayView::Dialog { active, .. }) => active,
        other => panic!("expected a selector overlay, got {other:?}"),
    }
}

// --- selector_highlight (the non-allocating per-frame preview read) -----

#[test]
fn selector_highlight_names_the_command_and_the_highlighted_filtered_row() {
    let mut c = model_selector_ready(vec![model_row("qwen"), model_row("llama")]);
    let (command, row) = c.selector_highlight().expect("a Ready selector");
    assert_eq!(command, "model");
    assert_eq!(row, &model_row("qwen"));

    // It tracks the cursor and the rest filter, like the OverlayView.
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.selector_highlight().unwrap().1, &model_row("llama"));
    press(&mut c, typed("qw"));
    assert_eq!(
        c.selector_highlight().unwrap().1,
        &model_row("qwen"),
        "the filter narrowed and the highlight snapped"
    );
}

#[test]
fn selector_highlight_is_none_outside_a_ready_selector() {
    // No overlay, the menu, and a Loading selector all read None.
    assert_eq!(fresh().selector_highlight(), None);
    assert_eq!(with_draft("fix the bug", 11).selector_highlight(), None);
    assert_eq!(slashing("/model").selector_highlight(), None, "the menu");
    let mut c = slashing("/model");
    fold_consumed(&mut c, Key::Enter);
    assert_eq!(c.selector_highlight(), None, "Loading has no rows");
}

#[test]
fn selector_highlight_is_none_when_the_filter_leaves_nothing() {
    let mut c = model_selector_ready(vec![model_row("qwen")]);
    press(&mut c, typed("zzz"));
    assert_eq!(c.selector_highlight(), None, "an empty filtered view");
}

#[test]
fn committing_a_selector_command_by_enter_loads_normalizes_and_fetches_once() {
    let mut c = slashing("/model");
    // Exactly one Effect::Command (the adapter fetches).
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Command {
            name: "model".into(),
            generation: 1,
        }]
    );
    // Draft normalized to `/model ` (rest = Some("")) - NOT cleared.
    assert_eq!(c.view().draft, "/model ");
    // Overlay is Loading for `model`.
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
}

#[test]
fn committing_a_selector_command_by_typing_a_space_loads_and_fetches_once() {
    // Typing the space after `/model` commits it the same way Enter does.
    let mut c = slashing("/model");
    assert_eq!(
        fold_consumed(&mut c, Key::Char(' ')),
        vec![Effect::Command {
            name: "model".into(),
            generation: 1,
        }]
    );
    assert_eq!(c.view().draft, "/model ");
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
}

#[test]
fn selector_ready_flips_loading_to_ready_and_the_rest_filters_the_rows() {
    let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
    let mut c = model_selector_ready(rows);
    // Ready, all rows shown (rest is "").
    match overlay(&c) {
        Some(OverlayView::Dialog {
            status: OverlayStatus::Ready,
            rows,
            active,
            command,
            ..
        }) => {
            assert_eq!(command, "model");
            assert_eq!(rows.len(), 3);
            assert_eq!(active, 0);
        }
        other => panic!("expected Ready selector, got {other:?}"),
    }
    // Typing after the space filters via `rest` (the draft owns the filter).
    fold_consumed(&mut c, Key::Char('q'));
    assert_eq!(c.view().draft, "/model q");
    match overlay(&c) {
        Some(OverlayView::Dialog { rows, .. }) => {
            assert_eq!(rows, vec![model_row("qwen")], "only 'qwen' contains 'q'");
        }
        other => panic!("expected filtered selector, got {other:?}"),
    }
}

#[test]
fn a_model_filter_reveals_a_matching_greyed_group_with_its_trailing_note() {
    // System A (ADR-0051): the numbered dialog scrolls instead of collapsing,
    // so a greyed-group filter reveals every matching collapsed row and keeps
    // the trailing note - no "· N more" cap. Typing the provider name shows
    // its whole catalog plus the "unavailable" note.
    let mut rows = vec![SelectorRow::header("openrouter")];
    rows.extend((0..8).map(|i| SelectorRow::collapsed(format!("openrouter/m{i}"))));
    rows.push(SelectorRow::note(
        "  unavailable",
        Some("set OPENROUTER_API_KEY".into()),
    ));
    let mut c = model_selector_ready(rows);
    press(&mut c, typed("openrouter"));
    match overlay(&c) {
        Some(OverlayView::Dialog { rows, .. }) => {
            // header + 8 collapsed + note, none collapsed away.
            assert_eq!(rows.len(), 10);
            let note = rows.last().expect("the note trails the group");
            assert_eq!(note.hint.as_deref(), Some("set OPENROUTER_API_KEY"));
        }
        other => panic!("expected a Ready selector, got {other:?}"),
    }
}

#[test]
fn arrows_move_within_the_filtered_rows_of_a_ready_overlay() {
    // Arrow-only nav (ADR-0046): the wheel no longer reaches the Composer.
    // System A (ADR-0051, qwen `useSelectionList`) WRAPS at the ends - a
    // divergence from the retired saturating Selector.
    let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
    let mut c = model_selector_ready(rows);
    assert_eq!(
        fold_consumed(&mut c, Key::ArrowDown),
        vec![],
        "arrows drive the overlay, not a scroll"
    );
    assert_eq!(highlight_of(&c), 1);
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(highlight_of(&c), 2);
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(highlight_of(&c), 0, "wraps from the last row to the first");
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(highlight_of(&c), 2, "wraps back to the last");
}

#[test]
fn enter_on_a_ready_overlay_chooses_the_highlighted_row_and_closes() {
    let rows = vec![model_row("qwen"), model_row("llama")];
    let mut c = model_selector_ready(rows);
    // Move to the second row, then Enter.
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::SelectorChosen {
            command: "model".into(),
            value: "llama".into(),
        }]
    );
    // Overlay closed, draft cleared.
    assert_eq!(c.view().draft, "");
    assert_eq!(overlay(&c), None);
}

#[test]
fn enter_selects_the_filtered_highlighted_row() {
    let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
    let mut c = model_selector_ready(rows);
    // Filter to just "llama" via `rest`, then Enter selects it.
    press(&mut c, typed("ll"));
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::SelectorChosen {
            command: "model".into(),
            value: "llama".into(),
        }]
    );
}

#[test]
fn selector_failed_shows_a_failed_overlay_and_enter_does_nothing() {
    let mut c = slashing("/model");
    let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
    deliver(&mut c, Event::selector_failed(generation, "no server"));
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Failed(_),
            ..
        })
    ));
    // Enter on a Failed overlay does nothing (no rows to pick).
    assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Failed(_),
            ..
        })
    ));
}

#[test]
fn escape_closes_the_selector_overlay_and_clears_the_draft() {
    let mut c = model_selector_ready(vec![model_row("qwen")]);
    assert_eq!(fold_consumed(&mut c, Key::Escape), vec![]);
    assert_eq!(c.view().draft, "");
    assert_eq!(overlay(&c), None);
}

// --- the Frozen (theme) dialog (ADR-0051 System A, filter-less) ---------

// A theme (Frozen) dialog Ready with `rows` - the mirror of
// `model_selector_ready` but for `/theme`, which commits to a `Frozen`
// flavour that swallows editing keys.
fn theme_selector_ready(rows: Vec<SelectorRow>) -> Composer {
    let mut c = slashing("/theme");
    let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
    deliver(&mut c, Event::selector_ready(generation, rows));
    c
}

// The visible dialog rows of a Ready overlay.
fn dialog_rows_of(c: &Composer) -> Vec<SelectorRow> {
    match overlay(c) {
        Some(OverlayView::Dialog { rows, .. }) => rows,
        other => panic!("expected a Ready selector, got {other:?}"),
    }
}

#[test]
fn a_typed_char_in_a_frozen_theme_dialog_is_swallowed() {
    // Typing in a Frozen dialog must NOT grow the `/theme ` draft nor
    // refilter the rows (qwen's filter-less dialog).
    let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
    let before = c.clone();
    fold_consumed(&mut c, Key::Char('d'));
    assert_eq!(c.view().draft, "/theme ", "the draft did not grow");
    assert_eq!(
        dialog_rows_of(&c).len(),
        2,
        "the rows were not refiltered by the swallowed char"
    );
    assert_eq!(c, before, "a swallowed char leaves the Composer unchanged");
}

#[test]
fn a_digit_quick_selects_in_a_frozen_theme_dialog() {
    // In a Frozen dialog a digit is a quick-select (not a filter char).
    let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
    // '2' picks the second row.
    assert_eq!(
        fold_consumed(&mut c, Key::Char('2')),
        vec![Effect::SelectorChosen {
            command: "theme".into(),
            value: "light".into(),
        }]
    );
    assert_eq!(overlay(&c), None, "picking closed the dialog");
}

#[test]
fn backspace_and_cursor_moves_pass_through_a_frozen_theme_dialog() {
    // Frozen swallows text INSERTION, but Backspace-out and cursor moves
    // still work (so the user can back out of the sub-state).
    // A cursor move (Left) is consumed and leaves the draft TEXT intact
    // (only the cursor position changes).
    let mut c = theme_selector_ready(vec![model_row("dark")]);
    let cursor_before = c.view().cursor;
    fold_consumed(&mut c, Key::Left);
    assert_eq!(c.view().draft, "/theme ", "a cursor move never edits text");
    assert_eq!(c.view().cursor, cursor_before - 1, "the cursor moved left");
    // From the trailing-space cursor, Backspace removes the space, dropping
    // back to the menu (rest goes None).
    let mut c = theme_selector_ready(vec![model_row("dark")]);
    fold_consumed(&mut c, Key::Backspace);
    assert_eq!(c.view().draft, "/theme");
    assert!(matches!(overlay(&c), Some(OverlayView::Menu { .. })));
}

#[test]
fn selector_highlight_repoints_to_the_frozen_dialogs_active_row() {
    // The theme live-preview reads `selector_highlight` (the active row of
    // the frozen dialog); arrowing repoints it, so the preview follows.
    let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
    let (command, row) = c.selector_highlight().expect("a Ready theme dialog");
    assert_eq!(command, "theme");
    assert_eq!(row, &model_row("dark"), "the first row previews first");
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(
        c.selector_highlight().unwrap().1,
        &model_row("light"),
        "arrowing repoints the preview"
    );
}

// --- filter_rows travel rules (ADR-0051 group retention) ----------------

#[test]
fn filter_rows_travels_a_header_and_notes_when_a_member_matches() {
    // A group whose MEMBER matches keeps its header and its trailing note,
    // even though neither the header nor the note text contains the needle.
    let raw = vec![
        SelectorRow::header("openai"),
        SelectorRow::new("gpt-4", "gpt-4", None),
        SelectorRow::note("  note", Some("info".into())),
    ];
    let out = filter_rows(&raw, "gpt");
    assert_eq!(out.len(), 3, "header + member + note all travel");
    assert_eq!(out[0].role, RowRole::Header);
    assert_eq!(out[2].role, RowRole::Note);
}

#[test]
fn filter_rows_keeps_header_and_notes_on_a_header_match_alone() {
    // The header label itself matches: the whole group's header + notes are
    // shown even if no member matches the needle.
    let raw = vec![
        SelectorRow::header("anthropic"),
        SelectorRow::new("claude", "claude", None),
        SelectorRow::note("  note", Some("info".into())),
    ];
    let out = filter_rows(&raw, "anthropic");
    // header hit → group_hit true → header + note kept; the member does not
    // contain "anthropic" so it is NOT kept (member keeps only on its own).
    let roles: Vec<_> = out.iter().map(|r| r.role).collect();
    assert!(roles.contains(&RowRole::Header));
    assert!(roles.contains(&RowRole::Note));
}

#[test]
fn filter_rows_drops_a_group_that_matches_nothing() {
    let raw = vec![
        SelectorRow::header("openai"),
        SelectorRow::new("gpt-4", "gpt-4", None),
        SelectorRow::header("anthropic"),
        SelectorRow::new("claude", "claude", None),
    ];
    let out = filter_rows(&raw, "claude");
    // Only the anthropic group survives.
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].label, "anthropic");
    assert_eq!(out[1].label, "claude");
}

#[test]
fn filter_rows_reveals_a_collapsed_member_by_its_name() {
    // A collapsed (greyed) member is revealed when its own label matches.
    let raw = vec![
        SelectorRow::header("openrouter"),
        SelectorRow::collapsed("openrouter/mixtral"),
        SelectorRow::collapsed("openrouter/llama"),
    ];
    let out = filter_rows(&raw, "mixtral");
    // header travels (member matched) + the matching collapsed row; the
    // non-matching collapsed row is dropped.
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, RowRole::Header);
    assert_eq!(out[1].label, "openrouter/mixtral");
}

#[test]
fn a_model_dialog_nav_skips_the_disabled_header_end_to_end() {
    // End-to-end (is_stop → disabled → header-skip): the header is not a
    // cursor stop, so ArrowDown from the first member lands on the next
    // member, jumping the header row between the groups.
    let rows = vec![
        SelectorRow::header("openai"),
        model_row("gpt-4"),
        SelectorRow::header("anthropic"),
        model_row("claude"),
    ];
    let mut c = model_selector_ready(rows);
    // Initial active snaps onto the first navigable row (index 1, the
    // member - index 0 is the disabled header).
    assert_eq!(highlight_of(&c), 1, "active snaps off the header");
    // ArrowDown from the first member skips the second header (index 2) and
    // lands on the second member (index 3).
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(highlight_of(&c), 3, "nav skipped the disabled header");
}

// --- the `←/→` expand mechanic (ADR-0051 System B, qwen expandedIndex) --

#[test]
fn arrows_expand_a_long_active_palette_row_but_move_the_cursor_on_a_short_one() {
    // A short active row (a command name) never expands: `←/→` fall through
    // to plain cursor moves, so the draft cursor changes and the palette's
    // expanded flag stays false.
    let mut c = slashing("/m");
    let before_cursor = c.view().cursor;
    fold_consumed(&mut c, Key::Left);
    assert_ne!(c.view().cursor, before_cursor, "short row: cursor moved");
    match overlay(&c) {
        Some(OverlayView::Menu { expanded, .. }) => {
            assert!(!expanded, "a short active row never expands")
        }
        other => panic!("expected the palette, got {other:?}"),
    }
}

#[test]
fn backspacing_the_space_returns_to_the_menu_and_reactivation_refetches() {
    let mut c = model_selector_ready(vec![model_row("qwen")]);
    // Backspace removes the trailing space: `/model ` → `/model`, so rest
    // goes None and we are back in the COMMAND MENU (overlay dropped).
    assert_eq!(fold_consumed(&mut c, Key::Backspace), vec![]);
    assert_eq!(c.view().draft, "/model");
    assert!(matches!(overlay(&c), Some(OverlayView::Menu { .. })));
    // Re-committing is a fresh activation: it re-emits Effect::Command,
    // with a NEW generation (the helper's commit was generation 1).
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Command {
            name: "model".into(),
            generation: 2,
        }]
    );
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
}

#[test]
fn backspacing_the_slash_exits_slash_mode_entirely() {
    let mut c = model_selector_ready(vec![model_row("qwen")]);
    // Erase the whole `/model ` draft: selector → menu → gone.
    for _ in 0.."/model ".chars().count() {
        fold_consumed(&mut c, Key::Backspace);
    }
    assert_eq!(c.view().draft, "");
    assert_eq!(overlay(&c), None, "no longer a slash draft");
}

#[test]
fn a_stale_selector_ready_after_the_overlay_closed_is_ignored() {
    // Commit, then Escape to close the overlay.
    let mut c = slashing("/model");
    let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
    fold_consumed(&mut c, Key::Escape);
    assert_eq!(overlay(&c), None);
    // A late SelectorReady is still CONSUMED (by variant - the caller
    // never sees it) but must not resurrect the popup - even with the
    // matching generation: there is no overlay to fill.
    deliver(
        &mut c,
        Event::selector_ready(generation, vec![model_row("qwen")]),
    );
    assert_eq!(overlay(&c), None, "stale event ignored");
}

#[test]
fn selector_ready_is_ignored_when_no_overlay_is_loading() {
    // No slash draft at all: the event is consumed but changes nothing.
    let mut c = fresh();
    deliver(&mut c, Event::selector_ready(1, vec![model_row("qwen")]));
    assert_eq!(overlay(&c), None);
}

#[test]
fn a_second_selector_ready_does_not_overwrite_a_ready_overlay() {
    // Guard: once Ready, a duplicate delivery must not reset the cursor.
    // The duplicate carries the SAME generation (1, the helper's commit),
    // so it is the Loading check alone that absorbs it.
    let mut c = model_selector_ready(vec![model_row("qwen"), model_row("llama")]);
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(highlight_of(&c), 1);
    // A second (stale) ready arrives - the overlay is no longer Loading.
    deliver(&mut c, Event::selector_ready(1, vec![model_row("gpt")]));
    match overlay(&c) {
        Some(OverlayView::Dialog { rows, active, .. }) => {
            assert_eq!(rows.len(), 2, "kept the first delivery");
            assert_eq!(active, 1, "cursor untouched");
        }
        other => panic!("expected Ready selector, got {other:?}"),
    }
}

#[test]
fn a_fill_from_a_previous_activation_never_lands_on_a_fresh_overlay() {
    // The race the generation exists for: backspace out of a Loading
    // sub-state, re-commit, and only THEN the first fetch delivers.
    let mut c = slashing("/model");
    let first = command_generation(&fold_consumed(&mut c, Key::Enter));
    // Backspace removes the trailing space: back to the MENU, overlay
    // dropped on the next consumed key. Re-commit: a SECOND activation.
    fold_consumed(&mut c, Key::Backspace);
    let second = command_generation(&fold_consumed(&mut c, Key::Enter));
    assert_ne!(first, second, "each activation gets its own generation");
    // The FIRST activation's fill finally arrives: dropped - the fresh
    // overlay stays Loading, it never asked for these rows.
    deliver(
        &mut c,
        Event::selector_ready(first, vec![model_row("stale")]),
    );
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
    // The SECOND activation's own fill lands.
    deliver(
        &mut c,
        Event::selector_ready(second, vec![model_row("qwen")]),
    );
    match overlay(&c) {
        Some(OverlayView::Dialog {
            status: OverlayStatus::Ready,
            rows,
            ..
        }) => assert_eq!(rows, vec![model_row("qwen")], "the second fetch's rows"),
        other => panic!("expected Ready selector, got {other:?}"),
    }
}

#[test]
fn a_failure_echo_applies_only_to_its_own_activation() {
    // Same backspace-out-and-re-commit race, failure edition.
    let mut c = slashing("/model");
    let first = command_generation(&fold_consumed(&mut c, Key::Enter));
    fold_consumed(&mut c, Key::Backspace);
    let second = command_generation(&fold_consumed(&mut c, Key::Enter));
    // The first activation's failure is stale: dropped, still Loading.
    deliver(&mut c, Event::selector_failed(first, "no server"));
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Loading,
            ..
        })
    ));
    // The second activation's own failure lands.
    deliver(&mut c, Event::selector_failed(second, "no server"));
    assert!(matches!(
        overlay(&c),
        Some(OverlayView::Dialog {
            status: OverlayStatus::Failed(_),
            ..
        })
    ));
}

#[test]
fn events_other_than_the_selector_fills_are_refused_untouched() {
    // The event first-refusal contract: everything that is not an
    // overlay fill comes back exactly as offered.
    let mut c = fresh();
    let event = Event::run_started("r1");
    match c.apply_event(event.clone()) {
        EventOutcome::Refused(back) => assert_eq!(back, event),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// --- Submit vs Steer (the Status parameter) ------------------------------

#[test]
fn enter_with_a_blank_draft_is_consumed_as_a_no_op() {
    // Consumed, not refused: Enter is never the caller's key.
    let mut c = with_draft("   ", 3);
    assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
}

#[test]
fn enter_submits_the_trimmed_prompt_when_idle() {
    let mut c = with_draft("  fix the bug  ", 15);
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Agent(AgentCommand::Submit("fix the bug".into()))]
    );
}

#[test]
fn enter_steers_when_running_instead_of_submitting() {
    let mut c = with_draft("  also check the README  ", 10);
    match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
        KeyOutcome::Consumed { effects, .. } => assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Steer(
                "also check the README".into()
            ))]
        ),
        other => panic!("expected a consumed steer, got {other:?}"),
    }
}

// --- The refusal set (the routing contract's "not mine" rows) -----------
//
// The refusal CONTRACT: a Refused key hands back the SAME key and leaves
// the Composer BIT-IDENTICAL (`PartialEq` over the whole state - history
// ring and stash included). Checked across tricky states on purpose: a
// mid-recall ring with a stashed draft, and a leftover selector after
// backspacing out of the sub-state - the states where a stray mutation on
// the refusal path would hide.

// Asserts the refusal contract for `key` against a clone of `state`.
fn assert_refusal_is_pure(state: &Composer, key: Key, status: Status) {
    let mut c = state.clone();
    let outcome = c.handle_key(UngatedKey::for_test(key.clone()), status);
    assert_eq!(outcome, KeyOutcome::Refused(key), "expected a refusal");
    assert_eq!(
        &c, state,
        "a refused key must leave the Composer bit-identical"
    );
}

// A Composer parked mid-recall: "typing..." stashed, "b" recalled.
fn mid_recall() -> Composer {
    let mut c = with_history(&["a", "b"]);
    set_draft(&mut c, "typing...", 9);
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "b");
    c
}

// A Composer back in the MENU with the selector overlay left over from
// backspacing out of the sub-state. The leftover is dropped lazily, on
// the next CONSUMED menu key - never on a refusal.
fn leftover_selector() -> Composer {
    let mut c = model_selector_ready(vec![model_row("qwen")]);
    fold_consumed(&mut c, Key::Backspace); // `/model ` → `/model`: menu again
    assert!(c.selector.is_open(), "the overlay must linger for the test");
    c
}

#[test]
fn always_refused_keys_leave_any_composer_bit_identical() {
    let states = [
        fresh(),
        with_draft("mid-draft", 3),
        slashing("/model"),
        mid_recall(),
        leftover_selector(),
    ];
    for state in &states {
        for key in [
            Key::PageUp,
            Key::PageDown,
            Key::ToggleCompact,
            Key::Named("f1".into()),
            Key::Other,
        ] {
            assert_refusal_is_pure(state, key, Status::Idle);
        }
    }
}

#[test]
fn escape_and_the_wheel_are_refused_pure_when_no_overlay_is_open() {
    // With no overlay, Escape belongs to Cancellation; the wheel is inert
    // (ADR-0046: no mouse capture, native scrollback owns history), so the
    // Composer refuses both to the caller, whatever the draft or ring holds.
    for state in [fresh(), with_draft("plain draft", 5), mid_recall()] {
        for key in [Key::Escape, Key::WheelUp, Key::WheelDown] {
            assert_refusal_is_pure(&state, key, Status::Running);
        }
    }
    // The ring outlives a refusal: Down after a refused Escape still
    // restores the stash (the coverage the old toggle-cross-contamination
    // test held).
    let mut c = mid_recall();
    assert!(matches!(
        c.handle_key(UngatedKey::for_test(Key::Escape), Status::Running),
        KeyOutcome::Refused(_)
    ));
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, "typing...");
}

// --- The submitted/steered outcome hooks --------------------------------

#[test]
fn submitted_ok_records_clears_and_emits_the_on_disk_append() {
    let mut c = with_history(&["a"]);
    set_draft(&mut c, "b", 1);
    // The in-memory record and the on-disk append are one invariant.
    assert_eq!(c.submitted_ok("b"), vec![Effect::HistoryAppend("b".into())]);
    assert_eq!(c.view().draft, "");
    assert_eq!(c.view().cursor, 0);
    // Recorded AND the recall position reset: Up walks b, then a.
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "b");
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "a");
}

#[test]
fn steered_ok_clears_the_draft_without_recording() {
    let mut c = fresh();
    press(&mut c, typed("mid-turn note"));
    c.steered_ok();
    assert_eq!(c.view().draft, "");
    // Not a prompt: nothing recorded, so Up recalls nothing.
    assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
    assert_eq!(c.view().draft, "");
}

// --- Draft editing -------------------------------------------------------

#[test]
fn a_typed_char_appends_at_the_end_of_the_draft() {
    let mut c = fresh();
    press(&mut c, typed("hi"));
    assert_eq!(c.view().draft, "hi");
    assert_eq!(c.view().cursor, 2);
}

#[test]
fn a_typed_char_inserts_at_the_cursor_mid_draft() {
    let mut c = with_draft("hllo", 1);
    press(&mut c, vec![Key::Char('e')]);
    assert_eq!(c.view().draft, "hello");
    assert_eq!(c.view().cursor, 2);
}

#[test]
fn backspace_deletes_the_char_before_the_cursor() {
    let mut c = with_draft("hello", 3);
    press(&mut c, vec![Key::Backspace]);
    assert_eq!(c.view().draft, "helo");
    assert_eq!(c.view().cursor, 2);
}

#[test]
fn backspace_at_the_start_of_the_draft_is_a_noop() {
    let mut c = with_draft("hi", 0);
    press(&mut c, vec![Key::Backspace]);
    assert_eq!(c.view().draft, "hi");
    assert_eq!(c.view().cursor, 0);
}

// The cursor is a CHAR index: multi-byte chars must neither split nor
// panic under insert/delete around them.
#[test]
fn multibyte_chars_insert_and_delete_without_splitting() {
    let mut c = with_draft("héllo", 2);
    press(&mut c, vec![Key::Char('🎩')]);
    assert_eq!(c.view().draft, "hé🎩llo");
    assert_eq!(c.view().cursor, 3);

    press(&mut c, vec![Key::Backspace, Key::Backspace]);
    assert_eq!(c.view().draft, "hllo");
    assert_eq!(c.view().cursor, 1);
}

#[test]
fn left_and_right_move_the_cursor_clamped_at_both_ends() {
    let mut c = with_draft("ab", 1);
    press(&mut c, vec![Key::Left]);
    assert_eq!(c.view().cursor, 0);
    press(&mut c, vec![Key::Left]);
    assert_eq!(c.view().cursor, 0); // clamped at the start

    press(&mut c, vec![Key::Right, Key::Right]);
    assert_eq!(c.view().cursor, 2);
    press(&mut c, vec![Key::Right]);
    assert_eq!(c.view().cursor, 2); // clamped at the end
    assert_eq!(c.view().draft, "ab"); // movement never edits
}

#[test]
fn home_and_end_jump_within_the_current_line_not_the_whole_draft() {
    // "ab\ncdef\ng", cursor mid second line (index 5, on 'e').
    let mut c = with_draft("ab\ncdef\ng", 5);
    press(&mut c, vec![Key::Home]);
    assert_eq!(c.view().cursor, 3); // start of "cdef"
    press(&mut c, vec![Key::End]);
    assert_eq!(c.view().cursor, 7); // end of "cdef", before its '\n'
}

#[test]
fn home_and_end_on_a_single_line_draft_reach_both_ends() {
    let mut c = with_draft("hello", 3);
    press(&mut c, vec![Key::Home]);
    assert_eq!(c.view().cursor, 0);
    press(&mut c, vec![Key::End]);
    assert_eq!(c.view().cursor, 5);
}

#[test]
fn insert_newline_adds_a_hard_newline_at_the_cursor() {
    let mut c = with_draft("ab", 1);
    assert_eq!(fold_consumed(&mut c, Key::InsertNewline), vec![]);
    assert_eq!(c.view().draft, "a\nb");
    assert_eq!(c.view().cursor, 2);
}

#[test]
fn enter_on_a_trailing_backslash_continues_the_draft_instead_of_submitting() {
    let mut c = with_draft("first line\\", 11);
    assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
    assert_eq!(c.view().draft, "first line\n");
    assert_eq!(c.view().cursor, 11); // cursor to the end
}

#[test]
fn enter_on_a_trailing_backslash_continues_while_running_too() {
    let mut c = with_draft("steer me\\", 9);
    match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
        KeyOutcome::Consumed { effects, .. } => assert_eq!(effects, vec![]),
        other => panic!("expected a consumed continuation, got {other:?}"),
    }
    assert_eq!(c.view().draft, "steer me\n");
}

// Only a LITERAL trailing backslash - the LAST char of the draft -
// triggers the continuation.
#[test]
fn a_backslash_anywhere_else_still_submits() {
    let mut c = with_draft("a\\b", 3);
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Agent(AgentCommand::Submit("a\\b".into()))]
    );

    // Trailing whitespace after the backslash: the backslash is not the
    // last char, so Enter submits (trimmed).
    let mut c = with_draft("a\\ ", 3);
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Agent(AgentCommand::Submit("a\\".into()))]
    );
}

#[test]
fn enter_submits_a_multi_line_draft_whole() {
    let mut c = with_draft("first\nsecond", 12);
    assert_eq!(
        fold_consumed(&mut c, Key::Enter),
        vec![Effect::Agent(AgentCommand::Submit("first\nsecond".into()))]
    );
}

// --- Prompt history ------------------------------------------------------
//
// Ring internals (dedup, cap, the one-stash rule) are covered in
// `ui::history`; these pin the Composer's wiring - seeding, the recall
// landing spot, and the edge-triggered Up/Down on multi-line drafts.

#[test]
fn new_seeds_the_ring_oldest_first() {
    // Seeded oldest-first and parked: the first Up recalls the newest.
    let mut c = with_history(&["a", "b", "c"]);
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "c");
}

#[test]
fn arrow_up_from_empty_history_is_a_consumed_no_op() {
    let mut c = fresh();
    assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
    assert_eq!(c.view().draft, "");
}

#[test]
fn arrow_up_moves_backward_saving_draft() {
    let mut c = with_history(&["a", "b", "c"]);
    set_draft(&mut c, "typing...", 9);

    assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
    assert_eq!(c.view().draft, "c");

    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "b");

    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "a");

    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "a"); // at the oldest - a no-op
}

#[test]
fn arrow_down_from_parked_does_nothing() {
    let mut c = with_history(&["a", "b"]);
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(c.view().draft, "");
}

#[test]
fn arrow_down_moves_forward_restoring_draft_at_end() {
    let mut c = with_history(&["a", "b", "c"]);

    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "c");

    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "b");

    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(c.view().draft, "c");

    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, ""); // past the newest: the empty draft returns
}

#[test]
fn arrow_down_from_oldest_restores_draft_off_the_end() {
    let mut c = with_history(&["a", "b"]);
    set_draft(&mut c, "my draft", 8);

    press(&mut c, vec![Key::ArrowUp, Key::ArrowUp, Key::ArrowUp]);
    assert_eq!(c.view().draft, "a");

    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, "b");

    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, "my draft"); // stash restored off the end
}

// --- edge-triggered history (Up/Down on a multi-line draft) --------------

#[test]
fn arrow_up_off_the_first_line_moves_the_cursor_not_history() {
    // Cursor on the second line: Up is cursor movement, history untouched.
    let mut c = with_history(&["old"]);
    set_draft(&mut c, "ab\ncd", 4); // on 'd' (line 1, col 1)
    assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
    assert_eq!(c.view().draft, "ab\ncd"); // draft intact - no recall happened
    assert_eq!(c.view().cursor, 1); // line 0, col 1
}

#[test]
fn arrow_up_on_the_first_line_of_a_multi_line_draft_recalls_history() {
    let mut c = with_history(&["old"]);
    set_draft(&mut c, "ab\ncd", 1); // line 0
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "old");
    assert_eq!(c.view().cursor, 3); // recall puts the cursor at the end
    // The multi-line draft was stashed: Down off the recalled entry's end
    // restores it.
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, "ab\ncd");
}

#[test]
fn arrow_down_off_the_last_line_moves_the_cursor_not_history() {
    let mut c = with_history(&["old"]);
    set_draft(&mut c, "ab\ncd", 1); // line 0, col 1 - not the last line
    assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
    assert_eq!(c.view().draft, "ab\ncd"); // draft intact - cursor moved, no recall
    assert_eq!(c.view().cursor, 4); // line 1, col 1
}

#[test]
fn arrow_down_on_the_last_line_of_a_multi_line_draft_recalls_history() {
    // Recall history, then Down from the recalled entry's last line
    // restores the stashed draft - the pre-multi-line behavior.
    let mut c = with_history(&["old"]);
    set_draft(&mut c, "draft", 5);
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().draft, "old");
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().draft, "draft");
}

#[test]
fn up_and_down_clamp_the_column_to_the_target_lines_length() {
    // "long line\nab\nlonger": from the end of "longer", Up lands at the
    // end of the shorter "ab"; Up again keeps col 2 into "long line".
    let mut c = with_draft("long line\nab\nlonger", 19);
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().cursor, 12); // end of "ab" (col clamped 6 → 2)
    fold_consumed(&mut c, Key::ArrowUp);
    assert_eq!(c.view().cursor, 2); // "long line", col 2
    fold_consumed(&mut c, Key::ArrowDown);
    assert_eq!(c.view().cursor, 12); // back down: "ab" clamps col 2 → 2
}

// =======================================================================
// AT file completion (Phase C2, qwen `useAtCompletion`).
// =======================================================================

// --- the pure detection (`at_context`) ---------------------------------

#[test]
fn at_detects_a_bare_sigil_with_an_empty_pattern() {
    // "@" with the cursor after it: AT mode, empty query, span [0, 1).
    let at = at_context("@", 1).expect("in AT mode");
    assert_eq!((at.at, at.start, at.end), (0, 1, 1));
    assert_eq!(at.query, "");
}

#[test]
fn at_detects_a_partial_path_forward_to_the_end_of_line() {
    // "@src/ma" cursor at end: query is everything after the `@`.
    let at = at_context("@src/ma", 7).expect("in AT mode");
    assert_eq!(at.query, "src/ma");
    assert_eq!((at.at, at.start, at.end), (0, 1, 7));
}

#[test]
fn at_runs_the_pattern_to_the_next_unescaped_space() {
    // "@a b" cursor after 'a': the unescaped space ends the pattern at it.
    let at = at_context("@a b", 2).expect("in AT mode");
    assert_eq!(at.query, "a");
    assert_eq!(at.end, 2, "pattern ends at the space");
}

#[test]
fn at_treats_an_escaped_space_as_part_of_the_pattern() {
    // "@my\\ notes" - the backslash-escaped space is inside the pattern.
    let at = at_context("@my\\ notes", 10).expect("in AT mode");
    assert_eq!(at.query, "my\\ notes");
}

#[test]
fn an_unescaped_space_before_the_cursor_breaks_the_at_scan() {
    // "hi @a there" cursor at the very end: the space after "@a" is
    // unescaped, so scanning back from the end hits it first - no AT.
    assert_eq!(at_context("hi @a there", 11), None);
}

#[test]
fn no_at_before_the_cursor_is_no_context() {
    assert_eq!(at_context("fix the bug", 11), None);
    assert_eq!(at_context("", 0), None);
}

#[test]
fn at_is_cursor_relative_not_line_anchored() {
    // "email me @ foo" - a mid-message `@` still triggers (unlike `/`).
    let at = at_context("see @src/x", 10).expect("in AT mode");
    assert_eq!(at.query, "src/x");
    assert_eq!(at.at, 4);
}

#[test]
fn at_scans_only_the_current_line() {
    // A `@` on a PREVIOUS line does not leak into the current line's scan.
    assert_eq!(at_context("@old\nnow", 8), None);
    // But a `@` on the current (second) line is found.
    let at = at_context("first\n@b", 8).expect("in AT mode");
    assert_eq!(at.query, "b");
    assert_eq!(at.at, 6);
}

#[test]
fn at_beats_slash_after_a_command() {
    // "/model @sr" - AT takes precedence, so the `@` opens the file picker
    // even though the draft leads with a slash command.
    let at = at_context("/model @sr", 10).expect("AT beats slash");
    assert_eq!(at.query, "sr");
}

// --- the composer fold (emit FileSearch, guarded fill, accept, dismiss) -

// The open AT overlay's suggestion values, or panics.
fn at_values(c: &Composer) -> Vec<String> {
    match overlay(c) {
        Some(OverlayView::AtFiles { suggestions, .. }) => {
            suggestions.iter().map(|s| s.value.clone()).collect()
        }
        other => panic!("expected the AT picker, got {other:?}"),
    }
}

// A FileSuggestion whose label is the path and value the (already-escaped)
// path to insert.
fn file(label: &str, value: &str) -> FileSuggestion {
    FileSuggestion {
        label: label.to_string(),
        value: value.to_string(),
        matched: None,
    }
}

// The most recent Effect::FileSearch (query, generation) a fold emitted, or
// panics - a helper for the emit-on-keystroke assertions.
fn last_file_search(effects: &[Effect]) -> (String, u64) {
    effects
        .iter()
        .rev()
        .find_map(|e| match e {
            Effect::FileSearch { query, generation } => Some((query.clone(), *generation)),
            _ => None,
        })
        .expect("a FileSearch effect")
}

#[test]
fn typing_an_at_opens_the_picker_and_emits_a_file_search() {
    let mut c = fresh();
    // Typing '@' opens the AT context and fires the initial empty search.
    let effects = fold_consumed(&mut c, Key::Char('@'));
    let (query, generation) = last_file_search(&effects);
    assert_eq!(query, "", "the initial search is the empty pattern");
    assert_eq!(generation, 1);
    // The overlay is open, loading (no fill yet).
    match overlay(&c) {
        Some(OverlayView::AtFiles { loading, .. }) => assert!(loading, "loading until filled"),
        other => panic!("expected the AT picker, got {other:?}"),
    }
}

#[test]
fn each_at_keystroke_bumps_the_generation_and_re_searches() {
    let mut c = fresh();
    let g0 = last_file_search(&fold_consumed(&mut c, Key::Char('@'))).1;
    let (q1, g1) = last_file_search(&fold_consumed(&mut c, Key::Char('s')));
    assert_eq!(q1, "s");
    assert!(g1 > g0, "the generation bumps per pattern change");
}

#[test]
fn a_guarded_fill_lands_only_for_the_live_generation_and_query() {
    let mut c = fresh();
    let (_, search_gen) = last_file_search(&fold_consumed(&mut c, Key::Char('@')));
    // A fill echoing the live (generation, query="") lands.
    deliver(
        &mut c,
        Event::file_search_ready(search_gen, "", vec![file("src/main.rs", "src/main.rs")]),
    );
    assert_eq!(at_values(&c), vec!["src/main.rs"]);
    // A stale fill (wrong generation) is dropped - the rows do not change.
    deliver(
        &mut c,
        Event::file_search_ready(search_gen + 99, "", vec![file("nope.rs", "nope.rs")]),
    );
    assert_eq!(
        at_values(&c),
        vec!["src/main.rs"],
        "stale generation dropped"
    );
    // A fill for a DIFFERENT query is dropped too.
    deliver(
        &mut c,
        Event::file_search_ready(search_gen, "other", vec![file("other.rs", "other.rs")]),
    );
    assert_eq!(at_values(&c), vec!["src/main.rs"], "stale query dropped");
}

#[test]
fn enter_accepts_the_highlighted_path_escaped_with_a_trailing_space() {
    let mut c = fresh();
    let (_, search_gen) = last_file_search(&fold_consumed(&mut c, Key::Char('@')));
    // The adapter delivers an already-escaped value for a spaced path.
    deliver(
        &mut c,
        Event::file_search_ready(search_gen, "", vec![file("my notes.md", "my\\ notes.md")]),
    );
    fold_consumed(&mut c, Key::Enter);
    // The `@` span is replaced with `@<escaped> ` and the popup closes.
    assert_eq!(c.view().draft, "@my\\ notes.md ");
    assert_eq!(c.view().cursor, "@my\\ notes.md ".chars().count());
    assert_eq!(overlay(&c), None, "the picker closes on accept");
}

#[test]
fn accept_replaces_only_the_at_span_in_a_larger_message() {
    let mut c = with_draft("see @co", 7);
    // Prime a fill for the "co" pattern (the live query at cursor 7).
    let (_, search_gen) = last_file_search(&fold_consumed(&mut c, Key::Char('m')));
    deliver(
        &mut c,
        Event::file_search_ready(
            search_gen,
            "com",
            vec![file("src/composer.rs", "src/composer.rs")],
        ),
    );
    fold_consumed(&mut c, Key::Tab);
    assert_eq!(c.view().draft, "see @src/composer.rs ");
}

#[test]
fn esc_dismisses_the_picker_but_keeps_the_draft() {
    let mut c = fresh();
    fold_consumed(&mut c, Key::Char('@'));
    fold_consumed(&mut c, Key::Char('x'));
    assert!(matches!(overlay(&c), Some(OverlayView::AtFiles { .. })));
    // Esc closes the popup without clearing the `@x` draft (unlike slash).
    fold_consumed(&mut c, Key::Escape);
    assert_eq!(c.view().draft, "@x", "the draft survives the dismiss");
    assert_eq!(overlay(&c), None, "the picker is dismissed");
    // Typing re-opens it (the pattern changed).
    fold_consumed(&mut c, Key::Char('y'));
    assert!(matches!(overlay(&c), Some(OverlayView::AtFiles { .. })));
}

#[test]
fn a_dismissed_picker_stays_closed_across_a_bare_cursor_move() {
    // Esc must STICK: a cursor move that leaves the pattern unchanged does
    // not re-open a dismissed popup (only a real pattern change does).
    let mut c = fresh();
    fold_consumed(&mut c, Key::Char('@'));
    fold_consumed(&mut c, Key::Char('x'));
    fold_consumed(&mut c, Key::Escape);
    assert_eq!(overlay(&c), None, "dismissed");
    // A bare cursor move (pattern still "x") keeps it dismissed.
    fold_consumed(&mut c, Key::Left);
    assert_eq!(
        overlay(&c),
        None,
        "a cursor move does not re-open a dismissed picker"
    );
    assert_eq!(c.view().draft, "@x", "the draft is untouched");
}

#[test]
fn arrows_navigate_the_at_suggestions() {
    let mut c = fresh();
    let (_, search_gen) = last_file_search(&fold_consumed(&mut c, Key::Char('@')));
    deliver(
        &mut c,
        Event::file_search_ready(
            search_gen,
            "",
            vec![file("a.rs", "a.rs"), file("b.rs", "b.rs")],
        ),
    );
    // Down moves the highlight; accepting picks the second row.
    fold_consumed(&mut c, Key::ArrowDown);
    fold_consumed(&mut c, Key::Enter);
    assert_eq!(c.view().draft, "@b.rs ");
}

#[test]
fn at_beats_slash_in_the_fold_too() {
    // A draft leading with `/` but with an `@` before the cursor folds
    // through the AT path, not the slash menu.
    let mut c = with_draft("/model ", 7);
    let effects = fold_consumed(&mut c, Key::Char('@'));
    // The `@` opened an AT search (slash would not emit a FileSearch).
    assert_eq!(last_file_search(&effects).0, "");
    assert!(matches!(overlay(&c), Some(OverlayView::AtFiles { .. })));
}

// =======================================================================
// Layout math.
// =======================================================================

fn rows(value: &str, width: usize) -> Vec<String> {
    layout(value, 0, width).rows
}

fn cursor(value: &str, cursor: usize, width: usize) -> (usize, usize) {
    let l = layout(value, cursor, width);
    (l.cursor_row, l.cursor_col)
}

// --- wrapping ------------------------------------------------------------

#[test]
fn an_empty_draft_is_one_empty_row_with_the_cursor_at_the_origin() {
    let l = layout("", 0, 10);
    assert_eq!(l.rows, vec![String::new()]);
    assert_eq!((l.cursor_row, l.cursor_col), (0, 0));
}

#[test]
fn a_short_draft_is_a_single_row() {
    assert_eq!(rows("hello", 10), vec!["hello"]);
}

#[test]
fn hard_newlines_split_rows_and_empty_lines_survive_as_blank_rows() {
    assert_eq!(rows("a\n\nb", 10), vec!["a", "", "b"]);
}

#[test]
fn a_long_line_wraps_at_the_width_char_by_char() {
    assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
}

#[test]
fn an_exact_multiple_line_gains_the_empty_row_the_cursor_lands_on() {
    // "abcd" at width 4 fills its row exactly; the cursor at its end (4)
    // needs the next row's first cell, so that row exists.
    assert_eq!(rows("abcd", 4), vec!["abcd", ""]);
    assert_eq!(cursor("abcd", 4, 4), (1, 0));
}

#[test]
fn hard_newlines_and_wrapping_compose() {
    assert_eq!(rows("abcdef\nxy", 4), vec!["abcd", "ef", "xy"]);
}

#[test]
fn multibyte_chars_wrap_by_char_count_not_bytes() {
    assert_eq!(rows("héllo wörld", 6), vec!["héllo ", "wörld"]);
    assert_eq!(cursor("héllo wörld", 11, 6), (1, 5));
}

#[test]
fn a_degenerate_zero_width_is_treated_as_one() {
    assert_eq!(rows("ab", 0), vec!["a", "b", ""]);
}

// --- cursor position -------------------------------------------------------

#[test]
fn the_cursor_at_start_middle_and_end_of_one_row() {
    assert_eq!(cursor("hello", 0, 10), (0, 0));
    assert_eq!(cursor("hello", 3, 10), (0, 3));
    assert_eq!(cursor("hello", 5, 10), (0, 5));
}

#[test]
fn the_cursor_on_a_wrapped_continuation_row() {
    // "abcdefghij" at width 4: rows abcd / efgh / ij.
    assert_eq!(cursor("abcdefghij", 4, 4), (1, 0));
    assert_eq!(cursor("abcdefghij", 7, 4), (1, 3));
    assert_eq!(cursor("abcdefghij", 10, 4), (2, 2));
}

#[test]
fn the_cursor_on_hard_newline_rows() {
    // "ab\ncd\nef": the cursor ON the '\n' is the end of the line before.
    assert_eq!(cursor("ab\ncd\nef", 2, 10), (0, 2));
    assert_eq!(cursor("ab\ncd\nef", 3, 10), (1, 0));
    assert_eq!(cursor("ab\ncd\nef", 5, 10), (1, 2));
    assert_eq!(cursor("ab\ncd\nef", 8, 10), (2, 2));
}

#[test]
fn a_wide_draft_of_hard_lines_and_wraps_places_the_cursor_exactly() {
    // width 4: "abcdef" wraps to abcd/ef (rows 0-1), "" is row 2,
    // "ghijklm" wraps to ghij/klm (rows 3-4).
    let value = "abcdef\n\nghijklm";
    assert_eq!(rows(value, 4), vec!["abcd", "ef", "", "ghij", "klm"]);
    assert_eq!(cursor(value, 6, 4), (1, 2)); // end of "abcdef"
    assert_eq!(cursor(value, 7, 4), (2, 0)); // the empty line
    assert_eq!(cursor(value, 12, 4), (4, 0)); // start of "klm"
    assert_eq!(cursor(value, 15, 4), (4, 3)); // end of the draft
}

#[test]
fn the_cursor_column_is_always_inside_the_width() {
    for cur in 0..=12 {
        let l = layout("abcd\nefghijkl", cur, 4);
        assert!(
            l.cursor_col < 4,
            "cursor {cur} produced col {}",
            l.cursor_col
        );
        assert!(l.cursor_row < l.rows.len());
    }
}

#[test]
fn a_cursor_past_the_draft_clamps_to_the_end() {
    assert_eq!(cursor("hi", 99, 10), (0, 2));
}

// --- edit/render agreement ------------------------------------------------
//
// The property this whole extraction exists to pin: the render path
// (`layout`) and the edit path (`ui::draft`) resolve the cursor to the
// SAME cell for the same `(draft, cursor)`. At a width wide enough that
// every hard line fits in one row, a `layout` cell maps back to logical
// geometry exactly: `cursor_row` IS the logical line and `cursor_col` the
// column. That is `draft::line_col` - and running it back through
// `draft::cursor_at` must return the original cursor.

fn agree(value: &str) {
    // Width past the longest hard line, so one row == one logical line.
    let width = value.chars().count() + 1;
    let chars = value.chars().count();
    for cursor in 0..=chars {
        let l = layout(value, cursor, width);
        // Render side, read as (logical line, column).
        let (render_line, render_col) = (l.cursor_row, l.cursor_col);
        // Edit side, from the shared owner.
        let (edit_line, edit_col) = draft::line_col(value, cursor);
        assert_eq!(
            (render_line, render_col),
            (edit_line, edit_col),
            "render/edit cursor disagree for value={value:?} cursor={cursor}"
        );
        // And the shared inverse returns exactly where we started.
        assert_eq!(
            draft::cursor_at(value, edit_line, edit_col),
            cursor,
            "cursor_at round trip failed for value={value:?} cursor={cursor}"
        );
    }
}

#[test]
fn render_and_edit_agree_on_an_empty_draft() {
    agree("");
}

#[test]
fn render_and_edit_agree_with_the_cursor_at_the_end() {
    agree("hello");
}

#[test]
fn render_and_edit_agree_across_a_trailing_hard_newline() {
    agree("hello\n");
}

#[test]
fn render_and_edit_agree_across_consecutive_newlines() {
    agree("a\n\n\nb");
}

#[test]
fn render_and_edit_agree_on_an_empty_line_between_two_newlines() {
    // The cursor sitting alone on the blank middle line.
    let value = "a\n\nb";
    let empty_line_cursor = draft::cursor_at(value, 1, 0);
    let l = layout(value, empty_line_cursor, 10);
    assert_eq!((l.cursor_row, l.cursor_col), (1, 0));
    assert_eq!(draft::line_col(value, empty_line_cursor), (1, 0));
    agree(value);
}

#[test]
fn render_and_edit_agree_on_multibyte_chars() {
    // Char index 2 sits after "hé" (é is two bytes): both sides must read
    // that in chars, not bytes.
    agree("héllo wörld");
    let l = layout("héllo", 2, 10);
    assert_eq!((l.cursor_row, l.cursor_col), (0, 2));
    assert_eq!(draft::line_col("héllo", 2), (0, 2));
}

#[test]
fn render_and_edit_agree_when_a_wrapped_row_carries_the_cursor() {
    // Narrow width forces wrapping: the render side divides the logical
    // column into row/col, but the logical column it divides is still the
    // one the edit side would compute.
    let value = "abcdefghij\nkl";
    let chars = value.chars().count();
    for cursor in 0..=chars {
        let l = layout(value, cursor, 4);
        let (line, col) = draft::line_col(value, cursor);
        // The visual col must be the logical col modulo the width, and the
        // visual row must advance by the logical col / width beyond the
        // line's base row.
        assert_eq!(l.cursor_col, col % 4);
        assert!(l.cursor_col < 4);
        // Round-trips through the shared inverse regardless of wrapping.
        assert_eq!(draft::cursor_at(value, line, col), cursor);
    }
}

// --- height cap and internal scroll ---------------------------------------

#[test]
fn max_visible_rows_is_a_third_of_the_terminal_capped_at_eight() {
    assert_eq!(max_visible_rows(60), 8); // 60/3 = 20, capped
    assert_eq!(max_visible_rows(24), 8); // 24/3 = 8, exactly the cap
    assert_eq!(max_visible_rows(12), 4);
    assert_eq!(max_visible_rows(3), 1);
    assert_eq!(max_visible_rows(0), 1); // never starves to zero
}

#[test]
fn first_visible_row_pins_the_cursor_to_the_bottom_of_the_box() {
    assert_eq!(first_visible_row(9, 4), 6); // cursor on the box's last row
    assert_eq!(first_visible_row(6, 4), 3);
}

#[test]
fn first_visible_row_shows_the_top_while_the_cursor_fits() {
    assert_eq!(first_visible_row(0, 4), 0);
    assert_eq!(first_visible_row(3, 4), 0);
    assert_eq!(first_visible_row(0, 0), 0); // degenerate box
}
