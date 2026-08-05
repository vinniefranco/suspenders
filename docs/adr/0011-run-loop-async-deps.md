# The Run loop is a pure async function over a Deps trait

The Run loop is `async fn run<D: RunDeps>(conv, session, deps: &mut D) -> Outcome`, generic over a `RunDeps` trait whose methods **are** every effect the loop needs: `complete` (a model request), `drain_steering`, `request_approval`, `checkpoint`, `compact`, `set_plan`, and an optional `after_pass` control hook returning continue, stop, or inject. Event emission alone is not a trait method: the loop obtains a detachable `Emitter` handle via `RunDeps::emitter`, so the streaming sink can emit while `complete` holds `&mut D` (ADR-0025).

The real implementation wires these methods to the Agent's channels and the Session. Tests supply a `FakeDeps` that records calls and returns canned values. Static dispatch via generic monomorphization means no `dyn` and no async-trait crate on the 2024 edition.

The loop owns zero I/O and zero process concerns. All policy and effects arrive through the trait, so it is unit-tested with a fake and no tokio runtime scaffolding.

Boundary with Hooks: `RunDeps` methods are infrastructure - control-bearing and fail-**loud** (a panicking Dep fails the Run honestly, ADR-0018). Hooks are the fail-open lifecycle-interception unit (ADR-0066).

Boundary with tool-initiated effects (ADR-0055): `RunDeps` is the *loop-owned* effect channel - effects the Loop drives at control points, over `&mut D`. Effects a Tool Call initiates while it runs (approve, ask, side-query, spawn) reach the host through `Capabilities` on the `ToolCtx` instead, as `Arc<dyn>` seams (a Tool Call has neither the `&mut D` nor a control point). Both channels terminate at the same Agent mpsc; the batch gate drives Approval through `RunDeps::request_approval`, and the tool-initiated `Approver` capability shares the same request path.

Considered and rejected:

- **An explicit state-machine / enum-of-states design.** The states are fake - the loop runs forward and never branches on "which state am I in" - and encoding them pushes policy back into the loop.
- **A struct of boxed async closures.** Async closures in a struct force `Box<dyn Fn() -> Pin<Box<dyn Future>>>` and the attendant lifetime pain.
- **The loop sending effect-messages back to the Agent and awaiting replies.** Re-entangles the loop with the process model - the exact coupling this design removes.
