# Phase 1 design — inline rendering seam + Commit path

Spike-verified design (ratatui 0.29.0 + crossterm 0.28.1). Implement per the
step-ordered checklist at the end; keep the build green between steps. Invariants:
ADR-0019 (only `ui.rs` + `components.rs` touch ratatui/crossterm; `screen.rs`
pure), ADR-0034, ADR-0046. Ground truth for qwen behaviour: `qwen-ui-reference.md` §1.

## 0. ratatui 0.29 mechanism (verified by compiled spikes)

- Inline terminal: NOT `ratatui::init()` (that enters the alternate screen). Use
  `Terminal::with_options(CrosstermBackend::new(stdout), TerminalOptions { viewport:
  Viewport::Inline(cap) })`, and enter raw mode explicitly with
  `crossterm::terminal::enable_raw_mode()`. Teardown: `disable_raw_mode()` + a
  trailing newline; no alt-screen restore.
- `Terminal::insert_before(height: u16, |buf: &mut Buffer| ...)` renders into a
  temp buffer and scrolls it into the region above the inline viewport; overflow
  past the top goes to native scrollback. No-op on non-inline viewports. Works
  under `TestBackend` (via `append_lines`), so it stays headless-testable.
- The inline viewport height is FIXED at construction (private field, no setter;
  `resize` recomputes from the stored `Inline(h)`). Per-frame reconstruction drifts
  the anchor (calls `append_lines`) and is ruled out. Decision: fixed
  `Inline(cap)` where `cap = min(PENDING_CAP_DESIRED, term_height - 1)`, draw the
  pending region bottom-anchored inside it, clip from the top on overflow (qwen's
  `MaxSizedBox` `overflowDirection:"top"`). Cost: on a real TTY the viewport
  reserves `cap` rows even when the live region is short (Ink does the same) —
  accepted. Fallback if rejected in review: hand-rolled crossterm inline renderer
  in `ui.rs` (large; not Phase 1).

## 1. Commit seam

- **High-water mark in `Transcript`** (pure): `committed: usize`,
  `committed_high_water()`, `mark_committed(n)` (advances; never regresses;
  mutates neither `items` nor `revision` — enroll in the prefix-or-bump property
  test with a "neither" expectation).
- **Committable predicate** — commit final, in-order items, stop at the first
  non-terminal one:
  ```rust
  fn item_terminal(item: &TranscriptItem) -> bool {
      match item {
          TranscriptItem::ToolCall { .. } => false,                       // awaits its result
          TranscriptItem::Marker { tone: Tone::Steering, .. } => false,   // awaits delivery
          _ => true,
      }
  }
  pub fn committable_upto(&self) -> usize {
      self.items.iter().take_while(|it| item_terminal(it)).count().max(self.committed)
  }
  ```
  Per-type rationale: `User`/`Info`/`ToolResult`/`Assistant`/`Thinking`/`Diff` are
  terminal (the live stream lives in `Streaming`, never in `items`, so any settled
  `Assistant`/`Thinking` is final); `ToolCall` is the boundary (a `ToolResult`
  supersedes it); a `Tone::Steering` marker is non-terminal (steering delivery
  removes it), so it must never be committed into frozen scrollback.
- **One new Effect variant**: `Effect::Commit { count: usize }` — carries only the
  count (rendering belongs to the adapter/components per ADR-0019; the adapter
  reads items via `screen.transcript().items()` + `RenderCache`). Screen computes
  `count = committable_upto() - committed_high_water()` at the two public fold
  exits via a single `with_commit(effects)` helper, pushing `Commit` when `> 0`.
  `Commit` REPLACES `Effect::PinBottom` (pinning is meaningless with native
  scrollback).

## 2. Adapter rewrite (`ui.rs`)

Construct inline terminal + raw mode (no mouse capture). Per frame: run effects to
completion (draining `Commit` via `insert_before` then `mark_committed`), then
`terminal.draw(render_pending)`. `commit_items` sums cached `wrapped` counts of
items `[hw, hw+count)`, `insert_before(total_height, render_committed_slice)`, then
`mark_committed(count)`. Delete `Geometry` return + the viewport/geometry threading.
Teardown on both success and error paths. The pre-agent picker (`pick_session`/
`pick_loop`) stays on the alt-screen path in Phase 1 (out of scope).

## 3. components.rs split

