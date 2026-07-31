# The background subagent task registry

ADR-0061 landed foreground subagents: the `agent` tool spawns a child Run
inline, the parent parks on the tool call, and the child's result crosses back
as the tool result. qwen's `agent` tool also has a BACKGROUND mode
(`run_in_background: true`): the parent launches the child, gets an
acknowledgement back immediately, and carries on; when the child settles a
`<task-notification>` reaches the parent's next reasoning turn. qwen also ships a
`task_stop` tool that cancels a running background agent. P4b ports both (4c +
4d). This ADR records how the background task registry, its notification
envelope, and the cancellation path fit the Agent.

A SECOND, parallel background registry - detached shell processes launched by
`run_shell_command` `is_background` - reuses this ADR's notification envelope and
`notifications` queue and shares `task_stop` (dual-registry resolution), but is
kept structurally separate because its records and lifecycle differ (OS process
vs child Run). See ADR-0064.

## The Agent owns the registry (single owner, ADR-0017)

A background child Run and the record tracking it are mutable Session state: a
launch inserts a record, a settlement mutates it and queues a notification, a
`task_stop` aborts and marks it, and a Session ending aborts them all. Per
ADR-0017 that state has ONE owner - the Agent actor - so the registry is a plain
`HashMap<String, BackgroundTask>` field on `AgentState`, never a shared lock. The
`agent` tool's background launch and `task_stop` reach it ONLY through the mpsc
(`RunMsg::SpawnBackground` / `RunMsg::StopBackground`), so the map never leaves
the actor task and the single owner serializes every mutation with all other
Session state.

