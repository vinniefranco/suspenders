
use super::*;

const NOW: Millis = 1_000_000;

fn no_recent(_: &str) -> Option<RecentUse> {
    None
}

fn labels(s: &[Suggestion]) -> Vec<String> {
    s.iter().map(|x| x.label.clone()).collect()
}

fn values(s: &[Suggestion]) -> Vec<String> {
    s.iter().map(|x| x.value.clone()).collect()
}

// --- empty query: recency then comparator ------------------------------

#[test]
fn an_empty_query_lists_every_command_shortest_first() {
    // With no query every command is an equal (Fuzzy) match, so the ladder's
    // tiebreak decides: the SHORTER item first (`item_length`), then registry
    // order. `mcp` (3) precedes `model`/`theme` (5), which keep registry order.
    let s = rank("", &no_recent, NOW);
    assert_eq!(values(&s), vec!["mcp", "model", "theme"]);
}

#[test]
fn recency_floats_a_used_command_to_the_top_of_the_empty_query() {
    // "theme" used recently outranks the rest despite being longest: recency
    // sits ABOVE the length/registry tiebreak. The unused pair keeps the
    // shortest-first order (`mcp` before `model`).
    let recent = |name: &str| {
        (name == "theme").then_some(RecentUse {
            count: 3,
            used_at: NOW,
        })
    };
    let s = rank("", &recent, NOW);
    assert_eq!(values(&s), vec!["theme", "mcp", "model"], "recent-first");
}

// --- the strength ladder (the PRIMARY sort key) ------------------------

#[test]
fn exact_beats_prefix_beats_fuzzy() {
    assert_eq!(strength("model", "model", 0), MatchStrength::Exact);
    assert_eq!(strength("model", "mod", 0), MatchStrength::Prefix);
    assert_eq!(strength("model", "mdl", 0), MatchStrength::Fuzzy);
    // Ladder order is exact > prefix > segment > fuzzy.
    assert!(MatchStrength::Exact > MatchStrength::Prefix);
    assert!(MatchStrength::Prefix > MatchStrength::SegmentPrefix);
    assert!(MatchStrength::SegmentPrefix > MatchStrength::Fuzzy);
}

#[test]
fn a_segment_prefix_is_a_match_at_an_interior_boundary() {
    // "haiku" prefixes "claude-haiku" at the '-' boundary (start 7).
    assert_eq!(
        strength("claude-haiku", "haiku", 7),
        MatchStrength::SegmentPrefix
    );
    // Same query mid-word (not at a boundary) is only FUZZY.
    assert_eq!(strength("claudehaiku", "haiku", 6), MatchStrength::Fuzzy);
}

// --- canonical orderings (pinned) --------------------------------------

#[test]
fn m_ranks_the_prefix_matches_first_shortest_first() {
    // "/m" prefixes BOTH "mcp" and "model" (PREFIX) and fuzzy-matches "theme"
    // (the 'm' in "theme"): all three surface, but the two PREFIX matches
    // float above the fuzzy one, and between the equal-strength prefixes the
    // shorter item wins the ladder tiebreak - "mcp" (3) before "model" (5).
    let s = rank("m", &no_recent, NOW);
    assert_eq!(values(&s), vec!["mcp", "model", "theme"]);
    assert_eq!(s[0].matched, Some((0, 1)), "the leading 'm' is highlighted");
    // "/mod" prefixes only "model"; "mcp"/"theme" have no 'o'/'d' run.
    assert_eq!(values(&rank("mod", &no_recent, NOW)), vec!["model"]);
}

#[test]
fn the_value_inserted_is_always_the_canonical_name() {
    let s = rank("model", &no_recent, NOW);
    assert_eq!(values(&s), vec!["model"]);
    assert_eq!(labels(&s), vec!["model"]);
}

#[test]
fn a_query_matching_nothing_yields_no_suggestions() {
    assert!(rank("zzzz", &no_recent, NOW).is_empty());
}

#[test]
fn the_match_window_is_the_contiguous_prefix_span() {
    // "/mod" prefixes "model": the inverted window is [0, 3).
    let s = rank("mod", &no_recent, NOW);
    assert_eq!(s[0].matched, Some((0, 3)));
}

// --- nav (wrap, clamp, scroll window) ----------------------------------

#[test]
fn nav_wraps_in_both_directions() {
    let mut c = Completion::new();
    assert_eq!(c.active(), 0);
    c.up(3);
    assert_eq!(c.active(), 2, "up from the top wraps to the last");
    c.down(3);
    assert_eq!(c.active(), 0, "down from the last wraps to the first");
    c.down(3);
    assert_eq!(c.active(), 1);
}

