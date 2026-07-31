# run_shell_command runs each command in its own process group and kills the group on timeout

The run_shell_command Tool spawns `sh -c <command>` with cwd set to the Project Root (or the tool's `directory` param, confined to the root), stdout and stderr merged, and a non-zero exit code mapped to is_error - in its OWN process group (`process_group(0)`). On timeout it signals the whole group (`killpg`), and `kill_on_drop(true)` is set as a Cancellation backstop, tying into abort-based Cancellation (ADR-0017).

Rationale: killing only the direct child leaves orphaned grandchildren alive after a timeout - a backgrounded subprocess, say. Group isolation makes both timeout and Cancellation actually reclaim the whole process subtree.

## The background child is Agent-owned and OUTLIVES the turn

A foreground command is bound to the Pass that spawned it: `kill_on_drop` and the turn-signal path reclaim it when the Run ends or is cancelled. A `run_shell_command` with `is_background: true` is deliberately NOT bound that way. The detached shell process is a member of the Agent-owned background-shell registry (ADR-0064), a turn-outliving process the single owner tracks past the Pass that launched it, so it must NOT die when the launching turn's handle drops:

- **No `kill_on_drop`** on the background child - the turn dropping its handle must not reclaim a process the operator launched to keep running.
- **No turn-signal forwarding** - the per-turn Cancellation that reaps a foreground command does not reach a background one.

A background child still runs in its OWN process group (`process_group(0)`), so its whole subtree stays reclaimable. Cancellation comes from exactly two places, both group-wide: `task_stop` `killpg`s the process group (the same group-kill discipline, now reached through the registry rather than a timeout), and the actor-loop-exit `abortAll` reaps every still-running background shell so none outlives the Session. Group isolation is what makes both of those actually reclaim the subtree.

Considered and rejected:

- **`child.kill()` on the direct child only.** Simpler and no libc, but leaks grandchildren.
- **A blocking spawn on a worker thread with a manual timeout thread.** Reintroduces the same kill gap and blocks a runtime thread.
- **`kill_on_drop` on the background child too.** Would tie a turn-outliving process to the turn's handle lifetime, defeating the whole point of `is_background`. The registry (ADR-0064) owns its lifetime; `task_stop`/`abortAll` are the only reapers.

Consequence: unix-only (`sh -c`), the assumed target platform. A future reader must not "simplify" this to `child.kill()` - the group kill is deliberate, and it is the ONLY kill a background child gets.
