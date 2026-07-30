# Inline scrollback rendering, a faithful qwen-code port

We are abandoning Suspenders' bespoke TUI presentation and porting qwen-code's
UI/UX wholesale (flow, look, function, displayed stats). qwen-code (Ink/React)
renders **inline** into the normal terminal buffer: finished output is committed
once to the terminal's own scrollback and only a small pending region at the
bottom repaints. Suspenders instead ran a full-screen alternate-screen TUI with
a Suspenders-owned Viewport, scrollbar, and mouse/key scrolling. To get the
actual qwen-code feel - native scrollback as history, the terminal wallpaper
showing through, no exotic scroll - the rendering model is what had to change,
because the look is downstream of it.

## Decision

Render inline. Keep ratatui and crossterm (ADR-0001) but use an inline viewport
plus `insert_before` to Commit finished Transcript items into native scrollback;
repaint only the Pending Region (in-flight Tool Calls, the streaming assistant
message, the Lull spinner) plus the Composer and status line. The pure Screen
core tracks which items are Committable (final, immutable) and emits a Commit
effect in strict order - stopping at the first still-pending item - which the
adapter drains via `insert_before`. Every non-append correction (a Tool Result
absorbing its paired Call, Steering promotion) already operates on not-yet-final
items, so it stays on the pending side of the Commit boundary and never needs to
edit frozen scrollback.

Component chrome, wording, and stats are copied verbatim from qwen-code, strings
included (`Apply this change?`, `Yes, allow always`, `auto-accept edits
(shift + tab to cycle)`, `N% used`, `esc to cancel`). The Approval prompt becomes
an inline numbered block in the Pending Region, not a centered modal.

This is a revamp, not a restyle: qwen-code's **mechanics** are adopted, not just
its colors. Thinking is a rolling thought subject on the spinner line that
replaces the waiting phrase while the model thinks (no Ctrl-T, no expand/collapse
of settled Thinking - both retired). Menus (Slash Command, model, theme) adopt
qwen-code's interaction model: arrow-key navigation, type-to-search filtering,
`Press Enter` to select. The `todo_write` task list adopts qwen-code's
visualization - circle glyphs (retiring the `[ ]`/`[~]`/`[x]` brackets in
`plan.rs`), the committed `TodoWrite` Tool Call list, and a live "Current tasks"
box in the Pending Region (numbered, in-progress floated to top, completed struck
through, capped with an "... and N more" tail). Where a Suspenders menu, thinking,
or task-list behavior differs from qwen-code's, qwen-code's wins.

## Considered and rejected

- **Restyle only** - keep the alternate-screen Viewport and rewrite component
  colors to look like qwen-code. Cheaper, but leaves an opaque scrollable pane;
  it reproduces qwen-code's look without its flow, which was the point. Rejected.

## Consequences

- **Retired:** the Suspenders-owned Viewport (`viewport.rs`), the scrollbar,
  mouse-scroll capture, scroll-key effects, and ADR-0040's turn lane + marker
  plane and standalone live-reasoning tail (qwen-code shows thoughts as inline
  bullets in the Pending Region, no separate lane), and the Ctrl-T settled-
  Thinking expand/collapse toggle (qwen-code has no such mechanic). ADR-0040 is
  superseded.
- **Revised in place, not superseded:** ADR-0034 (the Screen / Transcript /
  Composer split holds; the Transcript now realizes as a Committed history +
  Pending Region and drops the Viewport), ADR-0029 (the render pipeline now
  Commits settled lines to scrollback), ADR-0041 (the Lull draws on the Pending
  Region's spinner line, not under a lane).
- **Kept, reskinned into qwen-code's layout:** the Theme system (qwen-code has
  themes too), the whimsical Lull phrases, and the syntect + red/green-tint diff
  (qwen-code renders the same rich, syntax-colored, tinted diff).
- Terminal scrollback is now the scroll history; any operation that assumed a
  Suspenders-redrawable back-history (settled-item edits, Ctrl-T expansion) is
  either retired or must move to the Pending Region before it Commits.

## Phase 1 implementation notes (spike-verified)

