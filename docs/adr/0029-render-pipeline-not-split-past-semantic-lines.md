# The render pipeline is not split past the existing semantic-line seams

Architecture reviews recurringly propose deepening the render path by
extracting either the `RenderCache` into a frame-free module or the per-item
line builders into a `TranscriptItem -> semantic line` layer. Both look like
the same move ADR-0008 made (semantics in the core, colors in the adapter).
Neither survives the deletion test. This ADR records why, so the next review
stops re-suggesting it.

## Decision

The render pipeline is deliberately NOT split further. Two boundaries already
carry the pure semantics:

- `ui/markdown` folds assistant text into `MdLine`/`MdStyle` (pure, tested);
- `DiffSide`/`DiffLine`/`DiffHunk` (defined in `view_model`, ADR-0044) is the
  ADR-0008 first-class `Diff` vocabulary.

Everything left in `ui/components` (a directory of cohesive submodules -
`message`, `tool_group`, `render_cache`, and their siblings) is display chrome
bonded to ratatui - gutters, glyphs, and the per-variant `Color`, which are
ratatui `Style`/`Span` by nature and confined here by ADR-0019.

The `RenderCache` stays bonded to ratatui: it caches per-item `Line`s and
wrapped row counts for the fullscreen transcript body (ADR-0046), measuring
with ratatui's own wrapping, whose invariant is that measuring and drawing must
agree EXACTLY (measure==draw). A pure, char-based re-implementation of wrapping
is the one change guaranteed to drift from what ratatui paints - the bug the
current design exists to prevent. Every body renderer follows the same rule:
one `content_area.width` feeds the measure, the wrapped count, and the draw.

## Considered options

- **Extract `RenderCache` into a frame-free module.** Rejected: its
  correctness is bonded to ratatui's wrapper (above). Moving it either amends
  ADR-0019 to bless another ratatui-touching module for no test-surface gain,
  or reintroduces the measure/draw divergence hazard.
- **Extract a `TranscriptItem -> Vec<semantic line>` builder.** Rejected as a
  middleman. `DiffLine` cannot carry assistant markdown (that is the richer
  `MdLine`/`MdStyle`), so a unified return type means inventing a NEW bridge
  vocabulary spanning `DiffSide` and `MdStyle` whose only job is to be
  converted to ratatui one line later. The deletion test comes back shallow:
  it moves lines and adds a type, concentrating nothing.

## Consequences

- New display capability grows at the vocabularies that already exist
  (`MdStyle` for markdown, `DiffSide` for the `Diff` item) - the ADR-0008
  chokepoint - not by inserting a semantic-line layer between `TranscriptItem`
  and ratatui.
- `ui/components` stays the one place drawing happens; its size is not itself
  a deepening signal. The pure semantics were already lifted out; what remains
  is the adapter ADR-0019 confines there.
- This ADR does not forbid splitting the pure core - streaming is the
  Transcript store's private child and the prompt-history ring lives in
  `ui/composer`. Those ARE pure, ratatui-free, and materialize into
  `TranscriptItem`s - they clear the deletion test the render pipeline fails.
