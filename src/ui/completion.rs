//! The fuzzy `/` command palette - System B (ADR-0051, qwen
//! `useSlashCompletion.ts` + `useCompletion.ts` nav + `SuggestionsDisplay.tsx`).
//! This is the OTHER selection system: distinct from the numbered `›` dialogs
//! ([`crate::ui::selection`], System A), the `/` popup is fuzzy, color-only (no
//! marker, no numbers), and driven by the draft text as its filter.
//!
//! PURE (ADR-0019): no ratatui, no crossterm, no wall clock. The one dependency
//! that touches the outside world is `nucleo-matcher`, confined to this module's
//! [`rank`] and surfaced only as a score and match indices - the public
//! [`Suggestion`]/[`Completion`] types return `String`/`Vec<usize>`, never a
//! matcher type. `now: Millis` is INJECTED (the recency decay's clock), the same
//! seam [`crate::ui::selection`] uses for its digit timeout.
//!
//! ## The rank ladder (qwen `getCommandMatchStrength`, verbatim)
//!
//! A non-empty query ranks each `(command, matched-value)` pair - the value is
//! the command name or one of its `alt_names` - by an explicit STRENGTH ladder,
//! the PRIMARY sort key: EXACT (value == query) > PREFIX (value starts with
//! query) > SEGMENT_PREFIX (query prefixes the value at a `-`/`_`/`/`/space
//! boundary) > FUZZY. Below strength sits the command's static
//! `completion_priority` (qwen `completionPriority`, a ranking nudge), then
//! recency (a decaying use count), then the nucleo score (the FUZZY-tier
//! tiebreak), then `start`/length/original-index - qwen's
//! `compareRankedCommandMatches` order, ported field for field.
//! An empty query ranks by recency then that same comparator (so the recently
//! used surface first, all at FUZZY strength with score 0).
//!
//! The matcher's fuzzy pass is a superset of prefix, so a matcher error (or the
//! empty pattern) falls back to a PREFIX-only filter - never a crash, never an
//! empty palette when a prefix would have matched.

use crate::ui::selection::Millis;
use crate::ui::slash::{self, SlashCommand};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// qwen `RECENT_DECAY_MS`: a use's recency contribution decays linearly to zero
/// over this window (10 minutes).
pub const RECENT_DECAY_MS: Millis = 10 * 60 * 1000;

/// The most suggestion rows the palette shows before it scrolls (qwen
/// `MAX_SUGGESTIONS_TO_SHOW`).
pub const MAX_SUGGESTIONS_TO_SHOW: usize = 8;

/// The label width past which a row is "long" (qwen `PrepareLabel.MAX_WIDTH`):
/// a long row on the active line collapses to a truncated window with a ` → `
/// affordance until `←/→` expands it to the full label (` ← `). Below this a
/// row always shows in full and `←/→` are plain cursor moves.
pub const MAX_WIDTH: usize = 150;

/// The strength ladder (qwen `CommandMatchStrength`), the PRIMARY sort key.
/// Ordered so a plain `>=`/`Ord` derive sorts weakest-first; the comparator
/// sorts DESCENDING on it (strongest first). Values mirror qwen's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchStrength {
    /// No prefix relationship - a scattered fuzzy hit (qwen `FUZZY = 0`).
    Fuzzy,
    /// The query prefixes the value at an interior segment boundary
    /// (`-`/`_`/`/`/space) (qwen `SEGMENT_PREFIX = 1`).
    SegmentPrefix,
    /// The value starts with the query (qwen `PREFIX = 2`).
    Prefix,
    /// The value equals the query, case-insensitively (qwen `EXACT = 3`).
    Exact,
}

/// A recent-use record for one command (qwen `RecentSlashCommand`): how many
/// times it was used and when it was last used, in host `Millis`. The palette
/// host keeps a `name -> RecentUse` map; [`rank`] reads it to bias the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecentUse {
    pub count: u32,
    pub used_at: Millis,
}

/// One palette row for rendering (qwen `Suggestion`): the display `label` (the
/// name, or the matched alias when an alt name ranked best), the `value` to
/// insert (always the command's canonical name), the one-line `description`,
/// and `matched` - the match window `[start..end)` in char indices over
/// `label`, for the inverted-substring highlight (`None` when the query was
/// empty or the label carries no contiguous match). Plain owned data: no
/// matcher type crosses this seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub value: String,
    pub description: String,
    /// The `[start, end)` char range of the match within `label`, collapsed
    /// from nucleo's (possibly non-contiguous) indices to the min..=max window
    /// (qwen renders one inverted substring, `PrepareLabel`). `None` when there
    /// is nothing to highlight.
    pub matched: Option<(usize, usize)>,
}