Keep `RenderCache` + all line builders/gutters/status/composer/diff/markdown. Add:
- `render_committed_slice(buf, cache, hw, count, theme)` — blits cached `Line`s of
  the slice into a bare `Buffer` at successive `y` (no scroll math; commits draw
  whole, tall ones go to scrollback).
- `render_pending(frame, screen, conn, anim, cache, theme)` — replaces `render`:
  `cache.sync`, assemble the pending stack (uncommitted settled tail
  `cache.settled()[hw..]` + live thinking + streaming tail + lull + status +
  composer + overlay + approval), bottom-anchor + top-clip into `frame.area()`.
Delete `render`, `render_viewport`, `ViewportParams`, `visible_window`, scrollbar.

## 4. Delete list (+ call sites)

`viewport.rs` (whole module + tests); `ScrollStep`; `Effect::{PinBottom,ScrollUp,
ScrollDown}`; `Key::{WheelUp,WheelDown}` + PageUp/PageDown scroll arms (PageUp/Down
lose meaning — native scrollback); mouse capture (`Enable/DisableMouseCapture`,
`map_mouse`, MouseEvent handling); scrollbar; `Geometry` + all threading;
`apply_viewport`; `AdapterState.viewport`. Call sites enumerated in the spike design
(ui.rs imports/seeds/drain_input/apply_viewport; components.rs viewport import/
ViewportParams/StatusBarCtx; screen.rs handle_key scroll arms + emitters).

## 5. Overflow (tall pending item)

`cap = frame.area().height`; reserve bottom rows for composer+status; scrollable
body budget = `cap - composer_rows - 1`; on overflow keep the last `budget - 1`
rows (drop from top) + a one-row `… Ctrl-S to show more` marker (`text.secondary`).
Ctrl-S expand handling deferred; Phase 1 wires the marker + clip only.

## 6. Tests

Pure core stays testable; `TestBackend` works for inline + `insert_before`. New:
`committable_upto` per item type (leading `ToolCall` blocks; `Tone::Steering`
non-terminal; delivery makes promoted `User` terminal; `mark_committed` monotonic);
Screen emits `Commit { count }` after ToolResult merge / message_end / steering
delivery, none while a `ToolCall` pends; `render_committed_slice` golden vs old
`render_viewport` slice; `render_pending` bottom-anchor (blank top) + top-clip
(keep bottom + marker); `run_effect(Commit)` advances high-water and drops committed
text from the pending draw. Retire viewport/drain-scroll/map_mouse-wheel/
visible_window tests.

## 7. Risks

1. Reserved-row gap on real TTY (fixed Inline cap) — accept (Ink parity); tune cap
   ~12-16; fallback hand-rolled crossterm renderer.
2. insert_before/draw interleave — safe (single-threaded loop, effects flush before
   draw).
3. Height measure/draw width drift — sync cache at the same (full) width the commit
   draws at (ADR-0029 measure==draw discipline; no scrollbar gutter now).
4. Resize re-wraps only the pending region; committed scrollback keeps old wrap
   (immutable) — accept (refreshStatic out of scope).
5. PageUp/Down UX loss — intended (native scrollback); note in CONTEXT.md.

## 8. Checklist (green between steps)

1. `transcript.rs`: high-water + predicate + tests + property-test enrollment.
2. `screen.rs`: `Effect::Commit`, `with_commit`, `mark_committed` verb, emission
   tests (temp adapter arm keeps build green).
3. `components.rs`: `render_committed_slice` + golden test.
4. `components.rs`: `render_pending` alongside old `render`; anchor/clip tests.
5. `ui.rs`: inline construction + raw mode; `Commit` handler (`commit_items`);
   point draw at `render_pending`; drop `Geometry` return; stub viewport to compile.
6. `screen.rs`: delete scroll effects/keys + arms; drop `PinBottom` emitters.
7. `ui.rs`: delete `Geometry`/`apply_viewport`/mouse/`map_mouse`/wheel/scroll/
   `AdapterState.viewport`; delete old `render`/`render_viewport`/`ViewportParams`/
   `visible_window`/scrollbar.
8. Delete `viewport.rs` + `pub mod viewport`; retire dead tests.
9. `components.rs`: add the `Ctrl-S to show more` overflow marker.
10. Update CONTEXT.md/ADR-0046 (fixed Inline(cap), native-scrollback PageUp change);
    run `cargo test`, `cargo clippy -D warnings`, `rustqual`.
