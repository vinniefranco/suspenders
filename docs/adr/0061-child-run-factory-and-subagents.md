# The child-Run factory and foreground subagents

qwen's `agent` tool delegates a task to a specialized subagent: it launches an
`AgentHeadless` - a self-contained reasoning loop with its own tool access and
system prompt - runs it to settlement, and returns the subagent's final text to
the parent as the tool result. The subagent runs headless (no user interaction),
its whole reasoning loop is invisible to the parent's transcript, and only its
result crosses back. P4 (4a + 4b) ports this for the FOREGROUND case: the parent
calls `agent`, the child Run runs inline, and the parent parks on the tool call
until the child settles. The background case (4c) and `task_stop` (4d) are
DEFERRED (see below).

## RunDeps is the isolation seam

A Run already declares everything it needs from its host as `RunDeps` (ADR-0011):
`complete`, `emitter`, `checkpoint`, `set_plan`, `drain_steering`,
`request_approval`, `after_pass`, `compact`. The parent's adapter,
`AgentDeps`, threads every one of those over the Agent's mpsc. A child Run needs
the SAME port but a very different adapter: `ChildDeps` (`crate::run::child`)
threads only `complete` to a real boundary (the captured `Arc<dyn Llm>` with the
child's Model) and makes every OTHER effect a no-op or the safe answer:

- `emitter` emits into an optional child `sink` (the background live-output feed,
  DEFERRED) or a NO-OP `Emitter` otherwise - NEVER the parent Agent mpsc. This is
  what keeps the child's whole reasoning loop out of the parent's Session Log and
  Transcript.
- `checkpoint` and `set_plan` are no-ops: the child's partial state is not
  persisted to the parent's Log, and a child holds no Plan outside its own
  Conversation.
- `drain_steering` yields `vec![]` (a foreground child cannot be steered
  mid-run).
- `request_approval` returns `false`: a foreground subagent has no modal to
  answer a gated command through (its Approver is a `DenyingApprover` and its
  tool subset excludes `ask_user_question`), so it denies - the safe answer.
- `after_pass` / `compact` take the `RunDeps` defaults (Continue; no compactor).

Because `RunDeps` is the only way the loop reaches its host, swapping the adapter
is the entire isolation mechanism: no code in the loop, the batch, or the tools
knows or cares that it is running under a child adapter. This is why the same
`loop_::run` drives both a top-level Run and a subagent Run unchanged.

## `run_child` is a self-contained re-entrant driver

`run_child` (`crate::run`) needs NO Agent actor. It assembles a child Run from a
`ChildRunRequest` the way `run::run` assembles a top-level Run from a `Capture`:
a fresh child `Conversation` (the def's verbatim system prompt, seeded with the
task as the first user message), a child `ToolRegistry` over the def's tool
subset, a fresh `FileReadCache`, child `Capabilities`, a child `ToolCtx`, a
`ChildDeps`, then `loop_::run` with the child turn bound (`max_turns`). The child
derives its Root, command timeout, budget knobs, and Provider set from the
parent `Session` cloned onto the request (a subagent is the parent's Run over a
fresh Conversation and a narrowed tool set), overriding only `run_limit =
max_turns` and the captured Model. Being a plain async fn over the request, it is
identical for a foreground OR a background caller - the ONLY difference is the
`sink` (None foreground, Some background) - so 4c needs no new driver.

The child `Capabilities` carry a `DenyingApprover`, a `DecliningQuestioner`, an
`LlmSideQuery` over the child's own Llm/Model (so `web_fetch` still works inside a
subagent), and an `UnavailableSubagentSpawner`. That last one is the RECURSION
GUARD (below).

## Per-subagent Model is a Model value over the shared Dispatcher

The F4 seam (Opus-main / Qwen-scout) is that a subagent may run a DIFFERENT Model
than the Run that spawned it. This is not a second Llm: the SAME shared
`Arc<dyn Llm>` (the Dispatcher) routes ANY Model to its Provider, so the seam is a
Model VALUE flowing through the child's `ChildDeps.model`, exactly like
`LlmSideQuery`'s pinned-Model path (ADR-0055). `DirectSubagentSpawner`
(`crate::run::subagent`) resolves the child Model in three layers: an explicit
`model` override on the `SubagentRequest` wins; else the def's `SubagentModel`
(Inherit -> the parent Model; Scoped(id) -> resolved against the Session's
Provider set, an unresolvable Provider surfacing as an Err). qwen's `'fast'` model
alias is DEFERRED - the `Explore` agent, which qwen marks `'fast'`, ships as
`Inherit` for now.

## Subagent definitions (a leaf) and the built-in set

`crate::subagents` is a LEAF (it names only `tool`/`content`/`llm`/`std`): a
`SubagentDef` (name, description, verbatim system prompt, `SubagentModel`,
`ToolSelector`), a `SubagentRegistry` the `agent` tool holds like the `skill`
tool holds a `SkillManager`, and `builtins()`. Suspenders ships exactly TWO of
qwen's three built-ins (`subagents/builtin-agents.ts`): `general-purpose` (the
default, Inherit, all tools) and the read-only `Explore` (Inherit for now, whose
allowlist is qwen's Explore tool set - READ_FILE, GREP, GLOB, SHELL, LS,
WEB_FETCH, TODO_WRITE, MEMORY, SKILL, LSP, ASK_USER_QUESTION - intersected with
the tools that exist here and then minus the exclusions, so
read_file/grep/glob/run_command/list_files/web_fetch/todo_write; this grant backs
its verbatim prompt, which tells the model to use read-only run_command and to
fetch the web). qwen's third built-in, `statusline-setup`, edits
`~/.qwen/settings.json` and has no Suspenders analog, so it is not ported. Both
system prompts and descriptions are copied VERBATIM from qwen (em-dashes rendered
as hyphens per house style; qwen's `${ToolNames.*}` interpolations resolved to
Suspenders' own tool names at the interpolation points, as qwen does at build
time).

