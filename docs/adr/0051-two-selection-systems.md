# Two selection systems: the fuzzy `/` palette and the numbered `›` dialogs

Suspenders had ONE unified `Selector` (a group-aware substring filter over
`SelectorRow`s) behind all three menus - the `/` command palette, `/model`, and
`/theme` - hosted as a Composer overlay and rendered by `popup_rows`
(ADR-0032/0033 convergence). Porting qwen-code v0.16.0 (the Phase-0 finding)
revealed that qwen deliberately runs TWO distinct selection systems, and merging
them is a bug: the `/` palette and the numbered dialogs look, rank, and navigate
differently. This ADR de-converges them and supersedes the ADR-0032/0033
one-widget premise.

## Decision

Split the menus onto the two systems qwen runs, kept DISTINCT by the single
`in_selector` fork in `composer.rs` (MENU vs DIALOG - one branch, two systems,
no third):

**System A - the numbered `›` dialogs** (`ui::selection::SelectionList`, qwen
`useSelectionList.ts` + `BaseSelectionList.tsx`). Drives `/model` and `/theme`.
Rows are numbered (`1.`..`N.`), the active row carries a `›` U+203A marker in a
success-green gutter, and navigation is arrows (wrapping, skipping disabled
rows) + digit quick-select. `SelectionList` grew a per-row `disabled` mask (qwen
`findNextValidIndex`/`computeInitialIndex`): headers, greyed catalog rows, and
broken themes are unnavigable and Enter refuses them; `with_active` snaps the
initial active row off any disabled row. The approval radio's `SelectionList::new`
leaves the mask all-`false`, so its behaviour is unchanged. Render is
`components::dialog_rows` (numbered `›`, dim headers/greyed, `▲/▼` scroll).

**System B - the fuzzy `/` palette** (`ui::completion`, qwen
`useSlashCompletion.ts` + `useCompletion.ts` nav + `SuggestionsDisplay.tsx`).
Drives the `/` popup. Color-only: the active row reads `text.accent`, the rest
`text.secondary` - NO `›` marker, NO numbers. Two columns (command |
description), the fuzzy match substring drawn inverted (qwen `PrepareLabel`,
collapsed to one `[min..=max]` window), `←/→` expand, Tab/Enter accept (trailing
space), Esc dismiss, `MAX_SUGGESTIONS_TO_SHOW=8` with `▲/▼` + a `(n/m)` counter.
Ranking ports qwen's explicit `getCommandMatchStrength` ladder VERBATIM as the
PRIMARY sort key - EXACT > PREFIX > SEGMENT_PREFIX > FUZZY - then the command's
static `completion_priority` (qwen `completionPriority`), then recency, then the
fuzzy score (the FUZZY-tier tiebreak), then `start`/length/original-index below
(qwen `compareRankedCommandMatches`, field for field). An empty query
ranks by recency then that comparator. `SlashCommand.alt_names` feeds name +
altNames into the ranking; the best-ranked value labels the row.

The fuzzy matcher is **`nucleo-matcher`** (pure-Rust, fzf-v2 parity), confined to
`ui::completion` and surfaced only as a `u16` score + match indices - no matcher
type crosses the pure seam (ADR-0019). A matcher error or the empty pattern falls
back to a PREFIX-only filter, so the palette never crashes and never shows an
empty list where a prefix would have matched. `now: Millis` is injected (the
recency decay's clock), never read from the wall clock (ADR-0019).

Retired: `ui::selector` (the unified `Selector`), `slash::rows`, and
`components::popup_rows`. The session picker (`ui::picker`) is neither system and
is untouched. Grouping/greyed classification survives as `model_rows`/`theme_rows`
output + the `SelectionList` disabled mask.

## Model-dialog filter (deliberate divergence)

qwen's model dialog has NO free-text filter. Suspenders surfaces hundreds of
catalog models, so the model dialog KEEPS an editable filter: System A
render (numbered `›`, headers, detail) + a filter that narrows the rows as the
user types after `/model `. The filter is a case-insensitive SUBSTRING match
over the row label (`filter_rows`), NOT the `/` palette's nucleo fuzzy - the
numbered dialog keeps provider HEADER rows and per-group NOTES, and a whole
group must travel when any member matches (header + notes retained). A per-row
fuzzy match would drop the headers/notes, so substring is the correct filter
here; nucleo stays confined to the group-less System B palette. The theme
dialog stays filter-LESS System A
(few themes, qwen-faithful) - it swallows editing chars so the normalized
`/theme ` draft never grows. The composer DIALOG mode thus has two flavours:
model = `Filtered`, theme = `Frozen`.

## Consequences

- Navigation diverges from the retired `Selector`: System A now WRAPS at the
  ends (qwen `useSelectionList`), where the old selector saturated.
- The theme live-preview firewall holds: `theme_command::preview_name` and its
  tests are unchanged; only its INPUT repoints from `Selector::highlight` to the
  dialog's active row (`Composer::selector_highlight`).
- A bare `Key::Tab` is now mapped (palette accept); it is inert outside the
  palette (the Composer refuses it), so it never types a literal tab.
- The two systems' fold methods mix a guard with a delegating call
  (guard-then-delegate dispatchers). Four register as IOSP "logic + calls" in
  rustqual - `DialogList::refilter`, `CommandSelector::refilter`,
  `Composer::refilter_from_draft`, and `Composer::fill_ready`. Each is a guard
  (a skip predicate, an `if let Some(..)`, or a generation check) followed by a
  single delegating call; the pure compute is already extracted
  (`DialogList::rebuilt`, `suggestion_frame`), and the clippy `option_map_unit_fn`
  lint forbids the combinator rewrites that would satisfy the IOSP detector, so
  they are left as idiomatic guard-then-call - the same character as the
  pre-existing accepted fold/render roots (`main`, `render_pending`,
  `render_composer`). `refilter_from_draft` is the single owner of the
  parse→rest→refilter dance, called by `selector_key` and `dialog_edit` (it
  retired a 3-site duplication across those and `fill_ready`).
