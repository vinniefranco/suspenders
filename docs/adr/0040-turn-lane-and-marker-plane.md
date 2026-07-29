# A run is a visual object: the agent lane and the tinted marker plane

ADR-0008 made the Transcript a flat list of semantically-styled items and kept
all terminal markup in the adapter; ADR-0034 split the store but kept the flat
shape; ADR-0029 kept the render pipeline whole in the adapter. That flat list,
drawn with per-item glyphs and the two-planes coloring, reads as a stream of
disconnected events: a run never becomes one object, live reasoning shows only
as a `~N tokens` counter, and every harness event (compaction, result-cap cuts,
loop-detector close, Steering) draws as an identical `Info` line. This ADR
records the display principles that fix that, and the single schema change they
require.

## Decision

**Live reasoning is content, not a metric.** A running Run streams the tail of
its Thinking (the last few reasoning lines) under an animated `✦ Thinking`
header, and the running-spinner animation moves from the status bar to that
header - motion sits where the content and the user's cursor are. The status-bar
mode block collapses to a single dot. This is adapter-only: the store already
holds the real Thinking text in the streaming snapshot; the viewport simply
renders it instead of counting it.

**A lane is a user request, derived at render time.** Everything the agent
produces in service of one request - Thinking, tool machinery, the answer - hangs
off one dim vertical spine; the user's prompts break to the margin. A *lane-opener*
is a `User` item; every item until the next `User` hangs off that lane. This is
deliberately scoped to the user request, not the Run: any harness-produced work
without a fresh `User` item (its prompt is Voiced, not a `User` item) correctly
shares the originating request's lane. The region before the first `User` item
(the greeting, launch notices) is spineless by definition. The lane is computed by the adapter
from the existing item sequence, so NO run structure is stored and no revision
contract changes; the store stays the flat append-only list ADR-0034 defined.

Because the viewport is bottom-anchored and lives or dies on measure==draw
(ADR-0029), the spine is NOT a post-hoc prefix over measured lines - it is a
reserved fixed-width left gutter (the same move as the always-reserved scrollbar
column), unifying the user `›` gutter and the agent `│` spine. Content wraps and
is measured at the reduced width; lane membership is computed in the viewport's
assembly pass, never inside `message_lines` and never in the RenderCache key.

**A collapsed lane is a rolling window, not the full transcript** (design
confirmed hands-on against a live demo buffer, `Screen::demo()` / the
`dump_demo_render` test - the living spec). By default (thinking and tools
collapsed) a lane folds to a tidy, scannable shape via a render-time
`run_fold` pass over the item sequence (`FoldAction { Keep, Drop, Header, Elided }`):

- **The reasoning folds to ONE header: the LAST thought's text at the FIRST
  thought's slot.** The intervening thoughts `Drop`. This departs from the
  mockups' Decision C ("grouped, thinking-first, thinking stays in one place"):
  hands-on, the *latest* thought is the one worth surfacing (it reflects where
  the agent's reasoning landed), and a single header at the top of the run reads
  far denser than a preserved thinking block. Ctrl-T (`thinking_expanded`)
  disables the fold and shows every thought.
- **Low-signal machinery is a rolling window of the last `MACHINERY_WINDOW`
  (=4).** Non-error `ToolResult`/`ToolCall` one-liners older than the window
  `Drop`, replaced by a single `⋯ N earlier actions · ^O expand` count
  (`Elided(n)`) at the first windowed-out slot - a fold never *silently* hides
  work. Ctrl-O (`tools_expanded`) disables the window and shows every action.
- **Errors, `Diff` items, markers, assistant text and prompts always
  `Keep` - they break out** of both folds and are always shown. An error tool
  result is the one machinery item that stays foreground (red+bold, `⚙`, always
  a `✗`); code and diffs would be mangled by windowing.
- Each lane folds independently (delimited by `User` items).

**Flush vs. indent, and a dense spine.** The thought header and assistant text
sit FLUSH against the spine (`│ ` then content at column 0 of the content area);
tool machinery, markers, the `⋯ N earlier actions` count, and errors INDENT two
columns (a block indent, so a wrapped continuation stays at column 2, not the
margin - ratatui's `Wrap` has no hanging indent, so the machinery/marker arms
pre-word-wrap to `content_width - 2` and prefix every visual row). The lane is
DENSE: there is NO per-item blank separator row (an earlier design appended one),
so the `│` spine is continuous down the whole run - the two-planes coloring, not
whitespace, separates a run's parts.

**Harness events form one tinted marker plane, tone stamped at the firing site.**
Every Suspenders-authored marker carries a semantic tone: Housekeeping (compaction,
result-cap cuts), Aid (a marker that helps the model), Constrain (a guard that
limits it - the loop-detector's run-close), or Steering (the user's own voice).
Aid-vs-Constrain is the firing site's *intent* - a domain judgment - so the tone
is stamped where the marker is voiced and carried on the Event, the same shape as
an Artifact or Provenance: display-only data derived at the site that already
knows the intent. The Screen copies it onto a new `TranscriptItem::Marker { text,
tone }` item; the adapter tints by tone - Housekeeping neutral gray, Aid warm
amber, Constrain cool blue, Steering the prompt color (amber/blue chosen to dodge
the error-red / success-green palette). The adapter NEVER classifies tone from
text or variant - that would re-derive the firing site's judgment in the display
layer. The tone only tints a line in place; it never groups or reorders markers,
which stay in chronological order.

This is the one schema change, and it is cross-layer, not store-local: the tone
field on the marker-bearing Events, the `Marker` variant carrying it, and
ADR-0034's manual prefix-or-bump property test (markers append, never bump). It
is added at ADR-0008's deliberate vocabulary chokepoint. The Transcript is
display-only and is never persisted (the Session Log records the Conversation,
not the Transcript), so there is no decode path to protect and `Tone` needs no
serde - the `#[default] Plain` variant exists only for an unclassified `push`. A new `Marker` keeps `Info` for its
non-marker uses (greeting, notices, the recursion-bound extension-failure line) and
keeps Steering's removal-by-equality working once its anchor re-points to `Marker`.

## Considered and rejected

- **Speaker headers ("you" / model name per run).** Explicit grouping, but
  spends rows on labels and reads as a chat app, not a dense TUI.
- **Storing run boundaries in the Transcript.** Unnecessary: a boundary is
  derivable from `User`-item position, so grouping stays render-time and
  reversible, honoring ADR-0029's whole-pipeline-in-the-adapter boundary. (The
  next engineer will see harness-produced work share a lane and be tempted to
  "fix" it with a stored boundary - it is not a bug; the lane is a user request.)
