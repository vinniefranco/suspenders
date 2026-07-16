# UI plan — conversation-forward transcript

Branch: `architecture-deepening`. Started 2026-07-16.

## Why
Vinnie's the 27B works well; the ask is **better UI**, not engine work. Root
observation (with a Claude Code screenshot as reference): suspenders
**over-indexes on tool calls** — they render in loud `Color::Yellow` at column 0
(`src/ui/components.rs:620-647`), the same visual plane as conversation, so a
burst of tool calls buries the model's prose in a wall of yellow.

## Guiding principle — two visual planes
- **Foreground** (bright, column 0): user prompts + assistant prose. Plus
  **errors** (red+bold) — the one kind of machinery that must never be missed.
- **Background** (dim gray, indented): all machinery — tool calls, results,
  diffs/Blocks, Nudges, Info lines, eviction/compaction waves, thinking.

Approved visual target (Vinnie picked "dim + indent + collapse, staged"):
```
Assistant prose — the foreground, bright, column 0, owns the screen.

  ⋯ read_file  src/foo.rs · 340 lines
  ⋯ edit_file  src/foo.rs (+3 −1) · ⌥o expand
  ⋯ run_command  cargo test · ✓ exit 0

More assistant prose, still dominant. The machinery sits quiet underneath.
```

## Status

### ✅ Stage 1 — plane inversion (DONE, gates green: 1121 tests, clippy clean)
All in `src/ui/components.rs`:
- Added `machinery_style()` (`DarkGray`, **not** italic — italic stays reserved
  for Thinking/Info).
- `ToolCall`, `ToolResult{is_error:false}`, `Block` title: yellow `⚙` →
  machinery style + `  ⋯ ` gutter.
- `ToolResult{is_error:true}`: kept red+bold (errors pop), added 2-space indent
  to column-align.
- `Block` body: kept semantic diff colors, indented 2 spaces under the gutter.
- Inter-turn spacing: one trailing blank `Line` per settled item, appended in
  `RenderCache::sync` **before** measuring `wrapped` (keeps viewport/scroll math
  consistent — do NOT inject in the flat_map, that desyncs `counts`).
- Updated 3 cache_sync tests (line-count deltas only).

### ✅ Stage 2 — collapse (detail-on-demand) (DONE, gates green: 1128 tests, clippy clean)
Multi-line `Block`s (diffs) fold to their one-line title with a `· ^O expand`
affordance; **Ctrl-O** toggles `tools_expanded` (global), exactly mirroring the
Ctrl-T / `thinking_expanded` pattern. Added a `Tools { expanded }` status-bar
segment (twin of `Thinking`) so the toggle has feedback with no blocks on screen.
Threaded through `message_lines` + `RenderCache` invalidation.

Adversarial review (Distinguished Rust Eng, 2026-07-16) — architecturally sound,
kept. Findings actioned: dishonest `· +N lines` count (it counted the
display-capped body while the title already carries the true `(+A −R)` delta) →
replaced with the fixed `· ^O expand` affordance; `machinery_style` doc-comment
slippage fixed. **Deferred decisions recorded here so Stage 3 inherits no rot:**
- **Global-per-kind, not per-item** (C1). Collapse is one screen-wide switch (like
  Thinking). The plan originally allowed "per-item (or per-kind)"; we took
  per-kind. Stage 3's per-item summaries are in tension with a global bool —
  before merging pairs, DECIDE: graduate to per-item expand state, or keep global
  and accept no per-item expand. Don't layer per-item state *next to* the global
  bool (two collapse models + doubled `RenderCache` invalidation = the rot).
- **Collapse predicate is structural, not semantic** (C2). It keys on
  `matches!(item, Block)`, so a large *non-diff* `ToolResult` (grep/read_file/
  run_command dumps — the real window-eaters) does NOT fold. Stage 3's pair-merge
  may produce a new item shape; re-key collapse on a "has foldable body" property
  so the merge is free to choose its shape without re-implementing the fold rule.
