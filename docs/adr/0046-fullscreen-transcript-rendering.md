# Fullscreen alt-screen rendering: the whole Transcript redrawn from the model, app-owned scroll

Suspenders renders qwen-code's UI/UX (flow, look, function, displayed stats)
into a **fullscreen alternate-screen viewport** (`ratatui::init()` /
`Viewport::Fullscreen`, the same lifecycle the Session Picker uses) and redraws
the ENTIRE Transcript from the pure Screen model every frame. There is no
commit seam and no native scrollback: settled items, in-flight Tool Calls, the
streaming assistant message, the Lull spinner, the sticky "Current tasks" box,
the flat footer, and the Composer are all laid out into the live frame each
draw. History that runs off the top is reached through **app-owned scrolling**
(mouse wheel, PageUp/PageDown, Home/End), clamped to the live viewport at
render time. Because every frame re-wraps from the model at the current width
and height, **resize simply re-lays-out** - there is no frozen, unwrappable
back-history to corrupt.

The transcript body is BOTTOM-ANCHORED in its zone and TOP-CLIPPED on overflow
(qwen's `MaxSizedBox overflowDirection:"top"`): the newest rows always show
while following the tail. Scrolling up detaches from the tail (new streaming
output no longer yanks the view down); reaching the bottom or pressing End
re-attaches. Readable content (prose, diffs, tool output, the header panel) is
capped at `MAX_CONTENT_WIDTH = 100` columns left-aligned (qwen `mainAreaWidth =
min(terminalWidth - 4, 100)`); the footer rule spans the full width.

Component chrome, wording, and stats are copied verbatim from qwen-code, strings
included (`Apply this change?`, `Yes, allow always`, `auto-accept edits
(shift + tab to cycle)`, `N% used`, `esc to cancel`). The Approval prompt is an
inline numbered block in the transcript body, not a centered modal. qwen-code's
**mechanics** are adopted, not just its colors: menus (Slash Command, model,
theme) use its interaction model (arrow-key navigation, type-to-search
filtering, `Press Enter` to select), and the `todo_write` task list uses its
visualization - circle glyphs, the `TodoWrite` Tool Call list, and a live
"Current tasks" box (numbered, in-progress floated to top, completed struck
through, capped with an "... and N more" tail). Where a Suspenders menu,
thinking, or task-list behavior differs from qwen-code's, qwen-code's wins.
The Theme system, the whimsical Lull phrases, and the syntect +
red/green-tint diff draw inside this layout.

## Why fullscreen, not inline

qwen-code (Ink/React) renders inline into the normal terminal buffer; a
faithful inline port is structurally broken on the ratatui + crossterm stack:

- **The cursor-position race.** ratatui's inline viewport reads the cursor
  position (writes `ESC[6n`, waits for the terminal's reply) at construction,
  on every `insert_before`, and on every resize. Suspenders drives input with
  crossterm's async `EventStream`, which parks a background thread blocked in
  `poll_internal` whenever idle; that thread swallows the cursor-position reply,
  so the query times out and the app dies. crossterm documents the conflict:
  `position()` "will block and possibly time out while `event::read` or `poll`
  are being called."
- **Native scrollback cannot re-wrap.** Rows committed to the terminal
  emulator's scrollback are frozen; shrinking the window chops wide content,
  and the app cannot re-wrap what it no longer owns.
- **Fixed live-region height.** `Viewport::Inline(h)` fixes the height at
  construction from the startup terminal size, so a tall terminal is never
  filled and a small-then-grown terminal stays cramped.

A fullscreen viewport makes NO cursor-position reads (so the async
`EventStream` is safe), redraws everything from the model each frame (so resize
and the width cap just work), and fills the terminal by construction. The
cost - losing native terminal scrollback in favour of app-owned scroll - is
accepted; it is how most TUIs behave.

## Considered and rejected

- **Inline scrollback rendering** (commit finished items to native scrollback,
  repaint only a pending region). Faithful to Ink's model, but crashes on the
  ratatui + crossterm stack and cannot reflow on resize (above). Rejected.
- **Sync input to keep inline.** Dropping `EventStream` for a single-threaded
  sync poll loop would serialize cursor reads, but it still cannot reflow
  committed content on resize and reworks the whole async loop. Rejected in
  favour of the simpler, more robust fullscreen model.
- **Restyle only** - keep a bespoke layout and rewrite component colors to look
  like qwen-code. Cheaper, but it reproduces qwen-code's look without its flow,
  which is the point. Rejected.

## Implementation notes

- **Fullscreen lifecycle.** `run()` builds the terminal with `ratatui::init()`
  (enters the alt-screen + raw mode + a restoring panic hook) plus
  `EnableMouseCapture`, and tears down with `DisableMouseCapture` +
  `ratatui::restore()`.
- **Whole-transcript render.** `components::render_pending` draws the WHOLE
  transcript through the `grouped_rows` fold (start index `0`) plus the live
  stream, bottom-anchored and top-clipped. `render_pending_body_at` computes
  ONE `content_area.width = content_width(area.width)` (capped at
  `MAX_CONTENT_WIDTH`) and feeds it to the measure, the wrapped count, and the
  draw, so measure==draw holds (ADR-0029).
- **App-owned scroll.** The pure Screen holds the scroll INTENT (`scroll_lines`
  from the bottom + `follow_tail`); `components::scrolled_clip` resolves it
  against the live viewport each frame (`max_scroll = total - height`, clamped),
  so a grown terminal auto-re-clamps. PageUp/PageDown use the last rendered body
  height the adapter records via `Screen::note_body_height` (kept pure - the
  renderer never writes back). Ctrl-S is a keyboard page-up. Any operation that
  edits not-yet-final items (a Tool Result absorbing its paired Call, Steering
  promotion) is a plain model edit - there is no commit boundary to stay behind.
- **Thought subject on the spinner (a faithful port with a bounded
  divergence).** `Transcript::thought_subject()` fills the
  `SpinnerState.subject` seam (qwen `LoadingIndicator.tsx`
  `thought?.subject || currentLoadingPhrase`). qwen's `parseThought` (the first
  `**...**` bold subject) is ported verbatim, but suspenders' reasoning streams
  do not reliably emit `**bold**` subjects, so a three-fallback ladder is used:
  (1) the bold subject if present, else (2) the last non-empty line of the live
  reasoning, else (3) `None` → the whimsical Lull phrase. Spinner-only; the
  history keeps the raw Thinking text verbatim. There is no Ctrl-T and no
  per-item expand/collapse of settled Thinking.
- **Compact mode (Ctrl+O).** Flipping `Screen::compact_mode` is enough: the
  next full-frame redraw renders every item at the new compact - there is no
  frozen prefix to reconcile (ADR-0052).