The `agent` tool's empty-catalog wording is qwen's first sentence only - `No
subagents are currently configured.` qwen's second sentence (`You can create
subagents using the /agents command.`) is INTENTIONALLY dropped: there is no
`/agents` command ported yet (user/project subagent files are DEFERRED), so it
would point the model at a command that does not exist. The branch is unreachable
in production anyway - `builtins()` always ships two defs - and survives only for
the empty-registry spec test.

## Terminate-reason mapping

A child Run's `Outcome` maps to qwen's `AgentTerminateMode` vocabulary
(`agents/runtime/agent-types.ts`): `EndTurn`/`MaxTokens`/`ToolUse`/`StopSequence`
-> `GOAL`; `RunLimit` -> `MAX_TURNS`; a stuck loop, a custom after-Pass stop, a
failed Run, or an exhausted budget -> `ERROR`. `TIMEOUT` and `CANCELLED` are
DEFERRED with the background path (a foreground subagent settles synchronously
through the tool result, so it has no wall-clock timeout and no abort-signal
cancel of its own). The `SubagentResult.result` is the child's LAST assistant
text, joined and trimmed, with any trailing pure-Voice close marker
(`[turn limit reached ...]`, `[turn failed]`, ...) dropped - the Loop appends
that marker as a marker-only assistant message when it closes a Run itself, and
it is Suspenders' internal signal, not the subagent's answer. The `agent` tool
shapes the settled result qwen's FOREGROUND way (`tools/agent/agent.ts`
~2276-2306, the path this ports - NOT the background `registry.fail` string): an
`ERROR` terminate with an empty result becomes the verbatim
`Subagent execution failed.`; every other case (a non-empty result on any mode,
or a non-`ERROR` mode with an empty result) surfaces the bare result, which may
itself be empty. qwen's background `Agent terminated with mode: {reason}` string
belongs to the background `registry.fail` path (DEFERRED) and is not used here;
qwen's `CANCELLED` prefix is DEFERRED with the background path too.

## EXCLUDED_TOOLS and the three recursion guards

`EXCLUDED_TOOLS_FOR_SUBAGENTS` (`crate::subagents`) ports qwen's exclusion set
(`agents/runtime/agent-core.ts`), intersected with the tools that exist here:
`agent`, `task_stop`, `ask_user_question`. `subagent_tools` builds the child set
from the built-ins (MCP tools for subagents DEFERRED), applies the def's
allowlist, then drops every excluded tool regardless of the selector - so a def
can never re-admit an excluded tool. Recursion is blocked THREE ways, defence in
depth:

1. `EXCLUDED_TOOLS_FOR_SUBAGENTS` drops `agent` from the child tool set, so the
   child's model never even sees the delegation tool.
2. The child's `subagents` capability is an `UnavailableSubagentSpawner`, so even
   if an `agent` call reached the capability it would get the degraded Err rather
   than spawn a grandchild.
3. `ChildRunRequest.depth` rides at `depth = 1` for a foreground subagent - a
   belt-and-braces record a future nesting policy can read.

## Deferral of 4c (background) and 4d (task_stop), without foreclosure

Foreground only, on purpose. The deferral does not foreclose the background path:

- **4c background.** `run_child` is already identical for a background caller -
  it takes the same `ChildRunRequest`, differing only by a `Some(sink)` for the
  live-output feed. `SubagentSpawner` gains a `spawn_detached` method ADDITIVELY
  (the existing `spawn` is untouched), and the Agent grows a task registry that
  owns the detached `JoinHandle`s and posts completion notifications back over
  its mpsc - none of which touches the foreground path built here.
- **4d task_stop.** `task_stop` is already a deferred tool name in
  `EXCLUDED_TOOLS_FOR_SUBAGENTS` (so a subagent can never stop tasks) and would
  land as an Agent-owned tool over that same background task registry. It has no
  foreground analog (a foreground subagent settles synchronously), so its absence
  here is complete, not partial.

## Considered and rejected

- **Giving the child its own Agent actor.** A foreground subagent touches no
  Agent-owned state (no Steering, no Standing Approvals, no Session Log, no
  broadcast), so an actor + mpsc would be ceremony around a plain child Run. The
  `ChildDeps` no-op adapter is the whole isolation, and `run_child` drives the
  loop inline - the effect terminates at the Llm boundary, like the SideQuery.
- **A per-subagent Llm.** The shared Dispatcher already routes any Model to its
  Provider; a second Llm would duplicate the whole boundary to change one Model
  value. The seam is a `Model` on `ChildDeps`, nothing more.
- **Routing the spawn through the Agent mpsc like the Approver.** A subagent
  mutates no Agent/Conversation state, so a `RunMsg` variant and a reply oneshot
  would be round-trip ceremony. `DirectSubagentSpawner` drives the child Run off
  the captured Llm directly (ADR-0055's DIRECT kind).
- **Porting all three qwen built-ins.** `statusline-setup` is Qwen-Code-specific
  (it edits `~/.qwen/settings.json`); porting it would ship a subagent with no
  Suspenders behavior behind it.
