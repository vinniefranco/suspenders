# Fullscreen rendering with app-owned history, a faithful qwen-code port

We are abandoning Suspenders' bespoke TUI presentation and porting qwen-code's
UI/UX wholesale (flow, look, function, displayed stats). qwen-code (Ink/React)
renders **inline** into the normal terminal buffer: finished output is committed
once to the terminal's own scrollback and only a small pending region at the
bottom repaints. Suspenders first ported that literally - a `Viewport::Inline`
viewport plus `insert_before` to blit finished Transcript items into native
scrollback. That port shipped through Phase 6, then proved **structurally broken
on the ratatui + crossterm stack** (see "Why inline was abandoned" below), so the
rendering target changed to a fullscreen alternate screen where the app owns the
whole transcript and its scroll. The qwen-code LOOK and MECHANICS are unchanged;
only what the frame draws into differs.

## Decision

Render the whole app into a **fullscreen alternate-screen viewport**
(`ratatui::init()` / `Viewport::Fullscreen`, the same lifecycle the Session
Picker already used) and redraw the ENTIRE Transcript from the pure Screen model
every frame. There is no commit seam and no native scrollback: finished items,
in-flight Tool Calls, the streaming assistant message, the Lull spinner, the
sticky "Current tasks" box, the flat footer, and the Composer are all laid out
into the live frame each draw. History that scrolls off the top is reachable
through **app-owned scrolling** (mouse wheel, PageUp/PageDown, Home/End), clamped
to the live viewport at render time. Because every frame re-wraps from the model
at the current width and height, **resize simply re-lays-out** - there is no
frozen, unwrappable back-history to corrupt.