// The internal ranked match, before it collapses to a [`Suggestion`] - qwen
// `RankedCommandMatch`. The comparator sorts a `Vec` of these.
#[derive(Debug, Clone)]
struct Ranked {
    command: &'static SlashCommand,
    /// The command name or the alt name that matched (qwen `matchedValue`).
    matched_value: String,
    strength: MatchStrength,
    /// The command's static ranking nudge (qwen `completionPriority`): sorts
    /// DESC, ABOVE recency and below strength.
    completion_priority: i32,
    recent_score: f64,
    /// nucleo's fuzzy score (0 for the empty-query pass); the FUZZY-tier
    /// tiebreak.
    score: u16,
    /// The match's start index in `matched_value` (qwen `start`).
    start: usize,
    /// The matched value's length (qwen `itemLength`).
    item_length: usize,
    /// The command's index in the registry (qwen `originalIndex`) - the last,
    /// stable tiebreak.
    original_index: usize,
    /// The contiguous match window over `matched_value`, or `None`.
    matched: Option<(usize, usize)>,
    /// Whether `matched_value` is an alt name (so the label shows it).
    is_alias: bool,
}

/// The fuzzy `/` palette state (System B): the highlighted suggestion and the
/// render-scroll window offset. The suggestions themselves are DERIVED from the
/// draft filter each frame ([`rank`]), so this holds only the cursor and the
/// scroll - the same "draft is the one filter" shape as the old menu. Pure and
/// `Eq`, so a host holding one stays comparable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    active: usize,
    /// The first visible suggestion index (qwen `scrollOffset`); kept so the
    /// active row stays inside the [`MAX_SUGGESTIONS_TO_SHOW`] window as it
    /// moves.
    scroll: usize,
    /// The suggestion index the user has expanded (`←/→`, qwen
    /// `expandedSuggestionIndex`), or `None` when none is expanded. A long
    /// active row (label chars `>= MAX_WIDTH`) shows collapsed unless it IS the
    /// expanded index; the render only ever honours it on the active row.
    expanded: Option<usize>,
}

impl Default for Completion {
    fn default() -> Self {
        Completion::new()
    }
}

impl Completion {
    /// A fresh palette: the first suggestion active, scrolled to the top,
    /// nothing expanded.
    pub fn new() -> Self {
        Completion {
            active: 0,
            scroll: 0,
            expanded: None,
        }
    }

    /// The highlighted suggestion index (into the ranked list). Clamp against
    /// the current suggestion count at the call site - a shrinking filter can
    /// leave it past the end until the next [`Completion::clamp`].
    pub fn active(&self) -> usize {
        self.active
    }

    /// The first visible suggestion index (the scroll window's top).
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Whether the active row is currently expanded (qwen `expandedIndex ===
    /// activeIndex`). The render collapses a long active row unless this is
    /// `true`.
    pub fn active_expanded(&self) -> bool {
        self.expanded == Some(self.active)
    }

    /// Expands the active row (qwen `EXPAND_SUGGESTION` / `→`). The caller only
    /// calls this when the active row is long (label `>= MAX_WIDTH`); otherwise
    /// `→` is a plain cursor move.
    pub fn expand(&mut self) {
        self.expanded = Some(self.active);
    }

    /// Collapses the expanded row (qwen `COLLAPSE_SUGGESTION` / `←`). The
    /// caller only calls this when the active row is long.
    pub fn collapse(&mut self) {
        self.expanded = None;
    }

    /// Re-clamps the active row and the scroll window to a `len`-long
    /// suggestion list (call after the filter changed the count): the active
    /// row saturates at the last suggestion, and the scroll window slides so
    /// the active row is inside `[scroll, scroll + MAX_SUGGESTIONS_TO_SHOW)`.
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.active = 0;
            self.scroll = 0;
            return;
        }
        self.active = self.active.min(len - 1);
        self.scroll_into_view(len);
    }

    /// Moves the highlight up one suggestion, wrapping to the last (qwen
    /// `useCompletion` nav wraps), against a `len`-long list.
    pub fn up(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.active = (self.active + len - 1) % len;
        self.scroll_into_view(len);
    }

    /// Moves the highlight down one suggestion, wrapping to the first.
    pub fn down(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        self.active = (self.active + 1) % len;
        self.scroll_into_view(len);
    }

    // Slides the scroll window so `active` is inside it (qwen keeps
    // `scrollOffset` in range as the active index moves).
    fn scroll_into_view(&mut self, len: usize) {
        self.active = self.active.min(len.saturating_sub(1));
        if self.active < self.scroll {
            self.scroll = self.active;
        } else if self.active >= self.scroll + MAX_SUGGESTIONS_TO_SHOW {
            self.scroll = self.active + 1 - MAX_SUGGESTIONS_TO_SHOW;
        }
        // Never scroll past what fills the window from the bottom.
        let max_scroll = len.saturating_sub(MAX_SUGGESTIONS_TO_SHOW);
        self.scroll = self.scroll.min(max_scroll);
    }
}

