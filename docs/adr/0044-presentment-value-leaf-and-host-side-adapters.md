# A Presentment value leaf and host-side adapters: breaking the dependency cycle

`rustqual` (the quality gate) reported one nine-module circular dependency
spanning nearly the whole core: `agent -> compaction -> event -> extensions ->
presenter -> run -> session -> test_support -> ui`, plus ten Stable-Dependencies
violations. The single strongly-connected component was not one bad edge; it was
several small two-cycles knotted together, each the same shape: a type defined
in a "high" module (`ui`, `run`, `agent`) but imported by a module that the high
module in turn depends on. `event` imported `ui::{Tone, SelectorRow}` while `ui`
imported `event::Event`; `run` imported `agent::{Msg, RunMsg}` while `agent`
imported `run`; `event` reached into `run::governor::endgame::ReopenReason` while
`run` imported `event`. The domain layer was reaching outward into the layers
that should depend on it.

## Decision

Two placement rules, applied together, cut the knot without inventing a single
new abstraction:

1. **Presentment value types live in a dependency-free leaf, not in `ui`.**
   CONTEXT.md already draws the line: Presentment "decides WHAT a Transcript item
   is," rendering is "the terminal drawing the Transcript." The value types that
   cross that seam - a marker's `Tone`, a selector's `SelectorRow`/`RowRole`, the
   `TranscriptItem` (and its `StyledLine`/`LineStyle`) the core produces for the
   display - are Presentment vocabulary, not rendering machinery. They now live
   in `src/view_model.rs`, a leaf that imports nothing from the crate (In-only,
   instability 0.00). The functional core produces them (an `Event` carries a
   `Tone` or a row list; the diff extension builds a `Block` of `StyledLine`s),
   and `ui` renders them; neither the core nor `view_model` depends on the
   rendering layer. The interactive widgets and the tone-to-colour mapping stay
   in `ui` - only the value types moved.

2. **An adapter lives with its consumer, not with the port it fulfils
   (Ports and Adapters).** `run` declares the `RunDeps` port - what a Run needs
   from whatever host drives it (emit, checkpoint, complete, compact, approve).
   `AgentDeps` is the concrete adapter that fulfils it by threading each effect
   over the Agent's `mpsc` and the Session's injected `Llm`. It was defined in
   `run` (importing the Agent's `Msg`/`RunMsg` protocol) though it is
   *constructed* in the Agent. The adapter now lives in `src/agent/deps.rs`;
   `run::run` is generic over the `RunDeps` port and takes a `Capture { model,
   llm }` the host builds its tooling from, rather than reaching into a
   particular adapter's fields. `run` no longer imports `agent`.

A recovery-domain enum, `ReopenReason`, followed the same principle by a third
route: it is vocabulary the Endgame Governor *produces* but the event, log,
voice, and ui layers *consume*, and its own doc already modelled it as a sibling
of `session::RecoveryShape` (same `as_str`/`parse` pairing). It moves next to
`RecoveryShape` in `session`; the Endgame produces values of a type it imports.
`event` no longer imports `run`.

The dependency direction now points inward: `view_model` and the domain
vocabulary sit at the bottom; `run`, `ui`, and `agent` orchestrate above them
and depend downward.

## Considered options

- **The Visitor pattern** (define `Event`/`TranscriptItem` in the domain and let
  the UI supply a visitor that handles each variant): rejected as the wrong tool
  for this language and this problem. Visitor earns its ceremony with open
  hierarchies or double dispatch; Rust's exhaustive `match` already gives
  compile-checked per-variant dispatch, and the codebase already has the
  behavioural half - the Presenter pipeline is a chain of `present(item) -> item`
  strategies folding over every item. The cycles were never a "who operates on
  what" problem; they were a "where does the type live" problem, so the fix is
  relocation, not a behavioural pattern.
- **`Box<dyn Effect>` / trait-object Command for the effect interpreter and the
  run protocol**: rejected. In Rust this trades away enum exhaustiveness and
  zero-cost dispatch for dynamic dispatch and allocation, with no benefit the
  enum-plus-`match` interpreter does not already have.
- **`pub use` re-exports from the old locations** (leave `ui::transcript::Tone`
  as an alias of `view_model::Tone`): rejected. A re-export keeps the old import
  path alive, so the domain module still names the rendering layer and the cycle
  edge survives; it also leaves two public paths for one type. Every importer was
  repointed to the single canonical path instead.
- **Widening the `RunDeps` port with `model()`/`llm()` accessors** so `run::run`
  could read them off the port: rejected in favour of the `Capture` bundle. The
  port is about *effects*; the Model and Llm are *tooling inputs* the host
  supplies once. Bundling them keeps the port narrow and left the test double
  (`FakeDeps`, which holds a concrete `FakeLlm`, never an `Arc<dyn Llm>`)
  untouched.
- **Leaving the cycle** (coupling already scored ~99.5%): rejected because the
  inversion was real - the domain layer depended on the rendering layer and the
  provider depended on the consumer - and the fixes turned out cheap and
  compiler-verified, not the wide risky churn first feared.

## Consequences

- The nine-module strongly-connected component is dissolved. Stable-Dependencies
  violations dropped from ten to four. `presenter`, `extensions`, `event`, and
  `ui` all left the cycle; `run`, `ui`, and `agent` remain fan-out-heavy (high
  instability) but now sit at the top of a DAG - orchestrators depending inward -
  rather than inside a knot.
- `src/view_model.rs` is the one home for "value types the functional core hands
  the display." A new such type (a future marker kind, a new selector row shape)
  belongs there, imported by both the core that produces it and the `ui` that
  renders it - never defined under `ui`.
- Moving a type out of `ui` while its inherent `impl` stayed behind orphans the
  `impl` (rustqual flags it): the inherent `impl` moves with its type. The
  `TranscriptItem` fold predicates (`foldable_body`/`fold_title`) live in
  `view_model` with the enum.
- Two smaller cycles remain and are deferred as defensible: `run <-> test_support`
  (test-fixture coupling - `test_support` implements `RunDeps` and `run`'s
  test-gated fixtures use it) and `llm -> scout -> tool -> tools` (a separate
  strongly-connected component in the tool/scout area). Neither is a
  domain-layering inversion; both are candidates for a later pass.
- The rule generalises: when a cycle appears, look first for a type defined in a
  module that imports the module using it, and relocate it to the layer that owns
  its meaning (a leaf for shared vocabulary, the consumer for an adapter). Reach
  for a behavioural pattern only when the problem is genuinely about behaviour.
