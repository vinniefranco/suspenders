# The Agent is an actor task over channels; each Turn is a spawned task

The Agent is a dedicated long-lived tokio task that SOLELY owns the mutable Session and Conversation state. Callers - the UI, the headless driver, and tests - never touch that state directly; they send Commands (Submit, Steer, Approve, Cancel, Subscribe, and queries) over an `mpsc` channel and read Events off a `broadcast` channel. Single ownership means the state is serialized without shared locks, Events emit in one deterministic order, and the UI never talks to a Turn directly - only to the Agent.

Each Turn runs as a child task via `tokio::spawn`, and the Agent holds its `JoinHandle`. Cancellation is `JoinHandle::abort()`; partial work survives because the Agent already holds the latest per-Tool-Result checkpoint, delivered through the Deps `checkpoint` effect (ADR-0011). The `JoinError` observed after awaiting the handle distinguishes the three Turn outcomes directly: completed (`Ok`), cancelled (`Err` with the cancel flag set, so Turn Settlement records cancelled), and panicked (`Err`, so Turn Settlement records failed).

Considered and rejected:

- **`Arc<RwLock<State>>` with methods that lock and mutate.** Loses single-owner serialization, reintroduces lock-ordering reasoning, and makes the Event-ordering invariant far harder to hold.
- **A hybrid `Arc<Mutex>` snapshot for reads plus a channel for control.** Two access paths that can disagree about the current state.

Consequence: queries (Conversation, status, Plan, Session) are request-reply Commands carrying a `oneshot` reply channel. Steering stays cooperative - drained at the batch boundary - while Cancellation is abrupt (abort), matching the rule that Steering finishes in-flight work and Cancellation stops everything.