/// Ranks the registry against `query` (the command token typed after `/`, no
/// leading slash), newest-first by qwen's ladder (see the module doc). `recent`
/// maps a command name to its use record for the recency bias; `now` is the
/// host clock the decay reads. An empty `query` returns every command ordered
/// by recency then the comparator. A matcher failure falls back to a
/// prefix-only filter over name + alt names.
///
/// The returned [`Suggestion`]s are ready to render: `value` is the canonical
/// name to insert, `label` is the best-matching value (an alias when one
/// ranked best), `matched` is the inverted-highlight window over `label`.
pub fn rank(
    query: &str,
    recent: &dyn Fn(&str) -> Option<RecentUse>,
    now: Millis,
) -> Vec<Suggestion> {
    let recent_of = |name: &str| recent_score(recent, name, now);
    let mut ranked: Vec<Ranked> = if query.is_empty() {
        empty_query_ranked(&recent_of)
    } else {
        fuzzy_ranked(query, &recent_of).unwrap_or_else(|| prefix_ranked(query, &recent_of))
    };
    ranked.sort_by(compare_ranked);
    ranked.into_iter().map(to_suggestion).collect()
}

// The empty-query pass (qwen `partial === ''`): every command at FUZZY
// strength, score 0, matching on its canonical name, ordered later by recency.
fn empty_query_ranked(recent_of: &dyn Fn(&str) -> f64) -> Vec<Ranked> {
    slash::COMMANDS
        .iter()
        .enumerate()
        .map(|(index, command)| Ranked {
            command,
            matched_value: command.name.to_string(),
            strength: MatchStrength::Fuzzy,
            completion_priority: command.completion_priority,
            recent_score: recent_of(command.name),
            score: 0,
            start: 0,
            item_length: command.name.chars().count(),
            original_index: index,
            matched: None,
            is_alias: false,
        })
        .collect()
}

// The fuzzy pass (qwen fzf): rank every candidate value (name + alt names) with
// nucleo, keep the best-ranked value per command. `None` only if the matcher is
// unusable for the whole pass (never in practice) - the caller then falls back
// to prefix. An empty result (no value matched anything) is a valid `Some`.
fn fuzzy_ranked(query: &str, recent_of: &dyn Fn(&str) -> f64) -> Option<Vec<Ranked>> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut best: Vec<Ranked> = Vec::new();
    for (index, command) in slash::COMMANDS.iter().enumerate() {
        let mut command_best: Option<Ranked> = None;
        for (value, is_alias) in candidate_values(command) {
            if let Some((score, matched)) = fuzzy_one(&mut matcher, &value, query) {
                let start = matched.map(|(s, _)| s).unwrap_or(0);
                let candidate = Ranked {
                    command,
                    strength: strength(&value, query, start),
                    completion_priority: command.completion_priority,
                    recent_score: recent_of(command.name),
                    score,
                    start,
                    item_length: value.chars().count(),
                    original_index: index,
                    matched,
                    is_alias,
                    matched_value: value,
                };
                // Keep the better-ranked value for this command (qwen keeps the
                // min under the comparator).
                if command_best
                    .as_ref()
                    .is_none_or(|existing| compare_ranked(&candidate, existing).is_lt())
                {
                    command_best = Some(candidate);
                }
            }
        }
        if let Some(r) = command_best {
            best.push(r);
        }
    }
    Some(best)
}

