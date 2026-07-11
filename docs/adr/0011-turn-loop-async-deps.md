# The Turn loop is a pure async function over a Deps trait

The Turn loop is `async fn run<D: TurnDeps>(conv, session, deps: &mut D) -> Outcome`, generic over a `TurnDeps` trait whose methods **are** every effect the loop needs: `complete` (a model request), `emit` (an event), `drain_steering`, `request_approval`, `checkpoint`, `set_plan`, and an optional `after_pass` control hook returning continue, stop, or inject.

The real implementation wires these methods to the Agent's channels and the Session. Tests supply a `FakeDeps` that records calls and returns canned values. Static dispatch via generic monomorphization means no `dyn` and no async-trait crate on the 2024 edition.

The loop owns zero I/O and zero process concerns. All policy and effects arrive through the trait, so it is unit-tested with a fake and no tokio runtime scaffolding.

Boundary with Plugins: `TurnDeps` methods are infrastructure - control-bearing and fail-**loud** (a panicking Dep fails the Turn honestly). Plugins remain the fail-open, tool-scoped extension unit (ADR-0007).

Considered and rejected:

- **An explicit state-machine / enum-of-states design.** The states are fake - the loop runs forward and never branches on "which state am I in" - and encoding them pushes policy back into the loop.
- **A struct of boxed async closures.** Async closures in a struct force `Box<dyn Fn() -> Pin<Box<dyn Future>>>` and the attendant lifetime pain.
- **The loop sending effect-messages back to the Agent and awaiting replies.** Re-entangles the loop with the process model - the exact coupling this design removes.