#[test]
fn clamp_pulls_the_active_row_into_a_shrunk_list() {
    let mut c = Completion::new();
    c.down(5);
    c.down(5);
    assert_eq!(c.active(), 2);
    c.clamp(1);
    assert_eq!(c.active(), 0);
}

#[test]
fn the_scroll_window_follows_the_active_row_past_the_cap() {
    // A list longer than the cap: moving past the window slides the scroll.
    let len = MAX_SUGGESTIONS_TO_SHOW + 4;
    let mut c = Completion::new();
    assert_eq!(c.scroll(), 0);
    for _ in 0..MAX_SUGGESTIONS_TO_SHOW {
        c.down(len);
    }
    // Active is now at index 8 (the 9th), which must be inside the window.
    assert_eq!(c.active(), MAX_SUGGESTIONS_TO_SHOW);
    assert!(c.active() >= c.scroll());
    assert!(c.active() < c.scroll() + MAX_SUGGESTIONS_TO_SHOW);
}

#[test]
fn nav_on_an_empty_list_is_inert() {
    let mut c = Completion::new();
    c.down(0);
    c.up(0);
    c.clamp(0);
    assert_eq!(c.active(), 0);
    assert_eq!(c.scroll(), 0);
}

// --- expand (`←/→`, qwen expandedIndex) --------------------------------

#[test]
fn expand_toggles_only_the_active_row_and_collapse_clears_it() {
    let mut c = Completion::new();
    assert!(!c.active_expanded(), "fresh palette is collapsed");
    c.expand();
    assert!(c.active_expanded(), "→ expands the active row");
    // Moving the highlight off the expanded row un-expands the VIEW (the
    // render keys expanded on the active row), without a separate reset.
    c.down(3);
    assert!(
        !c.active_expanded(),
        "a different active row is not expanded"
    );
    // Moving back re-shows the expansion (qwen keeps the index).
    c.up(3);
    assert!(c.active_expanded(), "the original row is still expanded");
    c.collapse();
    assert!(!c.active_expanded(), "← collapses it");
}

// --- the comparator (qwen compareRankedCommandMatches), directly --------

// A neutral hand-built Ranked over the first registry command: FUZZY
// strength, everything zeroed. Tests set the ONE ladder field they pin via
// struct update (`Ranked { field, ..base() }`), so each test reads as the
// single tier it exercises.
fn base() -> Ranked {
    Ranked {
        command: &slash::COMMANDS[0],
        matched_value: slash::COMMANDS[0].name.to_string(),
        strength: MatchStrength::Fuzzy,
        completion_priority: 0,
        recent_score: 0.0,
        score: 0,
        start: 0,
        item_length: 5,
        original_index: 0,
        matched: None,
        is_alias: false,
    }
}

// Whether `a` sorts BEFORE `b` under the comparator (a ranks higher).
fn ranks_before(a: &Ranked, b: &Ranked) -> bool {
    compare_ranked(a, b).is_lt()
}

#[test]
fn strength_dominates_score() {
    // A strong strength with a LOW score beats a weak strength with a HIGH
    // score - strength is the PRIMARY key, score only a fuzzy-tier break.
    let strong_low = Ranked {
        strength: MatchStrength::Prefix,
        score: 1,
        ..base()
    };
    let weak_high = Ranked {
        strength: MatchStrength::Fuzzy,
        score: 999,
        ..base()
    };
    assert!(ranks_before(&strong_low, &weak_high));
}

#[test]
fn completion_priority_sits_between_strength_and_recency() {
    // Same strength: the higher completion_priority wins even against more
    // recency (priority is ABOVE recency).
    let hi_priority = Ranked {
        completion_priority: 5,
        ..base()
    };
    let recent = Ranked {
        recent_score: 100.0,
        ..base()
    };
    assert!(
        ranks_before(&hi_priority, &recent),
        "priority beats recency"
    );
    // But a stronger strength still beats a high priority (priority BELOW
    // strength).
    let stronger = Ranked {
        strength: MatchStrength::Prefix,
        ..base()
    };
    assert!(
        ranks_before(&stronger, &hi_priority),
        "strength beats priority"
    );
}

#[test]
fn recency_breaks_a_strength_tie_and_sits_above_score() {
    // Same strength + priority: more recency wins even with a lower score.
    let recent_lowscore = Ranked {
        recent_score: 50.0,
        score: 1,
        ..base()
    };
    let stale_highscore = Ranked {
        recent_score: 0.0,
        score: 999,
        ..base()
    };
    assert!(ranks_before(&recent_lowscore, &stale_highscore));
}

