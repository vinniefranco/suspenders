# Compact mode (Ctrl+O): one display toggle over the transcript

qwen-code has ONE display toggle over the transcript: `compactMode`
(`Ctrl+O` = `TOGGLE_COMPACT_MODE`), default off. When on it hides Thinking items
entirely (`HistoryItemDisplay` gates on `!compactMode`) and hides tool RESULT
bodies while keeping their headers (`ToolMessage`: `!compactMode ||
forceShowResult`). Suspenders matches: one `Screen::compact_mode: bool` (default
`false` = show everything), toggled by `Ctrl+O` → `Key::ToggleCompact`. There
are no per-item expand toggles and no Ctrl-T (ADR-0046).

`compact` threads through the ONE `message_lines` → `RenderCache` →
`grouped_rows` path the whole-transcript render draws (ADR-0046), so compact is
applied uniformly to every item each frame by construction. Its effect on each
item, faithful to qwen:

- **Thinking**: hidden ENTIRELY under compact (zero lines). There is no
  collapsed one-liner - qwen only ever show/hides a thought.
- **Diff / Todo (tool result bodies)**: folded to the header row under compact;
  the header always stays.
- **ToolCall / ToolResult / User / Assistant / Info / Marker**: untouched (they
  are single header/text rows already, or not a tool result body).

Flipping `compact_mode` needs no effect: under the fullscreen model the next
full-frame redraw renders the whole transcript at the new compact (ADR-0046) -
there is no frozen prefix to reconcile, so the toggle cannot split-brain the
history. The `Toggles { compact }` value is a `RenderCache` key: either flip
clears the cache wholesale.

Considered and rejected:

- **Two independent expand toggles** (a per-kind Ctrl-T for Thinking and Ctrl-O
  for tool bodies, each defaulting collapsed and expanding to the full body).
  The inverse polarity of qwen and a different key map; ADR-0046 decided
  suspenders is a faithful qwen port, so qwen's single hide-toggle wins.
