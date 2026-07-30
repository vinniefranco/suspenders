# Phase 7 design — flat footer + theme reconcile + fidelity pass (FINAL)

Replace suspenders' bespoke POWERLINE status bar with qwen's FLAT footer, close the
deferred colour-role gaps, final fidelity pass. All ratatui in components.rs (ADR-0019);
theme slots are pure values. Ground truth: `docs/qwen-ui-reference.md` §3 (Footer) + §7
(colours) + qwen `Footer.tsx`/`AutoAcceptIndicator.tsx`/`ContextUsageDisplay.tsx`/
`themes/qwen-dark.ts`.

## Ground truth
qwen footer (`Footer.tsx`): ONE row, `justifyContent:space-between`, `paddingX:2`,
`gap:1`, NO powerline, NO block bgs; narrow (<80) stacks to a column. Left = statusline/
worktree `⎇ branch`/leftBottomContent ladder (AutoAcceptIndicator when mode≠default, else
`? for shortcuts`). Right = sandbox `🔒`, `Debug Mode`, ContextUsageDisplay (`N% used`/`N%
context used`), joined by ` | ` in text.secondary. Suspenders has NO worktree/sandbox/
debug/statusline/goal facts (verified) — so most of qwen's items have no producer.

## THE USER DECISION (footer content) — pending
qwen's footer, mapped to facts suspenders has, reduces the right side to just
ContextUsageDisplay; AutoAcceptIndicator moves left. Suspenders' powerline extras (model,
base_url, cost, tokens, mode-dot, compact pill) aren't in qwen's footer.
- Option A (faithful-strict): right = `N% context used` only; drop model/cost/tokens/etc.
- Option B (RECOMMENDED, faithful-FORM): match qwen's flat form exactly, but keep the
  load-bearing suspenders facts as flat ` | `-joined right items = model · context% · cost
  (drop base_url, tokens as redundant-with-%, and the compact pill). Local-first multi-
  provider tool genuinely benefits from always-visible model + cost.
RESOLVE before implementing.

## Flat footer (design for B; A = drop model+cost)
- Right group order (each emitted only if the fact exists), ` | ` separator in
  secondary_style (no leading sep): model `<id>` (secondary), context% (secondary normal /
  error over-limit — match ContextUsageDisplay's INNER colour, not the outer accent
  wrapper), cost (secondary, only when > COST_HIDDEN). Reuse context_percent_label +
  cost_label but WITHOUT the padded() block wrapper.
- Left = leftBottomContent: AutoAcceptIndicator (mode≠Default) label+`(shift + tab to
  cycle)` (label colour per mode via the new success/warning/error slots), else `? for
  shortcuts` (secondary). Ctrl+C/D/esc/vim rungs have no producer — skip.
- DROP the `●/○` run-state mode dot (run-state is the spinner; a dot is a powerline
  remnant) — retire StatusSegment::Mode/ModeState.
- Layout: hand-rolled space-between (measure left+right cells, pad the gap), inset
  PADDING_X=2 both sides, no background fill. Narrow (<80): stay ONE line and let fit/drop shed
  right items (cost→model, context% survives) — NOT qwen's 2-line stack (inline layout math,
  ADR-0046 fixed 1-row footer zone). Documented divergence.
- Rename status_bar→footer, render_status_bar→render_footer, StatusBar→Footer,
  StatusBarView→FooterView. Collapse StatusSegment(11 powerline variants) → small FooterItem
  {Model,Context,Cost} + left {AutoAccept(mode),Shortcuts}; keep a pure fit(width) drop
  policy (Cost→Model→never Context). Assembly stays pure (returns Footer value, tested
  without a frame); only render_footer paints.

## Retire powerline machinery
Delete: SEP_RIGHT/SEP_LEFT, the powerline draw loop, segment_bg, segment_style, SegmentKind,
pressure_style (after confirming no other PressureLevel painter — keep pressure_level on
Screen), StatusSegment paint/kind/cells/fit. Retire from the view: toggles.compact, tokens,
base_url threading (leave ConnectionFacts.base_url on the struct).

