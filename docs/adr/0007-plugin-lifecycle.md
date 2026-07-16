# Plugins wrap the Tool Call lifecycle at three points

A Plugin implements three optional callbacks over one Tool Call's lifecycle:

- `pre_run` - before execution. May adjust the input, or halt and deny the call.
- `post_run` - after execution, before Shaping. May transform the Tool Result the model sees, and may attach Artifacts.
- `present` - a **pure** function inside the Transcript fold. May substitute a richer display item.

Plugins are a static, ordered list. Ordering is onion-style: `pre_run` in registration order, `post_run` in reverse.

## Fail-open with visibility

All three stages are synchronous, so each call is wrapped in `std::panic::catch_unwind`. A panicking Plugin is skipped - its effect dropped, the token or item passing through unchanged - and recorded as a failure surfaced to the user as an info line. The model never sees it, and the Turn never fails because of a Plugin.

This is distinct from the Turn's control-bearing effects (the `TurnDeps` trait, ADR-0011), which are fail-**loud**.

## Considered and rejected

- **A general middleware/pipeline abstraction.** Assumes one synchronous pass over one token; this lifecycle spans the Turn task and the UI and three points in time.
- **Telemetry-style observers.** Cannot modify inputs or results, which is the point.
- **A runtime plugin registry.** No consumer for mid-Session toggling; a static list keeps dispatch pure and testable.

## Consequence

Plugins never add Tools; they wrap existing ones. Approval is the only hard safety gate; Plugins are cosmetic and enriching, and safe to fail.
