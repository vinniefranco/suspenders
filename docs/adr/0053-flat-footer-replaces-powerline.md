# Flat footer replaces the powerline status bar

The qwen-code UI port (ADR-0046 and its Phase siblings) reached the status line
last. Suspenders drew a bespoke POWERLINE bar: block-background segments joined
by `` triangles (`SEP_RIGHT`/`SEP_LEFT`), a run-state mode dot, and a
per-`PressureLevel` token block. qwen-code's `Footer.tsx` is nothing like it: a
single flat row, `justifyContent:"space-between"`, `paddingX:2`, no triangles,
no block backgrounds, the right group ` | `-joined in `text.secondary`. To match
the qwen look the powerline had to go, because the look is the segment machinery.

## Decision

Replace the powerline `status_bar` with a flat `footer` (qwen `Footer.tsx`): ONE
row, hand-rolled space-between with a 2-cell inset on each side, NO background
fill. This reverses ADR-0046's `status_bar` naming and the ADR-0008/0040 segment
palette. The names move with the shape: `status_bar`→`footer`,
`render_status_bar`→`render_footer`, `StatusBar`→`Footer`,
`StatusBarView`→`FooterView`; the eleven-variant `StatusSegment` collapses to a
small `FooterItem` {Model, Context, Cost} plus a left `FooterLeft`
{AutoAccept(mode), Shortcuts}. The pure `footer()` assembly returns a `Footer`
value tested WITHOUT a frame; only `render_footer` touches ratatui (ADR-0019).

### Footer content — Option B (faithful FORM, load-bearing facts kept)

qwen's footer, mapped to the facts suspenders actually has (no worktree, sandbox,
debug, statusline, or goal producers), reduces the right side to just the context
figure. Two readings were on the table:

- **Option A** (faithful-strict): right = `N% context used` only.
- **Option B** (CHOSEN): match qwen's flat FORM exactly, but keep the
  load-bearing suspenders facts as the ` | `-joined right group —
  `model <id>` · context% · cost. A local-first, multi-provider tool genuinely
  benefits from an always-visible Active Model and Session cost; dropping them to
  chase strict fidelity would lose information the operator steers by. Dropped as
  redundant/absent: `base_url`, the token count (redundant with context%), the
  compact pill, and the `●/○` run-state mode dot (the spinner already carries
  run-state; a dot was a powerline remnant).

Right group order, each item emitted only when its fact exists, ` | ` separator
in `text.secondary` with NO leading separator: `model <id>` (secondary),
context% (secondary normal / `error` over-limit — qwen's INNER
`ContextUsageDisplay` colour, not an outer accent wrapper), cost (secondary, only
when the Session total is positive). The left content is qwen's
`leftBottomContent` ladder trimmed to the producers suspenders has: the
`AutoAcceptIndicator` (mode label + ` (shift + tab to cycle)`) when the Approval
mode is not Default, else `? for shortcuts`.

### Narrow: shed, don't wrap (a documented divergence)

qwen switches the footer to a two-line column below 80 columns. Suspenders stays
ONE row and lets `Footer::fit` shed right-group items lowest-value first
(cost → model; context% NEVER drops), because ADR-0046 fixed a one-row footer
zone in the inline layout and a growing footer would fight the Composer. Which
items show at a given width is a SEMANTIC decision, so it lives in the pure
`footer()` layer, not the painter.

### Retired machinery

Deleted outright: `SEP_RIGHT`/`SEP_LEFT`, the powerline draw loop,
`segment_style`/`segment_bg`, `SegmentKind`, `StatusSegment`, and `pressure_style`
(confirmed to have no consumer besides the retired bar — `PressureLevel` itself
stays on `Screen`, since context over-limit is now read straight off the
token/budget ratio, not a pressure block). The `TokenView` carrier's `level`
field died with the pressure block, so `FigureView.tokens` is now a plain
`Option<u64>` estimate.

The theme SLOTS the powerline read (`bar_bg`, `segment_muted_bg`, and the
`segment_*`/`pressure_*` family) were DELETED alongside the machinery: with no
painter left to read them they became dead colours, so they were removed from
the `theme_slots!` schema and both tomls. The total-floor contract (ADR-0038)
still holds over the remaining slots. See the ADR-0038 amendment. (The
`PressureLevel` STATE on `Screen` is unrelated and stays; only the pressure
COLOUR slots were dead.)

## Consequences

- The footer is a thin painter over a pure, frame-free assembly: item order, the
  shed policy, and the fact-presence rules are all unit-tested without a frame.
- The colour roles the footer paints (`success`/`warning`/`error`/`secondary`,
  and the per-mode AutoAccept label colour) come from the ADR-0008 semantic slots
  reconciled in this same phase — see the ADR-0008 amendment.
- The narrow behaviour diverges from qwen (shed vs. stack). Accepted: a one-row
  footer is the ADR-0046 layout contract; the shed keeps context% — qwen's sole
  figure — visible longest.
- `ConnectionFacts.base_url` stays on the struct (a Session fact other code may
  want) even though the footer no longer shows it.
