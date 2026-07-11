# Presentment speaks a semantic display vocabulary, not raw markup

A Plugin's Presentment callback (ADR-0007) needs to show rich output - a diff,
not a one-line summary - without breaking ADR-0001's boundary: the Transcript
is a pure semantic core and terminal markup exists only in the adapter. So the
core defines one structured Transcript item type - a titled block of lines
with semantic styles (added, removed, context, emphasis, muted) - and Plugins
compose within it. One module owns the single mapping from semantic styles to
terminal colors, the same move as the Steering Vocabulary: one module owns the
mapping, everyone else speaks semantics.

## Considered Options

- **Plugin-owned rendering (a render callback returning terminal markup).**
  Maximum display freedom, rejected: markup leaks into every Plugin, the pure
  core holds opaque payloads it cannot inspect or test, and ADR-0001's
  boundary dies quietly.
- **One ad-hoc item type per plugin (a bespoke diff variant).** Smallest step,
  rejected: every future Plugin with display needs reopens the question, and
  the adapter grows a renderer per plugin anyway.

## Consequences

Plugins can only express what the vocabulary can say. If a future Plugin
needs a table or interactive element, the vocabulary grows in core - a
deliberate chokepoint, so display capability is added once and every Plugin
gets it.
