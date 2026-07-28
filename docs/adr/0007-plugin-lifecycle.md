# Extensions wrap the Tool Call lifecycle via Middleware and Presenter roles

An Extension fills two composable roles over one Tool Call's lifecycle. A
Middleware implements the execution-path callbacks; a Presenter implements the
display-path callback. A registered Extension may fill either role or both.

- `pre_run` (Middleware) - before execution. May adjust the input, or halt and deny the call.
- `post_run` (Middleware) - after execution, before Shaping. May transform the Tool Result the model sees, and may attach Artifacts.
- `present` (Presenter) - a **pure** function inside the Transcript fold. May substitute a richer display item.

Extensions are a static, ordered list. Ordering is onion-style: `pre_run` in registration order, `post_run` in reverse.

## Fail-open with visibility

All three stages are synchronous, so each call is wrapped in `std::panic::catch_unwind`. A panicking Extension is skipped - its effect dropped, the token or item passing through unchanged - and recorded as a failure surfaced to the user as an info line. The model never sees it, and the Run never fails because of a Middleware or Presenter.

This is distinct from the Run's control-bearing effects (the `RunDeps` trait, ADR-0011), which are fail-**loud**.

## Considered and rejected

- **A general middleware/pipeline abstraction.** Assumes one synchronous pass over one token; this lifecycle spans the Run task and the UI and three points in time.
- **Telemetry-style observers.** Cannot modify inputs or results, which is the point.
- **A runtime extension registry.** No consumer for mid-Session toggling; a static list keeps dispatch pure and testable.

## Consequence

Extensions never add Tools; they wrap existing ones. Approval is the only hard safety gate; the Middleware and Presenter roles are cosmetic and enriching, and safe to fail.