#[test]
fn score_is_only_a_fuzzy_tier_tiebreak() {
    // Everything above score equal: the higher score wins, and NOTHING
    // below it (start/length/index) can override a score difference.
    let hi = Ranked {
        score: 10,
        start: 9,
        item_length: 9,
        original_index: 9,
        ..base()
    };
    let lo = Ranked { score: 5, ..base() };
    assert!(ranks_before(&hi, &lo), "higher score wins the fuzzy tier");
}

#[test]
fn recency_decay_responds_to_the_injected_clock() {
    let recent = |name: &str| {
        (name == "model").then_some(RecentUse {
            count: 0,
            used_at: 1_000_000,
        })
    };
    // Age 0 (now == used_at): full freshness = 10 * 1.0 = 10.
    assert_eq!(recent_score(&recent, "model", 1_000_000), 10.0);
    // Age exactly the decay window: freshness floors at 0.
    assert_eq!(
        recent_score(&recent, "model", 1_000_000 + RECENT_DECAY_MS),
        0.0
    );
    // Age PAST the window: the .max(0) floor holds (never negative).
    assert_eq!(
        recent_score(&recent, "model", 1_000_000 + RECENT_DECAY_MS * 3),
        0.0,
        "the freshness floor is 0, not negative"
    );
    // Halfway through the window: freshness = 0.5 → 5.0.
    assert_eq!(
        recent_score(&recent, "model", 1_000_000 + RECENT_DECAY_MS / 2),
        5.0
    );
    // No record: 0.
    assert_eq!(recent_score(&no_recent, "model", 1_000_000), 0.0);
}

// --- the prefix fallback (the matcher-error seam) ----------------------

// `rank` reaches `prefix_ranked` only if `fuzzy_ranked` returns `None` (a
// whole-pass matcher failure, which nucleo never produces in practice), so
// the fallback is exercised DIRECTLY here rather than left as a dead seam:
// it is the crash-safety net (a matcher error must still show prefix
// matches, never an empty palette), so it stays - and is tested.
#[test]
fn the_prefix_fallback_keeps_prefix_matches_with_the_ladder() {
    let recent_of = |_: &str| 0.0;
    // "mod" prefixes only "model".
    let mut out = prefix_ranked("mod", &recent_of);
    out.sort_by(compare_ranked);
    let vals: Vec<_> = out
        .into_iter()
        .map(to_suggestion)
        .map(|s| s.value)
        .collect();
    assert_eq!(vals, vec!["model"]);
    // An exact name still ranks EXACT (the ladder holds in the fallback).
    let exact = prefix_ranked("model", &recent_of);
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].strength, MatchStrength::Exact);
    // A non-prefix query yields nothing (prefix-only, no fuzzy).
    assert!(prefix_ranked("dl", &recent_of).is_empty());
}

// --- rank_paths (the AT file picker's pure ranking) --------------------

fn paths(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn an_empty_query_keeps_every_path_in_the_given_order() {
    // qwen searches '' and gets the searcher's natural order back; the walk
    // is already sorted, so rank_paths keeps it verbatim, nothing highlighted.
    let all = paths(&["README.md", "src/main.rs", "src/ui/composer.rs"]);
    let out = rank_paths(&all, "");
    assert_eq!(values(&out), all);
    assert!(out.iter().all(|s| s.matched.is_none()));
    // label == value for every AT row (the whole path is both).
    assert!(out.iter().all(|s| s.label == s.value));
}

#[test]
fn a_query_fuzzy_orders_paths_best_first_and_drops_non_matches() {
    let all = paths(&[
        "README.md",
        "src/main.rs",
        "src/ui/composer.rs",
        "docs/adr/0001.md",
    ]);
    let out = rank_paths(&all, "composer");
    // Only the composer path fuzzy-matches "composer".
    assert_eq!(values(&out), vec!["src/ui/composer.rs"]);
    // A query matching nothing yields no rows (a narrowing filter shrinks).
    assert!(rank_paths(&all, "zzznope").is_empty());
}

#[test]
fn the_match_window_spans_the_fuzzy_hit_for_the_highlight() {
    let all = paths(&["src/main.rs"]);
    let out = rank_paths(&all, "main");
    assert_eq!(out.len(), 1);
    // "main" sits at chars [4, 8) of "src/main.rs" - the inverted window.
    assert_eq!(out[0].matched, Some((4, 8)));
}

#[test]
fn a_stronger_fuzzy_hit_ranks_before_a_scattered_one() {
    // "srcmain" contiguously-ish hits "src/main.rs" better than the
    // scattered hit in "docs/some-readme.rs"; the tighter match sorts first.
    let all = paths(&["docs/some-readme.rs", "src/main.rs"]);
    let out = rank_paths(&all, "srcmain");
    assert_eq!(out[0].value, "src/main.rs");
}
