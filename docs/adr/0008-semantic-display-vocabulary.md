# Presentment speaks a semantic display vocabulary, not raw markup

A Presenter's Presentment callback (ADR-0007) needs to show rich output - a diff,
not a one-line summary - without breaking ADR-0001's boundary: the Screen core
(and the Transcript store it owns, ADR-0034) is pure and semantic, and
terminal markup exists only in the adapter. So the core defines one structured
Transcript item type - a titled block of lines with semantic styles (added,
removed, context, emphasis, muted) - and Presenters compose within it. One module owns the single mapping from semantic styles to
terminal colors, the same move as the Steering Vocabulary: one module owns the
mapping, everyone else speaks semantics.

## Considered Options

- **Presenter-owned rendering (a render callback returning terminal markup).**
  Maximum display freedom, rejected: markup leaks into every Presenter, the pure
  core holds opaque payloads it cannot inspect or test, and ADR-0001's
  boundary dies quietly.
- **One ad-hoc item type per Presenter (a bespoke diff variant).** Smallest step,
  rejected: every future Presenter with display needs reopens the question, and
  the adapter grows a renderer per Presenter anyway.

## Consequences

Presenters can only express what the vocabulary can say. If a future Presenter
needs a table or interactive element, the vocabulary grows in core - a
deliberate chokepoint, so display capability is added once and every Presenter
gets it.
