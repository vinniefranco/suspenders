# Phase 3 design — pending region + committed todo render

Reskin the pending region to qwen v0.16.0 and fix the committed todo render (the
live-vet defect: `todo_write` dumps raw JSON args instead of the circle list). All
ratatui stays in `components.rs`/`ui.rs` (ADR-0019); the Todo extension is pure like
`diff.rs`. Ground truth: `docs/qwen-ui-reference.md` §3 + qwen source at
`/home/vinnie/Sandbox/qwen-code-v0.16.0/packages/cli/src/ui/`. Preserve Phase 1
committed==pending identity + ADR-0029 measure==draw.

## 1. Committed todo render (the defect) — a Todo extension mirroring Diff
- New `TranscriptItem::Todo { items: Vec<plan::TodoItem> }` (reuse plan types; pure).
- `src/extensions/todo.rs` (mirror `extensions/diff.rs`): Middleware `post_run` on a
  successful `todo_write` reads the call input `todos` via `token.input`, parses with
  `plan::parse_todos` (make it `pub(crate)`), attaches a `todos` Artifact
  (`TodoArtifact { items }` — add `#[derive(Serialize,Deserialize)]` +
  `#[serde(rename_all="snake_case")]` on TodoStatus). Presenter `present` swaps a
  successful `todo_write` ToolResult → `TranscriptItem::Todo { items }`. Malformed/
  empty/errored/other-tool → passes through untouched.