The transcript body is BOTTOM-ANCHORED in its zone and TOP-CLIPPED on overflow
(qwen's `MaxSizedBox overflowDirection:"top"`): the newest rows always show while
following the tail. Scrolling up detaches from the tail (new streaming output no
longer yanks the view down); reaching the bottom or pressing End re-attaches.
Readable content (prose, diffs, tool output, the header panel) is capped at
`MAX_CONTENT_WIDTH = 100` columns left-aligned (qwen `mainAreaWidth =
min(terminalWidth - 4, 100)`); the footer rule spans the full width.

Component chrome, wording, and stats are copied verbatim from qwen-code, strings
included (`Apply this change?`, `Yes, allow always`, `auto-accept edits
(shift + tab to cycle)`, `N% used`, `esc to cancel`). The Approval prompt is an
inline numbered block in the transcript body, not a centered modal.

This is a revamp, not a restyle: qwen-code's **mechanics** are adopted, not just
its colors. Thinking is a rolling thought subject on the spinner line that
replaces the waiting phrase while the model thinks (no Ctrl-T, no expand/collapse
of settled Thinking - both retired). Menus (Slash Command, model, theme) adopt
qwen-code's interaction model: arrow-key navigation, type-to-search filtering,
`Press Enter` to select. The `todo_write` task list adopts qwen-code's
visualization - circle glyphs (retiring the `[ ]`/`[~]`/`[x]` brackets in
`plan.rs`), the `TodoWrite` Tool Call list, and a live "Current tasks" box
(numbered, in-progress floated to top, completed struck through, capped with an
"... and N more" tail). Where a Suspenders menu, thinking, or task-list behavior
differs from qwen-code's, qwen-code's wins.

## Why inline was abandoned

The literal inline port (`Viewport::Inline` + `insert_before`) is incompatible
with Suspenders' async input, and native scrollback cannot reflow:

- **The cursor-position crash.** ratatui's inline viewport calls crossterm
  `get_cursor_position()` (writes `ESC[6n`, waits for the terminal's reply) at
  construction, on every `insert_before`, and on every resize. Suspenders drives
  input with crossterm's async `EventStream`, which parks a background thread
  blocked in `poll_internal(None, EventFilter)` whenever idle; that thread
  swallows the cursor-position reply (its filter discards it as an internal
  event), so the query times out after 2s with `Error: The cursor position could
  not be read within a normal duration` and the run dies. crossterm documents
  this: `position()` "will block and possibly time out while `event::read` or
  `poll` are being called." `EventStream` + inline `insert_before` cannot coexist.
- **Native scrollback cannot re-wrap.** Committed rows live in the terminal
  emulator's scrollback; shrinking the window chops frozen wide content (the
  83-column wordmark shattered). The app cannot re-wrap what it no longer owns.
- **Fixed live-region height.** `Viewport::Inline(h)` fixes the height at
  construction from the STARTUP terminal size, so a tall terminal was never
  filled and a small-then-grown terminal stayed cramped.

A fullscreen viewport makes NO cursor-position reads (so the async `EventStream`
is safe), redraws everything from the model each frame (so resize and the width
cap just work), and fills the terminal by construction. The cost - losing native
terminal scrollback in favour of app-owned scroll - is accepted; it is how most
TUIs and the Session Picker already behaved.

## Considered and rejected

- **Restyle only** - keep the old alternate-screen Viewport and rewrite component
  colors to look like qwen-code. Cheaper, but leaves an opaque scrollable pane; it
  reproduces qwen-code's look without its flow, which was the point. Rejected.
- **Inline scrollback (the original port).** Faithful to Ink's model, but crashes
  on the ratatui + crossterm stack and cannot reflow on resize (above). Rejected.
- **Sync input to keep inline.** Dropping `EventStream` for a single-threaded
  sync poll loop would serialize cursor reads and keep native scrollback, but it
  still cannot reflow committed content on resize and reworks the whole async
  loop. Rejected in favour of the simpler, more robust fullscreen model.

## Consequences

- **Retired:** the Suspenders-owned Viewport (`viewport.rs`), the old scrollbar,
  ADR-0040's turn lane + marker plane and standalone live-reasoning tail, the
  Ctrl-T settled-Thinking toggle, AND the entire inline commit seam
  (`insert_before`, the `Transcript` `committed` high-water mark /
  `committable_upto` / `mark_committed`, `Effect::Commit` / `PeekPending` /
  `RedrawScrollback`, `render_committed_slice`, and the Ctrl-S "peek" into
  scrollback). ADR-0040 is superseded.
- **Revised in place, not superseded:** ADR-0034 (the Screen / Transcript /
  Composer split holds; the Transcript is now rendered whole each frame),
  ADR-0029 (measure==draw still holds; the render pipeline now draws the whole
  transcript into the live frame instead of committing slices), ADR-0041 (the
  Lull draws on the transcript body's spinner line), ADR-0052 (compact mode no
  longer needs a scrollback-redraw workaround - see below).
- **Kept, reskinned into qwen-code's layout:** the Theme system, the whimsical
  Lull phrases, and the syntect + red/green-tint diff.
- App-owned scrolling replaces native scrollback as the history mechanism:
  PageUp/PageDown, the mouse wheel, and Home/End scroll the transcript body;
  Ctrl-S is repurposed as a keyboard page-up. Any operation that edits
  not-yet-final items (a Tool Result absorbing its paired Call, Steering
  promotion) is a plain model edit now - there is no commit boundary to stay
  behind.

## Implementation notes

- **Fullscreen lifecycle.** `run()` builds the terminal with `ratatui::init()`
  (enters the alt-screen + raw mode + a restoring panic hook) plus
  `EnableMouseCapture`, and tears down with `DisableMouseCapture` +
  `ratatui::restore()` - the same lifecycle `pick_session` already used. No
  `Inline(cap)`, no `insert_before`, no trailing newline.
- **Whole-transcript render.** `components::render_pending` (name kept) draws the
  WHOLE transcript through the unchanged `grouped_rows` fold (start index `0`)
  plus the live stream, bottom-anchored and top-clipped. `render_pending_body_at`
  computes ONE `content_area.width = content_width(area.width)` (capped at
  `MAX_CONTENT_WIDTH`) and feeds it to the measure, the wrapped count, and the
  draw, so measure==draw holds (ADR-0029).
- **App-owned scroll.** The pure Screen holds the scroll INTENT (`scroll_lines`
  from the bottom + `follow_tail`); `components::scrolled_clip` resolves it against
  the live viewport each frame (`max_scroll = total - height`, clamped), so a
  grown terminal auto-re-clamps. PageUp/PageDown use the last rendered body height
  the adapter records via `Screen::note_body_height` (kept pure - the renderer
  never writes back). Following the tail is byte-identical to the bottom-anchored
  first-cut behaviour.
- **Compact mode (Ctrl+O).** Flipping `Screen::compact_mode` is enough: the next
  full-frame redraw renders every item at the new compact, so the old
  `RedrawScrollback` / qwen `refreshStatic` workaround (ADR-0052) is retired -
  there is no frozen prefix to reconcile.

## Phase 6 update (thinking subject + compact mode)

- **Ctrl-T retirement COMPLETED.** The Ctrl-T settled-Thinking expand/collapse
  toggle is deleted (`Key::ToggleThinking`, its handler, `thinking_expanded`, and
  the collapsed one-liner render). Ctrl-T types nothing. The greeting reads
  `Ctrl-O toggles compact mode`.
- **Thought subject on the spinner (a faithful port with a bounded divergence).**
  `Transcript::thought_subject()` fills the `SpinnerState.subject` seam (qwen
  `LoadingIndicator.tsx:72` `thought?.subject || currentLoadingPhrase`). qwen's
  `parseThought` (the first `**...**` bold subject) is ported verbatim, but
  suspenders' reasoning streams do not reliably emit `**bold**` subjects, so a
  three-fallback ladder is used: (1) the bold subject if present, else (2) the
  last non-empty line of the live reasoning, else (3) `None` → the whimsical Lull
  phrase. Spinner-only; the history keeps the raw Thinking text verbatim.
- **Compact mode (Ctrl+O) replaces the two expand toggles.** See ADR-0052. Under
  the fullscreen model the flip needs no effect at all (above).
