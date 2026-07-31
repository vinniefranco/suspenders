# Compact mode (Ctrl+O) and the scrollback-redraw seam

qwen-code has ONE display toggle over the transcript: `compactMode`
(`Ctrl+O` = `TOGGLE_COMPACT_MODE`), default off. When on it hides Thinking items
entirely (`HistoryItemDisplay` gates on `!compactMode`) and hides tool RESULT
bodies while keeping their headers (`ToolMessage`: `!compactMode ||
forceShowResult`). Suspenders had grown a DIFFERENT, bespoke mechanic: two
independent expand toggles - Ctrl-T (`thinking_expanded`) and Ctrl-O
(`tools_expanded`) - each defaulting to a COLLAPSED one-liner and expanding to the
full body. That is the inverse polarity of qwen and a different key map, and
ADR-0046 already decided suspenders is a faithful qwen port. Phase 6 reconciles.

## Decision

Collapse the two expand toggles into ONE `Screen::compact_mode: bool` (default
`false` = show everything), toggled by `Ctrl+O` → `Key::ToggleCompact`. Ctrl-T is
retired outright (see ADR-0046's Phase 6 update). The polarity inverts to match
qwen: the old code showed the full body when `expanded == true`; the new code
shows the full body when `compact_mode == false`.

`compact` threads through the ONE `message_lines` → `RenderCache` → `grouped_rows`
path that BOTH the pending body and `render_committed_slice` draw, so the
committed==pending identity (ADR-0046) holds under compact by construction. Its
effect on each item, faithful to qwen:

- **Thinking**: hidden ENTIRELY under compact (zero lines). There is no collapsed
  one-liner - qwen only ever show/hides a thought, so the old truncated-first-line
  form is deleted.
- **Diff / Todo (tool result bodies)**: folded to the header row under compact;
  the header always stays.
- **ToolCall / ToolResult / User / Assistant / Info / Marker**: untouched (they
  are single header/text rows already, or not a tool result body).

### The scrollback-redraw seam (`Effect::RedrawScrollback`) — RETIRED

> **Retired by the fullscreen model (ADR-0046).** The whole seam below exists only
> because the inline port froze committed rows in native scrollback. Under the
> fullscreen alt-screen model the app redraws the ENTIRE transcript from the model
> every frame, so flipping `Screen::compact_mode` re-renders every item at the new
> compact for free - there is no frozen prefix, no `Effect::RedrawScrollback`, no
> degraded fallback, and no upstream ratatui blocker. `Effect::RedrawScrollback`
> and `ui::redraw_scrollback` are DELETED. The rest of this section and the SPIKE
> below are kept as historical record of the abandoned inline design.

Committed Thinking/tool rows are frozen in native scrollback (ADR-0046's
`insert_before`); a compact toggle can't un-draw them, which would split-brain the
history (old rows keep the old compact, new pending rows the new one). qwen solves
this with `refreshStatic` = `clearTerminal` (wipe screen + scrollback) then replay
the whole committed history at the new `compactMode`. We port the SHAPE:

- pure `Transcript::compact_toggle_has_visual_effect()` → `true` iff any COMMITTED
  item is compact-affected (a Thinking item or a tool-group member). The pending
  region redraws every frame for free, so only the frozen prefix
  `[0, committed_high_water)` matters.
- new `Effect::RedrawScrollback` (carries no ratatui, no count - ADR-0019), minted
  by the `Ctrl+O` handler ONLY when that predicate is true. A plain-chat toggle
  (nothing compact-affected committed) mints nothing and flickers not at all.
- the adapter (`ui::redraw_scrollback`, sibling of `commit_items`) re-syncs the
  render cache to the new compact and repaints the live viewport. The high-water
  mark is left UNCHANGED (the committed prefix stays committed).

The shipped `redraw_scrollback` is a viewport-only re-render: it touches ONLY the
live region and NOTHING already frozen in native scrollback. So it is NOT an
exception to ADR-0046's "never touch frozen scrollback" - it is fully INSIDE that
rule. The faithful qwen `refreshStatic` (a scrollback-clearing re-blit) would be
the exception, but it is not what shipped; see the SPIKE result below.

## SPIKE result (the HIGH risk): the faithful re-blit is blocked upstream

The Phase-6 design flagged the full `terminal.clear()` + re-`insert_before` replay
as the high risk and asked for a spike before committing to the API. The spike's
findings, stated at the right layer:

1. **The scrollback purge IS available.** crossterm 0.28 has
   `ClearType::Purge` (`\x1b[3J`, clear scrollback), and `ui.rs` uses
   `CrosstermBackend` directly - not through a lowest-common-denominator trait -
   so it CAN emit the purge. The earlier framing that the purge was "un-portable
   via the ratatui `Backend` trait" pointed at the wrong layer.
2. **The real blocker is the private viewport anchor.** A faithful `refreshStatic`
   must re-anchor the inline viewport to `y = 0` and re-blit `[0, high_water)` from
   the top after purging. ratatui 0.29's viewport-anchor mutator
   (`set_viewport_area`) is PRIVATE, so there is no supported way to reset the
   inline viewport's origin. Even if it were reachable, the re-anchor path would be
   `CrosstermBackend`-specific (untestable under `TestBackend`, which cannot model a
   scrollback purge or a viewport re-anchor at all) and would repaint O(all-history)
   rows every toggle - a full-history flicker.

Per the design's explicit directive ("if it doubles/orphans rows, report and use
the degraded viewport-only fallback rather than shipping broken scrollback"), the
SHIPPED behaviour is the DEGRADED fallback: the pending region and every future
commit render at the new compact; the already-frozen prefix above the fold keeps
the compact it was blitted at. This fallback touches NOTHING frozen - no doubling,
no orphaning - just a bounded staleness of the history above the fold until it
scrolls away. Faithful re-blit is left as future work gated on an upstream ratatui
viewport-anchor / viewport-reset API (NOT on a "Purge variant" - the purge already
exists in crossterm).

## Consequences

- **Retired:** `Key::ToggleThinking`, `Screen::thinking_expanded`,
  `Screen::tools_expanded`, the `Toggles { thinking_expanded, tools_expanded }`
  pair (now `Toggles { compact }`), the two `StatusSegment::Thinking`/`Tools`
  status-bar segments (now one `StatusSegment::Compact`), and the collapsed
  settled-Thinking one-liner render branch.
- **Status bar:** the `▸/▾ thinking` + `▸/▾ tools` segments become one
  `▸/▾ compact`; the drop-tier count falls 7 → 6, and the fit thresholds shift
  down by roughly one segment's width (a test threshold moved 70 → 66 cols).
- **Revised in place:** ADR-0046 (Ctrl-T retirement completed; thought-subject
  fallback divergence recorded).
- **Retired by ADR-0046's fullscreen model:** `Effect::RedrawScrollback`,
  `ui::redraw_scrollback`, and `Transcript::compact_toggle_has_visual_effect()`.
  Flipping `compact_mode` needs no effect - the next full-frame redraw renders the
  whole transcript at the new compact. The `Toggles { compact }` cache key and the
  per-item compact effects (Thinking hidden, tool result bodies folded) are
  UNCHANGED; only the scrollback-reconciliation seam is gone.
- The compact item-effects still flow through the ONE `message_lines` →
  `RenderCache` → `grouped_rows` path the whole-transcript render draws, so compact
  is applied uniformly to every item each frame by construction.