// The prefix fallback (qwen `getPrefixSuggestions`): keep every command a value
// case-insensitively prefixes, ranked by the same ladder, score 0.
fn prefix_ranked(query: &str, recent_of: &dyn Fn(&str) -> f64) -> Vec<Ranked> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for (index, command) in slash::COMMANDS.iter().enumerate() {
        let mut command_best: Option<Ranked> = None;
        for (value, is_alias) in candidate_values(command) {
            if value.to_lowercase().starts_with(&needle) {
                let candidate = Ranked {
                    command,
                    strength: strength(&value, query, 0),
                    completion_priority: command.completion_priority,
                    recent_score: recent_of(command.name),
                    score: 0,
                    start: 0,
                    item_length: value.chars().count(),
                    original_index: index,
                    matched: Some((0, query.chars().count())),
                    is_alias,
                    matched_value: value,
                };
                if command_best
                    .as_ref()
                    .is_none_or(|existing| compare_ranked(&candidate, existing).is_lt())
                {
                    command_best = Some(candidate);
                }
            }
        }
        if let Some(r) = command_best {
            out.push(r);
        }
    }
    out
}

// The value candidates for a command: its canonical name (not an alias) then
// each alt name (qwen matches name + altNames).
fn candidate_values(command: &SlashCommand) -> Vec<(String, bool)> {
    let mut values = vec![(command.name.to_string(), false)];
    values.extend(command.alt_names.iter().map(|a| (a.to_string(), true)));
    values
}

// One nucleo fuzzy match: the score and the collapsed contiguous match window
// (min..=max of the returned indices), or `None` when the value does not match.
fn fuzzy_one(
    matcher: &mut Matcher,
    value: &str,
    query: &str,
) -> Option<(u16, Option<(usize, usize)>)> {
    let mut hay_buf = Vec::new();
    let mut needle_buf = Vec::new();
    let haystack = Utf32Str::new(value, &mut hay_buf);
    let needle = Utf32Str::new(query, &mut needle_buf);
    let mut indices: Vec<u32> = Vec::new();
    let score = matcher.fuzzy_indices(haystack, needle, &mut indices)?;
    // Collapse the (possibly non-contiguous) match indices into one window
    // [min..=max] - qwen's PrepareLabel renders a single inverted substring, so
    // suspenders highlights the tight window that spans the match.
    let window = if indices.is_empty() {
        None
    } else {
        let min = *indices.iter().min().unwrap() as usize;
        let max = *indices.iter().max().unwrap() as usize;
        Some((min, max + 1))
    };
    Some((score, window))
}

// The strength ladder (qwen `getCommandMatchStrength`), verbatim: EXACT if the
// value equals the query, PREFIX if it starts with the query, SEGMENT_PREFIX if
// the query prefixes the value at an interior `-`/`_`/`/`/space boundary, else
// FUZZY. `start` is where the fuzzy match began (used only for the segment
// test).
fn strength(value: &str, query: &str, start: usize) -> MatchStrength {
    let v = value.to_lowercase();
    let q = query.to_lowercase();
    if v == q {
        return MatchStrength::Exact;
    }
    if v.starts_with(&q) {
        return MatchStrength::Prefix;
    }
    let v_chars: Vec<char> = v.chars().collect();
    if start > 0 && start <= v_chars.len() {
        let boundary = matches!(v_chars[start - 1], '-' | '_' | '/' | ' ');
        let tail: String = v_chars[start..].iter().collect();
        if boundary && tail.starts_with(&q) {
            return MatchStrength::SegmentPrefix;
        }
    }
    MatchStrength::Fuzzy
}

// qwen `getRecentScore`: `count * 10 + 10 * max(0, 1 - age/decay)`, 0 when the
// command has no record. `now` is the injected host clock (ADR-0019 no wall
// clock in the pure core); the decay is linear over [`RECENT_DECAY_MS`].
fn recent_score(recent: &dyn Fn(&str) -> Option<RecentUse>, name: &str, now: Millis) -> f64 {
    let Some(use_record) = recent(name) else {
        return 0.0;
    };
    let age = now.saturating_sub(use_record.used_at) as f64;
    let freshness = (1.0 - age / RECENT_DECAY_MS as f64).max(0.0);
    use_record.count as f64 * 10.0 + 10.0 * freshness
}

