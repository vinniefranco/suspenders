# run_command runs each command in its own process group and kills the group on timeout

The run_command Tool spawns `sh -c <command>` with cwd set to the Project Root, stdout and stderr merged, and a non-zero exit code mapped to is_error - in its OWN process group (`process_group(0)`). On timeout it signals the whole group (`killpg`), and `kill_on_drop(true)` is set as a Cancellation backstop, tying into abort-based Cancellation (ADR-0017).

Rationale: killing only the direct child leaves orphaned grandchildren alive after a timeout - a backgrounded subprocess, say. Group isolation makes both timeout and Cancellation actually reclaim the whole process subtree.

Considered and rejected:

- **`child.kill()` on the direct child only.** Simpler and no libc, but leaks grandchildren.
- **A blocking spawn on a worker thread with a manual timeout thread.** Reintroduces the same kill gap and blocks a runtime thread.

Consequence: unix-only (`sh -c`), the assumed target platform. A future reader must not "simplify" this to `child.kill()` - the group kill is deliberate.
