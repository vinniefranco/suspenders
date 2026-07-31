# The background shell registry (parallel to subagents)

ADR-0023 spawns a `run_shell_command` in its own process group and reaps it when
the launching turn ends. qwen's `run_shell_command` also has a BACKGROUND mode
(`is_background: true`): the shell launches, the tool returns a started
acknowledgement immediately, and the process keeps running past the turn while
its output streams to a capture file. This ADR records how a second, DETACHED
shell registry fits the Agent, and why it is deliberately PARALLEL to - not
unified with - the background-subagent registry (ADR-0063).

## The Agent owns the registry (single owner, ADR-0017)

A background shell process and the record tracking it are turn-outliving Session
state: a launch inserts a record, `task_stop` kills it, and actor-loop exit reaps
them all. Per ADR-0017 that state has ONE owner - the Agent actor - so the
registry is a plain field on `AgentState`, never a shared lock. The
`run_shell_command` background launch reaches it ONLY through a new tx-backed
capability (below), so the map never leaves the actor task and the single owner
serializes every mutation with all other Session state.

The record holds the detached child's handle (its process group, for the group
`killpg`), its lifecycle status, its `description` (for the notification
envelope), and the path to its capture file. The task id is `bg_<n>`, where `n`
is a monotonic per-Session counter, so ids never collide.

## Parallel to subagents, NOT unified

Suspenders now has TWO background registries the Agent owns: the subagent
registry (ADR-0063, tracking detached child Runs) and this shell registry
(tracking detached OS processes). They are deliberately kept separate rather than
folded into one:

- A background subagent is a child Run driven over the captured Llm; its
  "cancellation" is an `AbortHandle::abort` at the next `.await`, and its
  settlement is a `BackgroundDone` MESSAGE carrying a result the model reads.
- A background shell is an OS process in a process group; its cancellation is a
  `killpg` of the group (ADR-0023's discipline), and it has no in-band "result"
  to carry back - only streamed output on disk.

The two lifecycles have nothing structural in common, so one `HashMap` keyed by a
shared id space would only force a sum type over two dissimilar records and a
branch on every operation. Keeping them parallel keeps each registry's ops
straight-line. What they DO share is the delivery plumbing: both reuse the
`task_notification` envelope and the `notifications` queue (ADR-0063), so a shell
that finishes reaches the model's next Run through the exact same drain-at-two-
points path a subagent settlement uses.

## The tx-backed BackgroundShellSpawner capability (ADR-0055)

`run_shell_command` initiates the background launch mid-run, deep inside
`dyn Tool::run` where no `&mut RunDeps` reaches. So it follows the Capability
Context pattern (ADR-0055): a new `dyn BackgroundShellSpawner` seam on the
`ToolCtx`'s `Capabilities`, whose real impl is tx-backed over the Agent mpsc - a
`RunMsg` the Agent folds into the registry, exactly as `AgentApprover` /
`AgentQuestioner` relay their effects. It lands like the Approver, NOT like
SideQuery: spawning a turn-outliving process the Agent must own and later reap is
an Agent-OWNED effect, not a bare Llm call, so it MUST cross the mpsc to reach the
single owner. Its degraded impl is the headless posture (ADR-0019): a host with
no Agent channel returns an error the tool folds into its own error result rather
than leaking an unowned process.

## The concurrent drain + capture file

A detached shell's stdout and stderr are drained CONCURRENTLY by a `select!` that
reads whichever stream is ready, strips ANSI escapes from each chunk
(`crate::text::strip_ansi`), and appends to a capture file under the Session
directory. The drain runs in the detached task alongside the process, so a chatty
background command never blocks the actor and its output is on disk for the model
or operator to read. When the process exits, the task queues the settlement
`task_notification` and marks the record terminal - the same envelope-and-queue
path as a subagent settlement (ADR-0063).

## Cancellation across BOTH registries (dual-registry task_stop)

`task_stop` (ADR-0063) now resolves across BOTH registries. The Agent tries the
subagent registry first, then the shell registry, via `Option` chaining: an id
that matches a background subagent aborts the child Run; an id that matches a
background shell `killpg`s its process group (ADR-0023's group kill, now reached
through the registry rather than a timeout). A not-found in both returns the
verbatim qwen not-found wording. At actor-loop exit (`run_agent` returns), the
`abortAll` reaps BOTH registries, so no detached Run and no detached shell process
outlives the Session.

## Fidelity fallbacks (deliberate, recorded)

The port matches qwen's shape and wording where Suspenders has the machinery, and
falls back explicitly where it does not:

- **No pid in the started message.** qwen surfaces the OS pid in its launch
  acknowledgement; Suspenders' started message omits it (the `bg_<n>` id is the
  handle the operator and `task_stop` use).
- **`bg_<n>` ids, not qwen's hex.** A monotonic per-Session counter, not qwen's
  hex suffix - stable, collision-free, and legible.
- **git-commit-in-background refused, VERBATIM wording.** qwen refuses launching a
  `git commit` in the background and points at a notes path; Suspenders reuses the
  verbatim refusal wording even though it lacks that notes path (the refusal is
  the load-bearing part; the pointer is not).
- **sleep-interception ported minus the Monitor sentence.** qwen intercepts a bare
  `sleep` and steers the model toward a poll loop; Suspenders ports that
  interception minus qwen's Monitor-tool sentence (Suspenders has no Monitor tool).

## Considered and rejected

- **One unified registry over subagents + shells.** A sum type over two dissimilar
  records (child Run vs OS process) with a branch on every op, for no gain - their
  lifecycles, cancellation, and settlement shapes are genuinely different. The
  registries stay parallel; only the notification plumbing is shared. See "Parallel
  to subagents, NOT unified".
- **A shared `Arc<Mutex<HashMap>>` shell registry.** Sharing it with the tool or a
  watcher task reintroduces the locking ADR-0017 exists to avoid. The registry is
  Agent-owned state; the tool reaches it through the tx-backed capability.
- **`kill_on_drop` on the detached shell.** Would tie a turn-outliving process to
  the launching turn's handle lifetime, defeating `is_background`. The registry
  owns its lifetime; `task_stop`/`abortAll` are the only reapers (ADR-0023).
- **Routing the spawner DIRECT like SideQuery/SubagentSpawner.** A background shell
  is an Agent-owned process the single owner must track and reap, so it MUST cross
  the mpsc - unlike a side-query or a foreground subagent, which touch no
  Agent-owned state.

## Deferred (without foreclosure)

- **A live-output feed to the UI.** Output lands in the capture file today; a live
  stream to the operator is additive polish, like the subagent `sink` (ADR-0063).
- **Persistence across a Session restart.** A background shell does not survive a
  Session restart; the capture file and the notification's log entry are the only
  durable traces. Faithful to qwen shipping the registry in-memory.
- **Prune of settled entries.** Settled records linger in the map until Session
  end, matching the subagent registry (ADR-0063).
