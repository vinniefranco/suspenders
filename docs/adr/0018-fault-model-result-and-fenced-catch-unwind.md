# Fault model: Result for expected failures, catch_unwind fenced to fail-open Plugin stages

All EXPECTED failures are modelled as `Result` / typed error variants. Tools return `Result<String, String>` mapped to an is_error Tool Result, and the LLM boundary returns a Response with an Error stop_reason - never an `Err`, never a panic (ADR-0002). `std::panic::catch_unwind` is reserved for EXACTLY the three synchronous fail-open Plugin stages (ADR-0007), so a panicking Plugin is isolated to a failure→info line and the Turn survives. Panics ANYWHERE else are treated as bugs and fail the Turn honestly: the spawned Turn task's panic becomes a failed Turn Settlement (ADR-0017).

Rationale: Rust panics do not cross `.await` cleanly, so "survive a crash" is only affordable where the work is synchronous - which the Plugin stages are and the control path is not.

Considered and rejected:

- **`catch_unwind` broadly around Tool execution and Turn stages.** Async `catch_unwind` is awkward, and blanket use erases the deliberate fail-open vs fail-loud distinction.
- **Pure `Result` with no `catch_unwind` at all.** A panicking Plugin would fail the Turn, violating the fail-open guarantee (ADR-0007).