- **Fixed inline viewport height.** ratatui 0.29's `Viewport::Inline(h)` fixes
  the live-region height at construction (private field, no setter). Per-frame
  reconstruction drifts the scrollback anchor (it re-runs `append_lines`), so we
  build ONE `Terminal::with_options(..., Viewport::Inline(cap))` with
  `cap = min(PENDING_CAP_DESIRED, term_height - 1)` (`PENDING_CAP_DESIRED = 16`),
  enter raw mode explicitly (NOT `ratatui::init()`, which enters the alt-screen),
  and tear down with `disable_raw_mode()` + a trailing newline. Cost: on a real
  TTY the viewport reserves `cap` rows even when the live region is short (Ink
  does the same) - accepted.
- **Commit seam.** The pure `Transcript` owns a monotonic `committed` high-water
  mark; `committable_upto()` counts leading terminal items (stopping at a live
  `ToolCall` or a `Tone::Steering` marker), and `Screen::with_commit` advances
  the mark and emits `Effect::Commit { count }` at the two public fold exits
  (`apply_event`/`handle_key`). The adapter drains it via `insert_before`, which
  blits the just-frozen slice `[hw - count, hw)` (rendered by
  `components::render_committed_slice`) into native scrollback. Rendering stays
  in the adapter/components layer (ADR-0019); the effect carries only the count.
- **Pending region.** `components::render_pending` draws the uncommitted settled
  tail plus the live stream, bottom-anchored and top-clipped (qwen's
  `overflowDirection:"top"`); on overflow the top row shows a `… Ctrl-S to show
  more` marker (the marker is wired; the Ctrl-S expand handling is deferred).
- **PageUp/PageDown and the mouse wheel no longer scroll the transcript** - they
  lose meaning under native scrollback and are inert in the Screen (they remain
  in `Key` only for the pre-agent Session Picker, which stays on the alt-screen
  path). The Screen's scroll effects (`PinBottom`/`ScrollUp`/`ScrollDown`), the
  `ScrollStep` enum, `viewport.rs`, and the scrollbar are deleted.

## Phase 6 update (thinking subject + compact mode)

- **Ctrl-T retirement COMPLETED.** ADR-0046 decided to retire the Ctrl-T
  settled-Thinking expand/collapse toggle, but Phase 1-5 left it inert in the
  code (`Key::ToggleThinking` + `Screen::thinking_expanded` still existed). Phase
  6 finishes the job: `Key::ToggleThinking`, its handler, `thinking_expanded`,
  and the collapsed one-liner render branch are DELETED. Ctrl-T now maps to
  `Key::Other` (crossterm arm removed), so it types nothing. There is no Ctrl-T
  in suspenders (qwen's Ctrl+T = `TOGGLE_TOOL_DESCRIPTIONS`, which suspenders does
  not adopt). The greeting line and its test now read `Ctrl-O toggles compact
  mode`.
- **Thought subject on the spinner (a faithful port with a bounded divergence).**
  The pure `Transcript::thought_subject()` fills the Phase-3 `SpinnerState.subject`
  seam (qwen `LoadingIndicator.tsx:72` `thought?.subject || currentLoadingPhrase`).
  qwen's `parseThought` (the first `**...**` bold subject) is ported verbatim, but
  suspenders' reasoning streams do NOT reliably emit `**bold**` subjects, so a
  three-fallback ladder is used: (1) the bold subject if present, else (2) the
  last non-empty line of the live reasoning (the streaming head), else (3) `None`
  → the spinner falls back to the whimsical Lull phrase. Spinner-only; the
  committed history keeps the raw Thinking text verbatim. Clear-timing is free:
  `streaming_thinking` empties between messages and the spinner only renders while
  Running, so the subject vanishes at Idle with no manual reset.
- **Compact mode (Ctrl+O) replaces the two expand toggles.** See ADR-0052. The
  one bounded exception to "never touch frozen scrollback" (`RedrawScrollback`,
  the qwen `refreshStatic` analogue) lands there; the faithful scrollback purge
  proved un-portable through ratatui's `Backend` trait, so the shipped behaviour
  is the design's documented degraded fallback (the pending region + all future
  commits use the new compact; the already-frozen prefix keeps the compact it was
  blitted at).
