# The todo vocabulary lives in plan.rs; the sticky box derives from the Transcript

The model's task list arrives as a `todo_write` Tool Call carrying a `todos`
array (each a `content` string and a `status`). qwen-code renders that list three
ways in the pending region + committed history: the committed inline circle list
(`TodoDisplay`/`TodoItemRow`), the sticky "Current tasks" box above the composer
(`StickyTodoList`), and internally the run-loop folds it into its Plan. Phase 3 of
the qwen UI port needed the committed render (a live-vet defect: `todo_write` was
dumping raw JSON args instead of the circle list) AND the sticky box, without
introducing a second source of truth for the task list.

The harness already parses `todos` in ONE place: `plan.rs` (`parse_todos`), which
the Run loop folds into the Plan (`Plan::update`). Two questions:

1. How should the committed render and the sticky box get the parsed items?
2. Where does the sticky box read the "latest task list" from?

## Decision

**`plan.rs` owns the todo vocabulary; every consumer reads it, none re-derives
it.** `TodoItem` / `TodoStatus` gain `Serialize`/`Deserialize` (`snake_case`
status, matching the `todo_write` wire tokens) and `parse_todos` is `pub(crate)`,
so the three consumers - the Run-loop Plan fold, the committed render, and the
sticky box - share one parse and one value type. A producer and consumer that
disagree fail to compile.

**The committed render is a display Extension (`extensions/todo.rs`), mirroring
`diff.rs`.** A Middleware `post_run` on a successful `todo_write` parses the input
with `plan::parse_todos` and attaches a `todos` Artifact; a Presenter `present`
swaps the successful `todo_write` Tool Result for a first-class
`TranscriptItem::Todo { items }`. Because the Todo item carries no `key_arg`, the
raw JSON summary is gone STRUCTURALLY - the defect cannot recur by construction.
The extension is registered in BOTH `extensions.rs::build` and the shipped config
list (`session.rs`); a missing registration would silently no-op (a ranked risk).

**The sticky "Current tasks" box DERIVES from the Transcript's latest `Todo`
item, not a new Agent→view Plan channel.** The Plan already flows to the view as a
rendered String; re-parsing that back into structured todos for the sticky box
would duplicate the source of truth. Instead `Transcript::latest_todo()` rev-finds
the newest `TranscriptItem::Todo` (the same value the committed render drew), and
a pure predicate `sticky_todos(latest, high_water)` decides visibility. The
`Todo` item IS the single source of truth all three consumers read.

**The sticky box shows iff the todo has COMMITTED** (`latest_index <
committed_high_water()`), is non-empty, and is not all-completed. This collapses
qwen's `getStickyTodos` pending/recency guards (`MIN_HISTORY_ITEMS_AFTER_TODO_BEFORE_STICKY`)
onto the ADR-0046 high-water fact: while the todo is still pending it renders
inline above the composer, so the sticky box would double it; once it commits to
native scrollback the inline copy scrolls away and the sticky box takes over. The
high-water mark is a testable fact, so the anti-double-render rule is a pure
predicate, not a heuristic item count.

## Consequences

- `plan.rs` and `view_model.rs` stay ratatui-free (ADR-0019): `plan.rs` carries
  the serde derives and the `str` glyph (`○ ◐ ●`); `view_model.rs` gains a
  `Todo { items }` variant carrying `plan::TodoItem`s; the colour/glyph treatment
  (in_progress green, completed Foreground + strikethrough - qwen colours
  completed Foreground, NOT green) lives in `ui/components`.
- committed==pending identity (ADR-0046) holds: `Todo` is a tool item
  (`is_tool_item`), so it flows the same `message_lines` → cache → `grouped_rows`
  path and renders byte-identically committed vs pending (a golden test pins it).
  The sticky box + spinner line are LIVE overlays - uncached, pending-only, never
  in `grouped_rows`/the committed slice - like the old lull row.
- measure==draw (ADR-0029): every new row (the circle list, the sticky box, the
  spinner, the composer chrome) is funneled through `box_row`/`push_cols`/
  `wrap_words`/`truncate_visual` so the wrapped count equals the drawn rows.

## The LoadingIndicator keeps the lull scene as phrase content

The Phase-3 spinner line (`spinner_line`, qwen `LoadingIndicator.tsx`) shows
whenever a Run is Running and carries the elapsed timer + `esc to cancel`
affordance + an optional `↑/↓ Nk tokens` figure. It DIVERGES from qwen's
`usePhraseCycler` (the 100+ witty phrase list): it keeps suspenders' whimsical
lull scenes (ADR-0041) as the phrase content, chosen per-lull. A `subject:
Option<&str>` parameter is the Phase-6 seam for the thought subject (qwen
`thought?.subject || currentLoadingPhrase`); a `tokens: Option<u64>` parameter is
the seam for a live token counter, shipped `None` initially to avoid per-frame
jitter. The spinner subsumed the separate lull "waiting" row: the LoadingIndicator
now carries the lull scene, so `lull_visible`/`live_lull_lines` were retired.
