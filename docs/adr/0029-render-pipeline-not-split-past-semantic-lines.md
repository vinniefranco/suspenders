# The render pipeline is not split past the existing semantic-line seams

An architecture review recurringly proposes deepening `ui/components` by
extracting either the `RenderCache` into a frame-free module or `message_lines`
into a `TranscriptItem -> semantic line` builder. Both look like the same move
ADR-0008 made (semantics in the core, colors in the adapter) and the same move
the status-bar and draft-cursor splits made. Neither survives the deletion
test. This ADR records why, so the next review stops re-suggesting it.

## Decision

The render pipeline is deliberately NOT split further. Two boundaries already
carry the pure semantics:

- `ui/markdown` folds assistant text into `MdLine`/`MdStyle` (pure, tested);
- `DiffSide`/`DiffLine`/`DiffHunk` (defined in `view_model`) is the ADR-0008
  first-class `Diff` vocabulary a Presenter composes within.

Everything left in `ui/components::message_lines` is one of two things, and
neither wants a new module:

1. **Display chrome bonded to ratatui** - the `"> "` gutter, the
   `⚙`/`🧠 thought:`/`✗` glyphs, and the per-variant `Color`. These are
   ratatui `Style`/`Span` by nature; ADR-0019 keeps them in `ui/components`.
2. **One pure decision** - the Thinking collapse rule (collapsed one-liner vs.
   expanded header + full text under Ctrl-T). If it ever needs isolated tests,
   a colocated pure helper is the whole fix; that is cleanup, not a deepening.

The `RenderCache` stays bonded to ratatui: it caches `Line<'static>` and
measures wrap counts with a throwaway `Paragraph::line_count`, whose own
invariant is that measuring and drawing must agree EXACTLY. A pure, char-based
re-implementation of wrapping is the one change guaranteed to drift from what
ratatui paints - the bug the current design exists to prevent.

## Presentment is a deep seam, not a shallow one

Do not read the "extraction comes back shallow" verdict above as a claim that
the whole display path is shallow. It is not. **Presentment** - the
`Presenter::present` seam (`TranscriptItem -> TranscriptItem`, folded across every
Presenter in `extensions::present`) - is a genuinely DEEP seam and is deliberately
kept intact for the opposite reason the extractions are rejected.

Run the deletion test on it: delete `Presenter::present` and every Presenter must
emit ratatui `Line`s directly, the diff vocabulary reappears inside each Presenter,
the Presentment substitution logic (the diff Presenter swapping a Tool Result summary
for a `Diff` item) duplicates across every site, and the panic-isolation that keeps
one Presenter's failure off the Transcript fragments. Complexity cascades across N
Presenters - the mark of a seam earning its keep, one semantic contract serving
all Presenters with colors living in one place (ADR-0019).

The distinction that matters:

- The `Presenter::present` **interface** is a deep seam Presenters participate in
  WITHOUT touching ratatui - keep it.
- The **vocabulary** it speaks (`DiffSide`/`DiffLine`, the `Diff` item) stays in
  the core because Presenters READ it, not export it. Lifting the vocabulary into
  its own module would scatter it across Presenters and tests and breach the
  ADR-0019 confinement - the same shallow move this ADR rejects for
  `message_lines`.

So the render pipeline is not split further, and the Presentment seam is not
collapsed inward - both for the same reason, read from opposite ends: depth
belongs where it already sits.

## Considered options

- **Extract `RenderCache` into a frame-free module.** Rejected: its
  correctness is bonded to ratatui's wrapper (see above). Moving it either
  amends ADR-0019 to bless a third ratatui-touching module for no test-surface
  gain, or reintroduces the measure/draw divergence hazard.
- **Extract a `TranscriptItem -> Vec<semantic line>` builder.** Rejected as a
  middleman. `DiffLine` cannot carry assistant markdown (that is the richer
  `MdLine`/`MdStyle`), so a unified return type means inventing a NEW bridge
  vocabulary spanning `DiffSide` and `MdStyle` whose only job is to be
  converted to ratatui one line later. The deletion test comes back shallow:
  it moves ~20 lines and adds a type, concentrating nothing.

## Consequences

- New display capability grows at the vocabularies that already exist
  (`MdStyle` for markdown, `DiffSide` for the `Diff` item) - the ADR-0008
  chokepoint - not by inserting a semantic-line layer between `TranscriptItem`
  and ratatui.
- The `ui/components` file stays large because drawing is genuinely there;
  its size is not itself a deepening signal. The pure semantics were already
  lifted out; what remains is the adapter ADR-0019 confines to this module.
- This ADR does not forbid splitting the pure core (streaming, prompt
  history) - and ADR-0034 since has: streaming is the Transcript store's
  private child and the prompt-history ring lives in `ui/composer`. Those ARE
  pure, ratatui-free, and materialize into `TranscriptItem`s - they clear the
  deletion test the render pipeline fails.