- Register `"todo"` in `extensions.rs::build` (with_middleware + with_presenter) AND
  add `"todo"` to the shipped extension config list (else it silently no-ops — risk #5).
- Render: `is_tool_item` += `Todo` (groups into the box); `message_lines` routes Todo
  to `tool_inner_lines`; new `tool_todo_lines` = clean header `✓ todo_write` (empty
  desc — the raw-JSON key_arg is gone STRUCTURALLY since Todo has no key_arg) + circle
  list indented in the box: glyph `TodoStatus::glyph()` (○/◐/●), in_progress
  `success_style` (green) else `primary_style`, completed `Modifier::CROSSED_OUT`,
  3-wide gutter, content word-wrapped to `inner-3` (measure==draw). NOTE qwen quirk:
  completed is Foreground (not green) + strikethrough; only in_progress is green.

## 2. Sticky "Current tasks" box — derive from the Transcript (qwen findLatestTodoSnapshot)
- DECISION: read the latest `TranscriptItem::Todo` from the Transcript, NOT a new
  Agent→view Plan channel (the Plan flows as a rendered String today; re-parsing it
  back would duplicate the source of truth). Add pure `Transcript::current_todos() ->
  &[TodoItem]` scanning committed+pending (rev-find the last Todo item).
- Show/hide: show iff non-empty AND not all-completed AND the latest Todo item's index
  `< committed_high_water()` (it has committed to scrollback, so the inline copy no
  longer redraws — collapses qwen's pending/recent guards onto the ADR-0046 high-water
  fact; avoids double-render). Pure predicate `sticky_todos(items, hw) -> Option<&[..]>`.
- Layout: new zone between body and composer in `frame_chunks`; height =
  `2 + 1 + min(visible,5) + overflow?1:0`. `render_sticky_todos`: rounded box (reuse
  `box_row`) marginX 2 paddingX 1; header bold `"Current tasks"` in `secondary_style`
  (GREY bold, not accent); order via `STICKY_TODO_STATUS_PRIORITY` {in_progress0/
  pending1/completed2} stable by original index; number label = ORIGINAL index+1; glyph
  col 2, content truncate-end; in_progress green else primary; completed crossed-out;
  cap 5; overflow `"... and {N} more"` secondary. All pure line-builders.
- The sticky box + spinner line are LIVE overlays (uncached, pending-only, never in
  grouped_rows/committed slice) — like the current lull row. Only `Todo` is committable.

## 3. Spinner line (LoadingIndicator)
- `spinner_line(anim, subject: Option<&str>, tokens: Option<u64>, receiving, width, theme)
  -> Vec<Line>`, paddingLeft 2. Compose: `<SPINNER braille> <subject-or-phrase>  (<elapsed>
  [· <arrow> <tokens> tokens] · esc to cancel)`. Reuse existing `SPINNER` array +
  `anim.spinner`; phrase from `lull.rs` (keep the whimsical scene as phrase content —
  a deliberate divergence, ADR note). `subject` (Phase 6 thought subject) wins when Some
  = qwen `thought?.subject || currentLoadingPhrase`. elapsed via `lull::format_elapsed`
  (quiet-ticks for now). tokens `format_token_count` (1234→"1.2k"), arrow `↑` else `↓`
  (receiving = streaming_text non-empty). `esc to cancel` secondary. Shows whenever
  status==Running (not gated on no-streaming); replaces the lull-row append.
- SEAMS/deferrals: true turn-elapsed clock (Anim.turn_ticks) + live streaming-token
  counter → polish; ship `tokens: None` initially to avoid per-frame jitter.

## 4. Footer
- DECISION: keep suspenders' powerline status bar as substrate; add the ONE genuinely-
  qwen figure it lacks: `context_percent_label(tokens, budget, width)` implementing
  `formatPercentageUsed` (>1→">100" else (p*100).toFixed(1)) + label (`% used` if
  width<100 else `% context used`), `error_style` over-limit else secondary. Add a
  `StatusSegment::Context` in the right group (keep raw tokens too).
- DEFER: AutoAcceptIndicator/approval-mode → Phase 4 (leave a left_bottom seam);
  flat-vs-powerline restyle + sandbox/debug/worktree pills → Phase 7. Do NOT re-theme
  the bar now.

## 5. Composer chrome
- `> ` prompt in `accent_style` (was `› `); `!`/`*` mode variants = Phase-4 seam.
- Top dash rule (`─`×content_width, border_style; focused→border.focused) with a
  `top_right_label: Option` seam (no session-name concept yet → bare rule).
- Bottom-only border = `─`×width row below the draft. Grows composer zone by 2 rows —
  update `capped_composer_height`/`frame_chunks`; fix `set_cursor_position` +1 y offset
  (the one correctness-critical arithmetic — unit-test cursor row).
- Placeholder `  Type your message or @path/to/file` (two leading spaces) in secondary,
  first glyph `Modifier::REVERSED`, when draft empty.
- Queued-message view: defer (steering already handles queued text) — seam.

## 6. Invariants
- ADR-0019: plan.rs (serde + pub(crate) parse) and view_model.rs (Todo variant) stay
  ratatui-free; extensions/todo.rs pure like diff.rs; all render in components.rs.
- committed==pending identity: Todo flows the same message_lines→cache→grouped_rows path;
  sticky/spinner are pending-only overlays (never committable). Every new row funneled
  through box_row/push_cols/wrap_words to ≤ width (measure==draw).

## 7. ADR
- ADR-0048 (write it): todo vocabulary owned by `plan.rs`; the Todo display extension
  consumes it; the sticky box derives from the Transcript's latest Todo item, not a new
  Agent→view Plan channel. Records the single-source-of-truth choice (3 consumers: Run-
  loop Plan fold, committed render, sticky box) + why sticky reads the high-water mark.
- ADR note: LoadingIndicator keeps suspenders' lull scenes as phrase content (divergence
  from qwen's usePhraseCycler) with a Phase-6 subject seam.

## 8. Tests
extensions/todo.rs (mirror diff.rs): artifact attach/skip, present swap/passthrough,
serde round-trip. plan.rs serde round-trip. components.rs pure: ordered_sticky (stable
priority sort, number=original index), sticky_height/cap5/overflow, show/hide predicate
(empty/all-completed/still-pending/committed), tool_todo_lines (glyphs/green/strikethrough/
≤width/clean header), spinner_line (subject-wins, arrow, esc, ≤width), context_percent_label
(>100/label-by-width), composer (placeholder, rule width, cursor +1). transcript.rs
current_todos. Golden: committed Todo byte-identical committed vs pending; full frame with
sticky+spinner+composer no height desync.

## 9. Checklist (green between steps; todo render first = fixes the defect)
1. plan.rs: serde derives + pub(crate) parse_todos + round-trip tests.
2. view_model.rs: `Todo { items }` variant; fix exhaustive matches (compiler-listed).
3. components.rs: is_tool_item+=Todo; message_lines route; tool_todo_lines + golden.
4. extensions/todo.rs + register + config "todo" + pub mod. End-to-end: todo_write→circle list.
5. transcript.rs current_todos; sticky order/height/predicate/render_sticky_todos + frame_chunks zone.
6. spinner_line + format_token_count; append in render_pending replacing lull row.
7. context_percent_label + StatusSegment::Context in status_bar right group + fit/paint.
8. composer reskin: > prompt, top rule, bottom border, placeholder, +2 height, cursor offset.
9. Full test/clippy/rustqual + manual smoke; write ADR-0048 + LoadingIndicator note.

## 10. Risks
1. Composer/sticky zone height math + cursor offset (HIGH) — keep height pure, unit-test cursor.
2. Sticky double-render (gate on latest-todo-index < high_water) — testable fact.
3. Serde vocabulary drift (snake_case + round-trip test).
4. Spinner elapsed/token semantics (quiet-ticks/prompt-tokens mislabeled) — ship None/quiet, polish later.
5. Config wiring miss ("todo" not registered → silent no-op) — end-to-end test through configured([...]).
