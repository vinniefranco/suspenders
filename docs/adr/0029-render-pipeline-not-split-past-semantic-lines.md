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
- `LineStyle`/`StyledLine` (defined in `ui/transcript`) is the ADR-0008 Block
  vocabulary a Plugin composes within.

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

## Considered options

- **Extract `RenderCache` into a frame-free module.** Rejected: its
  correctness is bonded to ratatui's wrapper (see above). Moving it either
  amends ADR-0019 to bless a third ratatui-touching module for no test-surface
  gain, or reintroduces the measure/draw divergence hazard.
- **Extract a `TranscriptItem -> Vec<semantic line>` builder.** Rejected as a
  middleman. `StyledLine` cannot carry assistant markdown (that is the richer
  `MdLine`/`MdStyle`), so a unified return type means inventing a NEW bridge
  vocabulary spanning `LineStyle` and `MdStyle` whose only job is to be
  converted to ratatui one line later. The deletion test comes back shallow:
  it moves ~20 lines and adds a type, concentrating nothing.

## Consequences

- New display capability grows at the vocabularies that already exist
  (`MdStyle` for markdown, `LineStyle` for Blocks) - the ADR-0008 chokepoint -
  not by inserting a semantic-line layer between `TranscriptItem` and ratatui.
- The `ui/components` file stays large because drawing is genuinely there;
  its size is not itself a deepening signal. The pure semantics were already
  lifted out; what remains is the adapter ADR-0019 confines to this module.
- This ADR does not forbid splitting `ui/transcript` (streaming, prompt
  history): those ARE pure, ratatui-free, and materialize into
  `TranscriptItem`s - they clear the deletion test the render pipeline fails.
