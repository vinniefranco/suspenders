# Tool groups are grouped at render time, not modeled in the core

qwen-code renders a Pass's tool activity as ONE rounded box - the
`ToolGroupMessage`: a single bordered container wrapping every tool call/result
in the group, one status marker + name + description per tool, results (diffs,
todos, text) indented inside the border. Suspenders' pure core (ADR-0044) carries
tool activity as a FLAT sequence of `TranscriptItem`s - `ToolCall`, `ToolResult`,
and first-class `Diff` items appended in order, with no "group" value. Phase 2 of
the qwen UI port (ADR-0046) needed that flat sequence to render as the box.

## Decision

**Group tool items into the box at RENDER time.** A maximal contiguous run of
`ToolCall` / `ToolResult` / `Diff` items is one qwen tool group; the render fold
(`grouped_rows` in `ui/components`) walks the settled items and boxes each such
run, passing everything else through as its cached lines. The pure core stays
group-free.

Two options were weighed:

- **Option A - model the group in the core.** Add a `ToolGroup { items }` variant
  (or a grouping pass in `ui/transcript`) so the store carries the box as a value.
- **Option B - group at render (chosen).** Leave the core flat; fold contiguous
  tool runs into a box purely in the render layer, over `&[TranscriptItem]`.

We chose **B**. The grouping is a pure fold over the flat transcript: a maximal
contiguous run of tool items becomes one box; any non-tool item breaks the run
and passes through on its own. The group is a pure view fold, so:

- The core keeps ONE flat append-only contract (ADR-0044) - no new variant, no
  grouping pass, no store state to keep in sync with the execution model.
- The fold reads `&[TranscriptItem]` values only, so it stays pure and testable
  without a frame, honoring the ADR-0019 boundary (all ratatui lives in
  `ui/components`).
- Committed and pending render through the SAME fold (`grouped_rows` is called by
  both `render_committed_slice` and the pending body), so a box is byte-identical
  live vs frozen - the Phase 1 committed==pending identity (ADR-0046) is preserved
  over groups.

Option A would have duplicated the execution model's sequencing into the display
value for no rendering gain, and would have made the identity guarantee harder
(the store, not the render fold, would own the box shape).

## A batch's box can be split by a mid-batch Info/Marker (accepted)

The flat-run heuristic is NOT lossless, and an earlier draft of this ADR was
wrong to claim it was. `batch.rs` does emit non-tool `Info` items INTO the
middle of a Pass's tool sequence:

- `run_lifecycle` (`src/run/batch.rs:207`) runs `extensions::pre_run` /
  `execute`, and `emit_extension_errors` emits an `Event::ExtensionError` for any
  extension failure; `request_approval`'s Standing-Approval path emits
  `Event::ApprovalAuto`. Both fire mid-tool, per tool.
- `screen.rs` turns `Event::ExtensionError` into `transcript.extension_failure`
  (`:690`) and `Event::ApprovalAuto` into `transcript.info` (`:725`) - both land
  as `TranscriptItem::Info`.

Combined with the supersede rule (`Transcript::tool_result` removes the pending
`ToolCall` and APPENDS the `ToolResult` at the tail, `src/ui/transcript.rs:216`),
a batch where a later tool logs such an `Info` can settle as
`[ToolResult-A, Info, ToolResult-B]`: the `Info` sits between two tool results.
`group_segments` renders that honestly as **two** boxes with the `Info` line
between them, not one box with an `Info` swallowed inside it.

We accept this split. The interleaved line is legitimately ABOUT the tool it
was emitted for (an extension that failed on that call, a standing approval that
covered that command), so surfacing it between the two boxes reads correctly.
There is no tear, no panic, no reorder: the fold is still append-only and
identity-preserving (committed == pending, the same `grouped_rows` on both
paths). A single Pass whose tools all run cleanly still renders as ONE box; only
a Pass that logged a mid-batch Info/Marker splits, and the split is meaningful.

If a future need arises to keep a batch visually unified ACROSS such an
interleaved line, that is a new grouping requirement (the core would have to
carry a batch id), i.e. Option A - not a defect in Option B's fold.

## Consequences

- The box is built uncached at assembly (cheap - tool headers are short). The
  expensive `Diff` syntect highlight stays cached per-item; only the border/pad
  wrapping is recomputed per frame. If profiling ever shows this matters, the fold
  can promote to a group-aware cache without changing the core (risk #5 in the
  Phase 2 design).
- The membership predicate (`is_tool_item`) and the border-colour precedence
  (shell -> `ui.symbol`, else `border.default`; a committed group is never
  pending, so the `status.warning` pending arm is a Phase 4 concern) live
  entirely in `ui/components`, next to the box drawing.
- A future need to render a group differently based on data the transcript does
  NOT carry (e.g. a subagent's nested group) would revisit this - but that is a
  new capability, not a reason to model today's flat run in the core.