- **`tone` on the `Info` item.** Rejected: `Info` carries non-marker uses (the
  greeting, launch notices, the extension-failure line that bypasses Presentment as
  the recursion bound), and the Steering pending marker is keyed by `Info`-text
  equality. A `tone` on `Info` forces a `Plain` sentinel onto all of them and
  makes the steering-removal breakage invisible to the compiler. A `Marker`
  variant isolates the plane.
- **The adapter inferring tone from the event or text.** Rejected: reconstructing
  Aid-vs-Constrain in the display layer re-derives the firing site's judgment
  outside it. Tone is stamped at the firing site and carried.
- **One flat color for all harness markers.** Calmest, but a guard firing (the
  loop-detector's close) reads identically to routine tidying - and watching
  those markers fire in the Transcript is a first-class tuning workflow here, so
  the Aid/Constrain tints earn their keep.
- **Warm/cold as a grouping.** Rejected explicitly: warmth tints a line, it does
  not cluster the markers. Grouping would break chronology and hide the causal
  order in which the harness acted.

## Consequences

The render pipeline stays in the adapter (ADR-0008, ADR-0029 intact); the lane
and the live tail are adapter-local and cheap to reverse. The tone is the only
cross-layer change (Event → Screen → store → property test) and is the part to
weigh: every future marker-bearing event must stamp a tone. Because the running
spinner moves to the `✦ Thinking` header, the status-bar mode dot is a static
running/idle color, not a pulse - motion lives at the brain by design, so a run
running while the user has scrolled away from the tail shows the running-color
dot without motion. That trade is deliberate (the user asked for the animation at
the brain, not the bar). This
redesign also carries one adjacent pure-core change that is NOT adapter-local: the
diff Presenter drops its line-number gutter (`extensions/diff/display.rs`), a change to
what the Presenter chooses to show (ADR-0008 leaves that to the Presenter) with its own
tests. (The diff's flat semantic-color rendering that this ADR assumed - the `+`/`-`
marker and a single semantic color carrying the whole line - was later superseded: the
diff became a first-class `Diff` item whose lines carry a `DiffSide`, rendered with an
added/removed background tint plus syntect foreground highlighting, per the revised
ADR-0008. The gutter stays dropped; the flat single-color-per-line rendering does not.) Cross-reference ADR-0029: the lane, tail, and the `run_fold` collapse all
land in `components.rs`, which 0029 twice refused to split past semantic lines -
the added code is not a signal to extract a `TranscriptItem`-to-line builder.

The collapsed fold is a display-time projection over the flat store, entirely
adapter-local and reversible: the store keeps every thought and action, and the
two toggles (Ctrl-T, Ctrl-O) reveal them. The confirmed shape is pinned by
`Screen::demo()` and the `dump_demo_render` / `the_demo_render_matches_...` tests,
which are the living spec (a future `--demo` CLI may surface the same buffer);
change the fold rules and those tests change with them. The one hazard the fold
adds is measure==draw: the synthetic header/count lines and the pre-word-wrapped
machinery/marker rows must each be `<= content_width` so the viewport never
re-wraps them, or `wrapped_count` desyncs from the drawn rows and the gutter slides
off its content (ADR-0029). That invariant is unit-tested at the `wrap_words` /
`indented_lines` seam and pinned end-to-end by the dense-spine render test.
