# Fault model: Result for expected failures, panics fail the Run

All EXPECTED failures are modelled as `Result` / typed error variants. Tools return error Tool Results the model reads and reacts to, and the LLM boundary never returns `Err` and never panics - transport and stream failures fold into a Response with an `Error` stop reason plus whatever partial content had streamed (ADR-0002). Failure is data the Run loop reads, not an exception it must catch.

Panics ANYWHERE are bugs and fail the Run honestly: the spawned Run task's panic becomes a failed Run Settlement (ADR-0017). There is no `catch_unwind` in the codebase - no code path survives its own panic.

The one fail-open seam is the Hook subsystem (ADR-0066): a Hook failure never fails the Run and never reaches the model - it is recorded visibly in the Transcript and the lifecycle proceeds as if the Hook had not fired. That is a policy on the Hook runner's ordinary `Result`s (a Hook is a subprocess, an http POST, or a prompt eval), not a panic fence.

Considered and rejected:

- **`catch_unwind` fences around "survive a crash" zones.** Async `catch_unwind` is awkward, blanket use erases the fail-loud discipline, and a swallowed panic hides a bug. The honest failed Settlement is cheaper and truer.
