# The LLM boundary is a trait injected per Session, with a per-instance fake

The `Llm` trait - the `complete` seam - is injected per instance: the Agent is constructed with an `Arc<dyn Llm>` beside the Session value. The production `Dispatcher` (holding the Session's resolved Providers and routing each request by its Model's Api, ADR-0037) and the test `FakeLlm` are peers behind that trait. The `FakeLlm` owns its OWN script queue (an `Arc<Mutex<VecDeque<Entry>>>`) supplied at construction, NOT a global or process-wide registry. A script entry can be a canned Response, an error, or a closure that inspects the request and may block on a barrier - enough to drive busy/cancel handshakes.

Rationale: per-instance injection means tests carry no shared mutable global, so the suite runs in PARALLEL. It also preserves the boundary's discipline that it reads no config: the request and the injected boundary carry everything an adapter needs (ADR-0002, ADR-0037).

Considered and rejected:

- **A global or thread-local script registry with serialized tests.** A wart the language lets us avoid; it makes the suite slower and order-sensitive.
