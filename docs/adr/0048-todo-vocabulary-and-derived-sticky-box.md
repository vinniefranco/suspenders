# The todo vocabulary lives in plan.rs; the sticky box derives from the Transcript

The model's task list arrives as a `todo_write` Tool Call carrying a `todos`
array (each a `content` string and a `status`). qwen-code renders that list two
ways - the inline circle list (`TodoDisplay`/`TodoItemRow`) and the sticky
"Current tasks" box above the composer (`StickyTodoList`) - and suspenders
matches both, without introducing a second source of truth for the task list.

## Decision

**`plan.rs` owns the todo vocabulary; every consumer reads it, none re-derives
it.** `TodoItem` / `TodoStatus` carry `Serialize`/`Deserialize` (`snake_case`
status, matching the `todo_write` wire tokens) and `parse_todos` is
`pub(crate)`, so every consumer - the `todo_write` Tool, the Transcript render,
and the sticky box - shares one parse and one value type. A producer and
consumer that disagree fail to compile.

**The `todo_write` Tool attaches its own display Artifact (ADR-0007).** On a
successful call the Tool parses its input with `plan::parse_todos` and attaches
a `todos` Artifact to the Tool Result; the Transcript store's Presentment
(`swap_for_display`) substitutes a first-class `TranscriptItem::Todo { items }`
for the plain summary. Because the Todo item carries no `key_arg`, a raw JSON
summary cannot appear - the rich render holds by construction.

**The sticky "Current tasks" box DERIVES from the Transcript's latest `Todo`
item, not a separate Agent→view channel.** `Transcript::latest_todo()` rev-finds
the newest `TranscriptItem::Todo` (the same value the inline render drew), and a
pure predicate `sticky_todos(latest, total)` decides visibility: the box shows
iff the list is non-empty, not all-completed, and the Todo is NOT the newest
Transcript item (`latest_index + 1 < total`) - while it is the newest item it
renders inline right above the Composer, so the sticky box would double it.
The `Todo` item IS the single source of truth all consumers read.

## Consequences

- `plan.rs` and `view_model.rs` stay ratatui-free (ADR-0019): `plan.rs` carries
  the serde derives and the `str` glyph (`○ ◐ ●`); `view_model.rs` has a
  `Todo { items }` variant carrying `plan::TodoItem`s; the colour/glyph
  treatment (in_progress green, completed Foreground + strikethrough - qwen
  colours completed Foreground, NOT green) lives in `ui/components`.
- `Todo` is a tool item (`is_tool_item`), so it flows the same `message_lines`
  → cache → `grouped_rows` path as every transcript item (ADR-0046). The sticky
  box + spinner line are LIVE overlays - uncached, never in `grouped_rows`.
- measure==draw (ADR-0029): every row (the circle list, the sticky box, the
  spinner, the composer chrome) is funneled through `box_row`/`push_cols`/
  `wrap_words`/`truncate_visual` so the wrapped count equals the drawn rows.

## The LoadingIndicator keeps the lull scene as phrase content

The spinner line (`spinner_line`, qwen `LoadingIndicator.tsx`) shows whenever a
Run is Running and carries the elapsed timer + `esc to cancel` affordance + an
optional `↑/↓ Nk tokens` figure. It DIVERGES from qwen's `usePhraseCycler` (the
100+ witty phrase list): it keeps suspenders' whimsical lull scenes (ADR-0041)
as the phrase content, chosen per-lull, displaced by the thought subject while
the model thinks (ADR-0046). The spinner line is the one carrier - there is no
separate lull row.
