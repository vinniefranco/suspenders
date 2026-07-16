# Event emission is a detachable Emitter handle, not a TurnDeps method

Event emission is an owned `Emitter` handle - `Emitter::new(impl FnMut(Event) + Send)`, one `emit(Event)` method - that the Turn loop obtains once from `TurnDeps::emitter()` and carries beside the deps. Every other effect stays a trait method (ADR-0011); emission alone moves out.

The reason is a borrow split. `complete` exclusively borrows the deps (`&mut D`) for the whole model call, and the streaming sink it drives must emit a `MessageUpdate` per delta **during** that call - deltas go out as they stream, between `MessageStart` and `MessageEnd`. With `emit` as a trait method, the sink would need a second `&mut` borrow of the same deps, which the compiler rightly rejects; the loop was forced to buffer every delta and emit the whole burst after `complete` returned, so the UI saw nothing while the model generated. As an owned sibling field, the handle and the deps are disjoint borrows: destructure the loop state and the sink emits live while `complete` holds the deps.

Order stays deterministic. The real `emitter()` clones the Agent's mpsc sender, so the handle and the Turn task feed the SAME channel from the SAME task (ADR-0017) - detaching emission changes nothing about event ordering. The message grammar (`MessageStart` → `MessageUpdate`* → `MessageEnd`, per Pass, on every path) stays loop-owned and fake-testable (ADR-0021): the fake's `Emitter` shares the recording `Arc<Mutex<Vec<Event>>>` with the fake itself, one ordered log either way. One boxed call per event is the accepted cost - an SSE delta dwarfs a boxed-closure dispatch.

Considered and rejected:

- **Buffering deltas and emitting after completion.** The status quo this replaces: order-preserving but not live - the user watches a spinner, then gets the whole response in one burst. Liveness is the whole point of streaming.
- **A second generic `Emit` parameter on `run` (`run<D: TurnDeps, E: Emit>`).** The same borrow split, but a second type parameter infects every call site and helper signature, and formally breaks ADR-0011's shape - one trait that **is** every effect the loop needs.
- **Shell-side emission of `MessageUpdate` inside `AgentDeps::complete`.** Splits the per-Pass message grammar across two layers - start/end in the loop, updates in the shell - and moves the updates out of the fake-tested loop, where a grammar regression would no longer be caught by the loop's own tests.
