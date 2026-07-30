# Phase 5 design — menus revamp (slash / model / theme)

De-converge suspenders' one unified `Selector` into qwen's TWO distinct systems (the
Phase-0 finding: merging them is a bug). All ratatui in components.rs (ADR-0019);
selection/fuzzy state pure. Ground truth: `docs/qwen-ui-reference.md` §5 + qwen source.

## Current reality
Today `/`, `/model`, `/theme` ALL flow through ONE pure `Selector` (`src/ui/selector.rs`,
group-aware substring filter) hosted as a Composer overlay, rendered by `popup_rows`.
Phase-4 built `SelectionList` (`src/ui/selection.rs`, System A core) used only by the
approval radio. Phase 5 splits the menus onto the two systems and retires `Selector`.

## Decisions
1. **System A = model & theme dialogs on `SelectionList`** (numbered `›`, digit+arrows,
   NO free-text filter). Extend `SelectionList`: `disabled` mask (headers/greyed/broken
   rows skipped by nav, qwen findNextValidIndex), `with_active(initial)`, and a render-only
   scroll window + `▲/▼` in `selection_rows` (qwen puts scrollOffset in BaseSelectionList,
   not the fold). `onHighlight` is a DERIVATION (read `active()` each frame), not a callback.
2. **System B = the `/` popup** — NEW `src/ui/completion.rs`: pure `Completion`/`Suggestion`/
   `rank`. Fuzzy via **`nucleo-matcher`** (ui-confined, pure-Rust, fzf-v2 parity, returns
   match indices). Rank by qwen's explicit ladder exact>prefix>segment-prefix>fuzzy
   (`useSlashCompletion.ts:209-249`) with nucleo score as the FUZZY-tier tiebreak, recency
   below (RECENT_DECAY 10min). Color-only active row (accent vs secondary, NO `›`/number),
   two columns, inverted match substring (PrepareLabel), `←/→` expand (MAX_WIDTH 150),
   Tab/Enter accept (trailing space = `/{value} ` normalization), Esc dismiss,
   MAX_SUGGESTIONS_TO_SHOW 8, `▲/▼` + `(n/m)` counter. Prefix fallback if matcher errors.
   `SlashCommand.alt_names` for name+altNames. `now: Millis` injected (no wall-clock in core).
3. **The two stay DISTINCT** via the single `in_selector` fork in composer.rs: MENU
   (System B, editable filter) vs DIALOG (System A, draft frozen, editing keys swallowed).
   One branch, two systems, no third.
4. **Theme dialog** (`theme_command.rs`): live-preview-on-highlight + Esc-reverts PRESERVED
   VERBATIM — `preview_name(Option<(&str,&SelectorRow)>)` unchanged; only its input repoints
   from `Selector::highlight` to `DialogList.active()`. So theme_command.rs + its 4 tests need
   ZERO edits (regression firewall). Broken rows → disabled (skipped) with reason dim inline.
   Tab/scope selector DROPPED (suspenders single-scope config, ADR-0038).
5. **Model dialog** (`model_command.rs`): rows from `model_rows` (unchanged), headers/greyed
   → disabled. Keep provider HEADER rows (richer than qwen's per-row [authType] badge for
   suspenders' N providers). Detail pane on highlight (context window + current marker; modality/
   baseUrl if the catalog carries them, else follow-up), switch on Enter (NO live switch).
   **DECISION (user, 2026-07-29): KEEP A FILTER — deliberate divergence** from qwen's filter-less
   dialog (suspenders surfaces hundreds of catalog models). The model dialog is a hybrid: System A
   render (numbered `›`, detail pane, headers) + an editable SUBSTRING filter (case-insensitive
   `contains` over the row label, NOT nucleo - whole-group retention needs header/note travel, which
   a fuzzy per-row match cannot give) that narrows the visible rows as the user types after
   `/model `. The narrowed
   rows back a `SelectionList` (numbered ›, digit/arrows/Enter). The `/` popup stays System B; the
   THEME dialog stays filter-LESS System A (few themes, qwen-faithful). So the composer DIALOG mode
   has two flavors: model = editable-filter, theme = frozen-draft.