## Theme reconcile (ADR-0038/0008)
Add 4 slots to theme_slots! + both tomls (dark total-floor + light total):
- foreground = text.primary #bfbdb6 (was terminal default) -> primary_style
- accent = text.accent AccentPurple #D2A6FF (was prompt_gutter cyan) -> accent_style
- success = status.success AccentGreen #AAD94C (was diff added) -> success_style
- warning = status.warning AccentYellow #FFD700 (was marker_aid amber) -> warning_style
error stays (already clean). secondary/symbol/border stay -> muted (all Gray). New qwen roles
enter as HEX (designed hexes, not legacy ANSI). Keep prompt_gutter slot (ADR-0038: removing a
slot is a contract break) — unread after repoint, or repoint its toml value to purple too.
bar_bg/segment_*/pressure_* colour slots become DEAD and were REMOVED from the schema + both
tomls (no painter reads them; the total-floor contract holds over the remaining slots). Extend
the dark/light totality/drift tests with the 4 new slots. light.toml gets light-polarity counterparts (foreground #24292f,
accent #8839ef, success #1a7f37, warning #9a6700).
RISK: primary_style foreground pin changes body text on non-default terminals (intended, qwen-
faithful, highest-visibility — verify on a non-default bg).

## Fidelity checklist (vs the 5 screenshots)
Footer: 1 flat line, no  triangles, no block bgs, paddingX2, ` | ` grey joins, context%
label/colour, left shortcuts/AutoAccept, narrow sheds not wraps. Colours: user `>`+assistant
`✦` PURPLE #D2A6FF, warning `△`+pending border GOLD #FFD700, success `✓` LIME #AAD94C, body
#bfbdb6. Regression spot-check (Phases 2-6): tool boxes/glyphs (✓o⊷?-x), prefixes, diff
(gutter/tint/syntect/═), todo ○◐●+sticky, spinner, approval (inline ›+verbatim), menus (System
A/B). Accept: 1-line narrow footer, no worktree/sandbox/debug, prompt_gutter cyan unread.

## ADRs
- NEW ADR-0053: flat footer replaces the powerline (reverses ADR-0046's status_bar +
  ADR-0008/0040 segment palette; record content A/B choice, segment_*/pressure_* removal,
  narrow-sheds divergence).
- Amend ADR-0038: 4 new semantic slots; dead powerline colour slots removed (total-floor holds).
- Amend ADR-0008: primary_style carries a real fg; accent/success/warning distinct roles.

## Tests
theme: extend dark/light totality + drift with the 4 slots; new-roles-parse-to-hexes. footer:
port bar_at/kinds -> footer_at/items; wide shows model|context|cost in order; narrow sheds
cost then model keeping context; context error over-limit; default-mode left = shortcuts;
non-default = autoaccept label+hint; no-powerline-separators. style: accent purple/success
lime/warning gold/primary foreground (assert .fg == slot).

## Checklist (green between steps)
1. Add 4 theme slots + toml values + extend totality/drift tests (additive, no reader).
2. Repoint accent/success/warning/primary_style helpers + their tests (colour change lands).
3. FooterItem + pure footer() + render_footer alongside old; switch render_pending to it; port
   assembly tests.
4. Delete powerline machinery (SEP_*, segment_style/bg, SegmentKind, pressure_style [confirm
   no other consumer], StatusSegment/StatusBar/render_status_bar) + orphaned tests.
5. Remove dead bar_bg/segment_*/pressure_* colour slots from theme_slots! + both tomls.
6. ADR-0053 + amend 0038/0008.
7. Fidelity pass (run app, walk checklist).

## Risks
1. primary_style fg pin — highest-visibility, verify on non-default bg.
2. PressureLevel orphaning — grep before deleting pressure_style.
3. Segment test churn — mechanical but large.
4. context_percent_label padded() wrapper — need an un-padded flat variant.
5. A/B fork — A simplifies step 3 (drop Model/Cost items).
