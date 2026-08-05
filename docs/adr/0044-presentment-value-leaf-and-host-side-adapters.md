# A Presentment value leaf and host-side adapters

Two placement rules keep the dependency direction pointing inward - `view_model`
and the domain vocabulary at the bottom, `run`, `ui`, and `agent` orchestrating
above them - with no cycle between the domain and the rendering layer:

1. **Presentment value types live in a dependency-free leaf, not in `ui`.**
   CONTEXT.md draws the line: Presentment decides WHAT a Transcript item is,
   rendering is the terminal drawing it. The value types that cross that seam -
   a marker's `Tone`, a selector's `SelectorRow`/`RowRole`, the
   `TranscriptItem` (and its `Diff`'s `DiffHunk`/`DiffLine`/`DiffSide`) the
   core produces for the display - are Presentment vocabulary, not rendering
   machinery. They live in `src/view_model.rs`, a leaf that imports nothing
   from the crate. The functional core produces them (an `Event` carries a
   `Tone` or a row list; a Tool's Artifact becomes a `Diff` of `DiffLine`s) and
   `ui` renders them; neither the core nor `view_model` depends on the
   rendering layer. The interactive widgets and the tone-to-colour mapping stay
   in `ui` - only the value types live in the leaf. A type's inherent `impl`
   lives with the type: the `TranscriptItem` fold predicates
   (`foldable_body`/`fold_title`) are in `view_model` with the enum.

2. **An adapter lives with its consumer, not with the port it fulfils
   (Ports and Adapters).** `run` declares the `RunDeps` port - what a Run needs
   from whatever host drives it (ADR-0011). `AgentDeps`, the concrete adapter
   that fulfils it by threading each effect over the Agent's `mpsc` and the
   injected `Llm`, lives in `src/agent/deps.rs` with the Agent that constructs
   it. `run::run` is generic over the port and takes a `Capture { model, llm }`
   the host builds its tooling from, rather than reaching into a particular
   adapter's fields, so `run` never imports `agent`.

The same principle places shared vocabulary generally: a type belongs to the
layer that owns its meaning - a leaf for vocabulary both producer and consumer
read, the consumer's module for an adapter. When a dependency cycle appears,
look first for a type defined in a module that imports the module using it, and
relocate it; reach for a behavioural pattern only when the problem is genuinely
about behaviour.

## Considered options

- **The Visitor pattern** (define `Event`/`TranscriptItem` in the domain and
  let the UI supply a visitor that handles each variant): rejected as the wrong
  tool for this language. Visitor earns its ceremony with open hierarchies or
  double dispatch; Rust's exhaustive `match` already gives compile-checked
  per-variant dispatch. Layering trouble here is a "where does the type live"
  problem, so the fix is relocation, not a behavioural pattern.
- **`Box<dyn Effect>` / trait-object Command for the effect interpreter and the
  run protocol**: rejected. In Rust this trades away enum exhaustiveness and
  zero-cost dispatch for dynamic dispatch and allocation, with no benefit the
  enum-plus-`match` interpreter does not already have.
- **`pub use` re-exports from old locations** (leave `ui::transcript::Tone` as
  an alias of `view_model::Tone`): rejected. A re-export keeps the old import
  path alive, so the domain module still names the rendering layer and the
  cycle edge survives; it also leaves two public paths for one type. Every
  importer names the single canonical path.
- **Widening the `RunDeps` port with `model()`/`llm()` accessors**: rejected in
  favour of the `Capture` bundle. The port is about *effects*; the Model and
  Llm are *tooling inputs* the host supplies once.

## Consequences

- `src/view_model.rs` is the one home for value types the functional core
  hands the display. A new such type (a marker kind, a selector row shape)
  belongs there, imported by both the core that produces it and the `ui` that
  renders it - never defined under `ui`.