6. **Retire `selector.rs`, `slash::rows`, `popup_rows`. Keep `picker.rs`** (session picker,
   neither system). Grouping/greyed classification survives as `model_rows`/`theme_rows` output
   + `SelectionList.disabled`.

## MODEL-DIALOG FILTER — RESOLVED (user 2026-07-29): KEEP A FILTER (divergence)
Model dialog = System A render + editable SUBSTRING filter (case-insensitive `contains` over the
row label, narrows rows on type after `/model `). Deliberately NOT nucleo: the numbered dialog keeps
provider HEADER rows and per-group NOTES, and a whole-group must travel when any member matches -
substring retention (`filter_rows`) does that; a per-row fuzzy match would drop the headers/notes.
The `/` popup keeps its nucleo fuzzy rank (System B, per-row, no groups). Theme dialog stays
filter-less System A. Composer DIALOG mode gets two flavors: model=editable-filter,
theme=frozen-draft. Justified by suspenders' large catalogs (hundreds of models); recorded in the
two-systems ADR + ADR-0033 amendment.

## Invariants
ADR-0019: Completion/rank/Suggestion + SelectionList pure (now:Millis injected, nucleo confined
to completion.rs, returns only String/Vec<usize>); all render in components.rs; live-preview a
derivation not a callback. committed==pending N/A (overlays are pending-only, uncommitted chrome).

## ADRs
- NEW: two selection systems (fuzzy `/` palette vs numbered dialogs; supersede the ADR-0032/0033
  convergence; record model-dialog filter loss per the decision).
- NEW/amend: nucleo-matcher dep, ui-confined, fzf-v2 parity, prefix fallback.
- Amend ADR-0038 (theme: broken→disabled, scope dropped, live-preview preserved), ADR-0033
  (model: headers over badge, detail-on-highlight, switch-on-Enter, filter decision).

## Tests
completion.rs: rank ladder + recency tiebreak + empty-query order + wrap nav + Tab==Enter accept
+ ←/→ expand-only + scroll window + prefix fallback (pin canonical orderings for /m,/mod,/,segment).
selection.rs: disabled-skip both dirs + wrap, initial skips disabled; digit/expire unchanged.
theme_command/model_command: preview_name/model_rows/theme_rows/pick tests UNCHANGED (firewall).
components.rs (TestBackend): System B active=accent no ›/number, two columns, inverted match, ▲▼/(n/m),
expand; System A numbered › + marker + scroll arrows + disabled dim; goldens for model/theme/slash.
composer: typing /mo shows System B; committing /model → DIALOG ignores typed chars (no filter); Esc
empties; generation-guard fill unchanged.

## Checklist (green between steps; step 6 is the atomic swap)
1. Add nucleo-matcher to Cargo.toml (ui-confined). 2. Extend SelectionList (disabled/with_active +
tests; approval `new(len)` unchanged). 3. Extend selection_rows (scroll window+▲▼+detail variant,
defaulted so approval call site unchanged). 4. NEW completion.rs (rank/Completion/Suggestion +
alt_names + full pure tests; unwired). 5. NEW suggestion_rows + draw_popup tests (unwired). 6. ATOMIC
SWAP in composer.rs: slash_cursor→Completion (MENU), Selector→DialogList/SelectionList (DIALOG),
repoint selector_highlight→active(), OverlayView payload change, menu_key/selector_key delegate,
DIALOG swallows editing keys, render dispatch to suggestion_rows/selection_rows, model/theme run
builds DialogList in SelectorReady fill; preview_name untouched. 7. Delete selector.rs/slash::rows/
popup_rows. 8. ADRs + full suite + fmt.

## Risks
1. Fuzzy rank parity (nucleo≠fzf) — port qwen's strength LADDER (score only a fuzzy-tier tiebreak);
   pin canonical orderings. 2. Atomic composer.rs swap (hottest file) — new pieces tested in isolation
   first (steps 2-5); theme/model policy tests as firewall. 3. Model-dialog filter regression — the
   pending decision above. 4. Detail-pane data availability — render what the catalog has, scope rest.
   5. Inverted-match render (nucleo non-contiguous vs PrepareLabel single window) — collapse to [min..=max].