- **Flags-struct tripwire** (N1/N2/N4). Two global toggles now = two `bool`s on
  `message_lines`, a `too_many_arguments` allow on `status_bar`, and ~7 edit sites
  per toggle (field, `Key`, `StatusSegment`+`SegmentKind` twins, drop_order,
  `RenderCache` field+invalidation, `message_lines` param). Do NOT extract yet
  (only two instances). But the **third** detail-on-demand toggle (e.g. Bundle A
  waves) is the tripwire: at that point collapse the `bool`s into a
  `DisplayToggles { thinking, tools, … }` flags struct — that also drops
  `status_bar` back under the lint without re-hiding facts.
- **Not scroll-tested** across a collapse round-trip; add when per-item lands.

### ✅ Stage 3 — legibility within the plane (DONE, gates green: 1153 tests, clippy clean)
- **Pair-merge**: `ToolCall` carries a display-opaque `id` (the tool_use_id);
  its `ToolResult` finds the pending call by id (rposition, NEVER by
  position — parallel calls interleave), removes the redundant call line
  (bumping `messages_revision` — a non-append structural edit, mirroring
  `SteeringDelivered`), and recovers the call's `key_arg` onto the result BEFORE
  Presentment. Unpaired (governor-injected) results don't remove or bump and
  carry `key_arg = None`. Merged line: `⋯ read_file  src/foo.rs · 340 lines`.
- **Semantic fold (C2)**: `TranscriptItem::foldable_body(&self) ->
  Option<&[StyledLine]>` in the pure core is the collapse predicate;
  `message_lines` keys Ctrl-O on `foldable_body().is_some()`, not
  `matches!(Block)`. Today only a non-empty Block folds.
- **Exit badge**: `run_command::report`'s `[exit code: N]` tail gets an inverse
  `parse_exit_code` (+`parse_timed_out`), round-trip tested — the tail is now a
  single-sourced contract. New `plugins/run_command` plugin: `post_run`
  attaches an `exit_code`/`timed_out` Artifact (never mutates model content),
  `present` rewrites the summary to `✓ exit 0` / `✗ exit N` / `✗ timed out`.
  Registered in `build()` + shipped in the default plugin list
  (`session.rs:177`).
- **Cleaner summaries**: `key_arg(name, input)` picks path/command/pattern/query
  (else first sorted value); the live call line and the merged result both use
  it. C1 honored (collapse stays global, no per-item state); N1 honored (no
  DisplayToggles struct). Scroll round-trip test added (Stage 2 debt).
- **Known limitation** (C2 gap, accepted for Stage 3): a failing run_command
  shows `✗ exit N` but its stdout/stderr is no longer on the transcript line
  (still in model content) — large non-diff `ToolResult`s have no foldable body.

### ✅ Bundle A — context visibility (DONE, gates green: 1166 tests, clippy clean)
- **Wave visibility** (`transcript.rs`): explicit `EvictionWave` /
  `CompactionProgress` arms before the catch-all, both APPEND-ONLY (push a dim
  `Info` line, no `messages_revision` bump — the precondition that keeps the
  RenderCache incremental; a revision-unchanged test guards it). A pure
  `eviction_wave_line(&WaveStats)` recedes ONE terse line naming only the NONZERO
  counts plus the AT-WAVE (pre-reclaim) Dead Mass share as an integer percent
  (`context wave · 12% dead mass · 3 results, 1 read superseded, 2 husked`).
- **Dead-mass % in the status bar is LIVE** (`conversation.rs`, `event.rs`,
  `turn/loop_.rs`, `transcript.rs`, `components.rs`): the bar tracks the CURRENT
  dead mass, refreshed every pass on `Event::ContextPressure` (the same event
  that already drives the live token estimate/budget) — NOT a wave's pre-reclaim
  snapshot. New `Conversation::dead_mass()` computes the live fraction with the
  same formula `evict_traced` snapshots; `ContextPressure` carries a `dead_mass:
  f64`; `emit_context_pressure` fills it. `Transcript` stores it pre-rounded as
  `dead_mass_pct: Option<u64>`, set in the `ContextPressure` arm (the wave arm no
  longer touches the bar). Threaded through `TokenView`/`StatusSegment::Tokens`
  as `dead_mass_pct: Option<u64>`; the tail (`· N% dead`, shown even at a live
  `Some(0)`) is a segment FIELD with its own `cells()`, drawn from a shared
  `tokens_label()` so `cells`/`paint` stay in lockstep (the load-bearing fit
  invariant, tested with and without the tail). Arg COUNT stays at 8, so the
  `too_many_arguments` allow was NOT re-triggered and DisplayToggles extraction
  stayed unneeded; Tokens still drops as a unit, `kind()` unchanged.