// The comparator (qwen `compareRankedCommandMatches`): strength DESC,
// completion-priority DESC, recency DESC, score DESC, then start ASC, length
// ASC, original index ASC.
fn compare_ranked(left: &Ranked, right: &Ranked) -> std::cmp::Ordering {
    right
        .strength
        .cmp(&left.strength)
        .then_with(|| right.completion_priority.cmp(&left.completion_priority))
        .then_with(|| {
            right
                .recent_score
                .partial_cmp(&left.recent_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| right.score.cmp(&left.score))
        .then_with(|| left.start.cmp(&right.start))
        .then_with(|| left.item_length.cmp(&right.item_length))
        .then_with(|| left.original_index.cmp(&right.original_index))
}

// Collapse a ranked match into a render-ready [`Suggestion`]: the label is the
// matched value (an alias when one ranked best, so the user sees what matched),
// the value is always the canonical name, the description is the help.
fn to_suggestion(r: Ranked) -> Suggestion {
    let label = if r.is_alias {
        r.matched_value
    } else {
        r.command.name.to_string()
    };
    Suggestion {
        label,
        value: r.command.name.to_string(),
        description: r.command.help.to_string(),
        matched: r.matched,
    }
}

/// Ranks repo-relative `paths` against `query` for the AT file picker (System B,
/// qwen `useAtCompletion` → `FileSearch.search`), best-first. Each kept path
/// becomes a [`Suggestion`] whose `label`/`value` are BOTH the path (the caller
/// escapes the value before inserting - see the composer's `escape_path`), and
/// whose `matched` is the contiguous fuzzy-match window over the path for the
/// inverted highlight. An EMPTY `query` keeps every path in the given (already
/// sorted, `walk::walk_files`-deterministic) order with no highlight - qwen
/// searches `''` and the file searcher returns its natural order. Capping to
/// `MAX_SUGGESTIONS_TO_SHOW * 3` is the CALLER's job (the adapter), matching
/// qwen's `maxResults`; this ranks the whole set.
///
/// PURE like [`rank`]: the one impurity is the `nucleo-matcher` fuzzy pass,
/// surfaced only as a score + window. A path that does not fuzzy-match `query`
/// is dropped, so a narrowing query shrinks the list (mirroring the palette).
pub fn rank_paths(paths: &[String], query: &str) -> Vec<Suggestion> {
    if query.is_empty() {
        // qwen's empty-pattern search returns the searcher's natural order; the
        // walk is already sorted, so keep it verbatim with nothing highlighted.
        return paths.iter().map(|p| path_suggestion(p)).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    // The fuzzy score is the sole sort key (paths carry no strength ladder /
    // recency / priority the way commands do): higher score first, ties broken
    // by the original (sorted) order so the result stays deterministic.
    let mut ranked: Vec<RankedPath<'_>> = paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            fuzzy_one(&mut matcher, path, query).map(|(score, window)| RankedPath {
                score,
                index,
                path,
                window,
            })
        })
        .collect();
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
    ranked
        .into_iter()
        .map(|r| Suggestion {
            label: r.path.to_string(),
            value: r.path.to_string(),
            description: String::new(),
            matched: r.window,
        })
        .collect()
}

// One fuzzy-matched path before it collapses to a [`Suggestion`]: its nucleo
// score (the sort key), original index (the sorted-order tiebreak), the path,
// and the contiguous match window for the highlight.
struct RankedPath<'a> {
    score: u16,
    index: usize,
    path: &'a str,
    window: Option<(usize, usize)>,
}

// One AT path suggestion with no match window - the empty-query row (the whole
// path is the label AND the value; the caller escapes it on accept).
fn path_suggestion(path: &str) -> Suggestion {
    Suggestion {
        label: path.to_string(),
        value: path.to_string(),
        description: String::new(),
        matched: None,
    }
}

#[cfg(test)]
mod tests {
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
    fn an_empty_query_lists_every_command_in_registry_order() {
        let s = rank("", &no_recent, NOW);
        assert_eq!(values(&s), vec!["model", "theme"]);
    }

    #[test]
    fn recency_floats_a_used_command_to_the_top_of_the_empty_query() {
        // "theme" used recently outranks "model" despite the registry order.
        let recent = |name: &str| {
            (name == "theme").then_some(RecentUse {
                count: 3,
                used_at: NOW,
            })
        };
        let s = rank("", &recent, NOW);
        assert_eq!(values(&s), vec!["theme", "model"], "recent-first");
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
    fn m_ranks_model_first_by_prefix() {
        // "/m" prefixes "model" (PREFIX) and also fuzzy-matches "theme" (the
        // 'm' in "theme"), exactly like qwen's fzf: both surface, but the
        // PREFIX strength floats "model" first.
        let s = rank("m", &no_recent, NOW);
        assert_eq!(s[0].value, "model", "the prefix match ranks first");
        assert_eq!(s[0].matched, Some((0, 1)), "the leading 'm' is highlighted");
        // "/mod" prefixes only "model"; "theme" has no 'o'/'d' to fuzzy-match.
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
}