`BackgroundTask` holds the child's `AbortHandle` (for `task_stop` and the
Session-exit abortAll), its `BackgroundStatus` lifecycle (`Running`, `Done`,
`Stopped`, `Failed` - all UNIT variants today), and its `description` (carried
for the notification envelope). The terminal `Done`/`Failed` variants carry NO
payload: `background_done` assembles the notification from the settling result
inline and never reads it back off the status, and the id already embeds the
subagent type (`{subagent_type}-{n}`), so a stored `subagent_type` field would go
unread - both are dropped rather than kept as dead scaffolding (a future
`send_message`/resume re-adds the terminal payload, see Deferred). The task id is
`{subagent_type}-{n}` (`mint_task_id`, qwen's `<subagentName>-<suffix>` shape),
where `n` is the Agent's monotonic per-Session counter, so ids never collide.

## The detached child + BackgroundDone

`SpawnBackground` resolves the request into a `ChildRunRequest` through the SAME
`DirectSubagentSpawner::build_child_request` helper the foreground `spawn` uses
(so foreground and background can never drift on how a def/Model/tool-subset
resolves), then `tokio::spawn`s a DETACHED task that drives `run_child` to
settlement and posts a `RunMsg::BackgroundDone { id, result }` back over the
Agent's own mpsc. The Agent holds only the `AbortHandle`; the detached task's
`JoinHandle` is dropped (fire-and-forget), because the result comes back as a
message, not by awaiting the handle. `sink: None` - the live-output feed is
DEFERRED (below), so a background child is as invisible mid-run as a foreground
one; only its settlement notification crosses back.

When `BackgroundDone` arrives, `background_done` records the terminal status
(`GOAL` -> `Done`/`completed`, else `Failed`/`failed`), queues the
`<task-notification>`, logs it as a durable user-role entry, and broadcasts a
`BackgroundNotification` + `BackgroundTaskFinished` Event. If the entry is
already `Stopped` (a `task_stop` cancelled it first), the racing result is
DROPPED - no double-notify.

## The `<task-notification>` envelope, VERBATIM but trimmed

The envelope is qwen's `background-tasks.ts` `emitNotification`, VERBATIM in shape
and TRIMMED to the fields Suspenders carries today:

```
<task-notification>
<task-id>{id}</task-id>
<status>{status}</status>
<summary>Agent "{description}" {statusText}.</summary>
<result>{result}</result>   <!-- CONDITIONAL: omitted entirely when result is empty -->
</task-notification>
```

`statusText` is qwen's `completed | failed | was cancelled`; `status` is the raw
lifecycle word (`completed`/`failed`/`cancelled`); a failed `<result>` is
`Error: {error}`. The `<result>` line is CONDITIONAL, matching qwen's
`if (entry.result)` guard (`background-tasks.ts`): an empty result omits the line
ENTIRELY, so a cancelled notification (which carries no result) emits NO
`<result>` tag rather than a bare `<result></result>`. A completion carries the
child's answer text and a failure always carries a non-empty `Error: {error}`
line, so both keep the tag. Every interpolated value is `escape_xml`-escaped (the
SAME escaper the skills catalog uses, ADR-0058), so a result carrying `<`/`&`/`"`
cannot close the envelope early and forge sibling tags the model would trust.
qwen's deferred `<tool-use-id>`, `<output-file>`, and `<usage>` lines are TRIMMED
(no per-task tool-use id, no live-output file, no per-task usage roll-up yet).

## The parent-settled race: queue survives idle drains + immediate UI Event

A background child can settle while the parent is idle (no Run in flight) or
mid-Run. The queue handles both without a special case: `background_done` pushes
onto `AgentState.notifications`, and the Loop drains that queue
(`drain_notifications`) at TWO points, so a queued notification reaches the model
on its very NEXT Run no matter the shape of that Run:

- **At Run start** (`drain_notifications_at_run_start`), once, before the first
  request: any pending notification merges into the FIRST request's user turn
  (the Run's prompt), the same trailing-text shape the tool-results message uses.
  This is the delivery point for a notification that landed between Runs - it
  reaches the model even when the next Run's ONLY Pass is pure text (no tool
  call), which the between-Passes drain below would never see.
- **Between Passes** (the tool-answering tail of every Pass, right after
  `drain_steering`): a notification that settles mid-Run merges into that Pass's
  tool-results user message, so a child settling while the Run is still going is
  read on the very next request without waiting for the Run to end.

The queue is drained-and-cleared once at each point (the Agent empties it on the
drain), so a notification is delivered exactly once - a Run-start drain that
carries it means no later Pass re-delivers it. A notification that lands between
Runs simply survives on the queue and enters the model's Conversation at the next
Run's start. So the model always reads a completion, whenever it settled. The UI
does NOT wait for that: `background_done` also broadcasts a
`BackgroundNotification` Event immediately, so the operator sees the completion
now even though the model reads it on its next turn.

`drain_notifications` is a PARALLEL channel to steering, deliberately NOT steering
itself: steering is the user's voice continuing a request; a background
notification is machine-generated task news. They share the merge mechanism (both
ride the tool-results user message as trailing text) but are separate queues and
separate Events.

## Cancellation (task_stop) + abort-safety

`task_stop` reaches the Agent through `stop_background`. The Agent aborts the
child's detached task (`AbortHandle::abort` - cancels at the next `.await`), sets
the entry `Stopped` (so the racing `BackgroundDone`, if the abort loses the race,
is dropped), queues the `was cancelled` notification SYNCHRONOUSLY (the abort
means no `BackgroundDone` will carry the child's partial result, so the terminal
notification is queued now), and returns the VERBATIM qwen wording. Three legs,
all VERBATIM: found+running (the stop confirmation, `Cancellation requested for
background agent "{id}". A final task-notification carrying the agent's last
result will follow.\nDescription: {desc}`), found+not-running (`Error: Background
agent "{id}" is not running (status: {status}).`), and not-found (`Error: No
background task found with ID "{id}".`).

Abort-safety: an abort only cancels at an `.await`, so a child mid-tool-execution
finishes that step and unwinds cleanly - no partial file left half-written by a
torn synchronous call. At actor-loop exit (`run_agent` returns, every handle
dropped), `abort_all_background` aborts every still-running child, so no detached
Run outlives the Session.

## Recursion guards hold

A background subagent cannot recurse: a child Run's own subagents capability is
the `UnavailableSubagentSpawner`, whose `spawn_background` errs and
`stop_background` not-founds - the same guard the foreground `spawn` relies on.
`agent`, `task_stop`, and `ask_user_question` stay in
`EXCLUDED_TOOLS_FOR_SUBAGENTS`, so a child never even sees `task_stop` on its wire
list. The acyclic dependency holds: `task_stop` reaches only the `caps`
`SubagentSpawner` trait (never the Agent); the registry types live under
`agent/background.rs`, owned by the Agent.

## Considered and rejected

- **A shared `Arc<Mutex<HashMap>>` registry.** Sharing the registry with the
  tools or a watcher task would reintroduce the locking ADR-0017 exists to avoid.
  The registry is Agent-owned state; the tools reach it through the mpsc.
- **Awaiting the detached `JoinHandle` to get the result.** The Agent would then
  co-own the handle it also aborts through, the exact bind ADR-0017's watcher
  pattern avoids. The result comes back as a `BackgroundDone` message instead, so
  the Agent holds only the `AbortHandle`.
- **A live-output feed (the `sink`).** `run_child` already takes an optional
  `ChildSink`, so a background child's events COULD stream to the parent. DEFERRED
  - the notification-on-settlement path is the whole 4c behavior qwen's small
  models rely on; the live feed, per-task usage stats, and the `output_file`
  jsonl are additive polish.

## Deferred (without foreclosure)

- **The live-output feed / `output_file`.** `sink: None` today; `run_child`'s
  `Some(sink)` seam is already wired for it.
- **Resume / persistence of the registry.** A background task does not survive a
  Session restart; the notification's user-role log entry is the only durable
  trace. Faithful to qwen shipping the registry in-memory.
- **`send_message` (continuing a background agent).** Not ported; the launch
  string's send_message clause is trimmed. `task_stop` (cancel) is the only
  control-plane verb ported.
- **Terminal-result payload retrieval.** `BackgroundStatus::Done`/`Failed` are
  UNIT variants today (the notification is assembled from the settling result
  inline). A future `send_message`/resume feature that needs to read a settled
  child's result back off the registry re-adds the payload to the terminal status
  then; today nothing reads it, so it is not stored.
- **Prune / grace-timer.** Settled entries linger in the map until Session end;
  qwen's prune and paused-agent grace timer are not ported.