- **Single rounding rule**: `conversation::dead_mass_pct(f64) -> u64` (rounds to
  nearest) is the ONE fraction→percent rule, shared by the status bar, the
  transcript wave line, and the engine's stdout wave/pressure prints (`app.rs`) —
  a test locks that the wave line and the bar agree for the same fraction.
- **Adversarial review actioned**: S1 (bar advertised the just-cleared
  pre-reclaim snapshot forever) fixed — dead mass is now live off
  ContextPressure. S2 (three different roundings) fixed — one `dead_mass_pct`
  helper. N1 (avoidable `f64` that forced dropping `Eq`) fixed — the UI stores a
  pre-rounded `u64`, so `Eq` is RESTORED on `StatusSegment`/`StatusBar`/`TokenView`
  and the earlier Eq-drop deviation is retired. Remaining minor test note: the
  `unknown_events_and_keys_are_ignored` test uses `Anchor` (still
  display-irrelevant) as its stand-in, since `CompactionProgress` is now folded.

**Preconditions from the Stage 3 adversarial review (do these AS PART OF Bundle
A, in this order):**
- **DisplayToggles extraction is now due if a 3rd toggle or 9th arg lands** (S2/
  N1). `status_bar` already carries `#[allow(clippy::too_many_arguments)]` (8
  args). Do NOT add a field to that already-suppressed signature. If Bundle A
  adds a detail toggle (or the dead-mass segment needs a 9th arg), FIRST collapse
  the `thinking`/`tools` bools into `DisplayToggles { thinking, tools, … }` — that
  drops the lint without re-hiding facts. The dead-mass **segment** itself is a
  `Vec<StatusSegment>` entry (no arg), so it alone does NOT trip this.
- **Wave arms must stay append-only** — push dim `Info` lines and do NOT bump
  `messages_revision` (keeps the RenderCache incremental). Only a non-append edit
  bumps; the pairing fold is the sole current bump site and is tested to bump
  ONLY on removal. `sync`'s `items.len() > messages.len()` guard catches shrink,
  not in-place edits — the revision bump is the real guard, so any future in-place
  wave edit MUST bump.
- **Dead-mass % is a `StatusSegment` field with its own `cells()` width**, baked
  in like `Tokens` — never recomputed in the painter, or `fit()`'s width math
  desyncs from what's drawn (same trap as the Stage 1 inter-turn blank).
- **N2 (deferred, not blocking)**: `key_arg`'s per-tool arg-name table
  (`transcript.rs`) mirrors tool input schemas. Graceful fallback (first-sorted
  key), so a new tool is only suboptimal. If it grows past ~6 tools, promote to a
  `salient_arg()` on the Tool trait instead of growing the table.

## Not doing / deferred
- **Persistent memory feature**: grilled and rejected (dense-small budget; the
  budget-safe form needs a model that self-initiates recall). See chat 2026-07-15.
- **Token-floor cache-fields bug**: real but deferred — see memory
  `token-floor-ignores-cache`. Independent of this UI work.
- **Bundle D** (composer/approval affordances: Esc in approval modal, slash
  "type to filter" hint, multi-line keybinding hints): not yet chosen; good
  cheap follow-up. Full 4-bundle survey is in the 2026-07-16 chat.

## Working-tree note
Uncommitted on this branch: Stage 1 (`components.rs`), the earlier thinking
indicator `~N tokens` change (`components.rs:365` + `tokens_for_chars` made
`pub(crate)` in `conversation.rs:671`), and pre-existing `M src/session.rs`.
Nothing committed yet.
