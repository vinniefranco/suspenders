# The hook subsystem: user- and skill-configured actions fired at lifecycle events

Suspenders is a coding agent for small local models, and much of its value comes from being scriptable: a user (or a Skill the model invokes) wants to run a linter after every edit, deny a dangerous command before it executes, POST every tool call to an audit endpoint, or ask a second model to vet an approval. This ADR is the hook subsystem, a faithful port of qwen v0.16.0's hooks (`hooks/types.ts`, `hooks/hook-executor.ts`, `hooks/hook-registry.ts`). A *hook* is a user- or Skill-configured action fired at a named lifecycle event; its JSON output can observe, halt, decide, or enrich the surrounding Run. Hooks are the single generic lifecycle-interception layer in Suspenders.

## A hook is configured, not coded

A hook is a configured action, never a compiled callback. It is declared in one of two places and fired when its event fires.

The first source is `config.json`, under a `hooks` key, at both project scope (`<project-root>/.suspenders/config.json`) and user scope (`~/.suspenders/config.json`), matching every other Suspenders convention (we use `.suspenders/`, qwen's `.qwen/`). These are the standing hooks a user always wants: a post-edit formatter, a pre-command guard, an audit sink.

The second source is a Skill's frontmatter, under a `hooks:` key (ADR-0058 parses-and-ignores this today; this ADR is what makes it live). A skill's hooks are SESSION-scoped: they are registered when the model actually invokes that skill, not at discovery time, and they carry the skill's directory in `SUSPENDERS_SKILL_ROOT` (qwen's `QWEN_SKILL_ROOT`) so a hook `command` can resolve scripts that ship beside the `SKILL.md`. A skill that is never invoked contributes no hooks. This keeps the standing config small and lets a specialized capability bring its own lifecycle behavior only while it is in play, the same on-demand ethos as the skill catalog itself.

## Three hook types, the function type rejected

A hook has a `type` that says how it runs.

- **command** shells out. The event payload is delivered to the process (on stdin, as JSON), and the process's stdout is parsed back as the hook's JSON decision. This is the workhorse: run a formatter, a linter, a guard script.
- **http** POSTs the event payload as JSON to a `url` and reads the response body as the hook's JSON decision. This is the audit/remote-policy path: a central service can observe or veto.
- **prompt** runs the LLM to evaluate. The event payload is spliced into a prompt template and sent as a single-turn completion; the model's reply is read back as the hook's decision. This is the "ask a second opinion" path, and it runs on the Active Model (ADR-0033) unless a per-hook override is given.

qwen has a fourth type, **function**: an in-process SDK callback (`FunctionHookCallback`, a JS closure the embedding host registers). We REJECT it. It is a JavaScript-embedding hook with no meaning in a compiled Rust CLI: there is no host program registering closures against our event loop, and porting it would mean inventing a plugin-callback ABI that nobody in a shipped binary can populate. The three configured types (command, http, prompt) cover every use a `config.json`- or frontmatter-configured hook can express, so the function type drops with no loss of reachable capability.

## Discrete fire-points, not a chain

The central design decision is that a hook fires inline at a specific lifecycle site, and its decision is processed inline at that site. There is no composed interceptor chain, no ordered list of behaviors that each pass wraps, no generic pass over a token. The tool scheduler, the session loop, and the compaction service each simply `await` the relevant hook at its own call site and act on the result then and there, which is faithful to qwen and keeps every fire-point explicit.

This is possible because hooks carry only the generic lifecycle-interception concern. Tool-specific behavior lives in the Tool that owns it: the diff, todo, and `run_command` output behaviors belong to their respective Tools, and each renders its own rich result. The condense/summarize behavior belongs to the compaction service (ADR-0012), which owns the context-shrinking decision. That relocation is recorded in ADR-0007 and ADR-0042. With those concerns housed where they belong, hooks are left as the ONLY generic lifecycle layer, and each hook site stays explicit and easy to reason about.

## The full decision protocol

A hook is not observability-only. Its whole value is that its JSON output can steer the Run, and we port qwen's decision protocol faithfully. The output object carries these steering fields:

- **Halt the loop.** `continue: false` (with an optional `stopReason` string) stops the surrounding loop; the `stopReason` becomes a Suspenders `StopReason` so the Run ends with the hook's explanation rather than the model's.
- **Control a Tool Call.** A `decision` field of `block`, `deny`, `approve`, `allow`, or `ask` governs whether a tool call proceeds. `block`/`deny` stop the call and feed the `reason` back to the model as the failure; `approve`/`allow` let it through; `ask` routes it to the user.
- **Resolve an Approval.** For the approval seam specifically, `hookSpecificOutput.permissionDecision` of `allow`, `deny`, or `ask` feeds directly into the ADR-0050 approval flow, so a hook can auto-approve or auto-deny a call that would otherwise prompt (a PermissionRequest hook can additionally return a structured decision object with `updatedInput`/`message`/`interrupt`, which we honor for parity).
- **Inject context.** `hookSpecificOutput.additionalContext` (a string) is injected as a conversation turn, so a hook can hand the model new information (lint output, a policy note) that shapes what it does next. Per qwen, `<`/`>` in the injected string are escaped so a hook cannot smuggle tags into the transcript.
- **Hide output.** `suppressOutput: true` keeps the hook's stdout/stderr out of the visible transcript for a chatty hook whose noise is not worth surfacing.

The precedence and defaults follow qwen's specialized output classes: a PostToolUse hook that returns no explicit `decision` defaults to `allow` (allow-by-default unless explicitly blocked), a Stop hook's `stopReason` is surfaced as "Stop hook feedback", and a PreToolUse hook maps a bare `decision` onto a `permissionDecision` (`approve`/`allow` become allow, `deny`/`block` become deny). We port those mappings rather than reinvent them.

## Sixteen events, each with a real Suspenders anchor

The reason we wire the WHOLE qwen event set, not a convenient subset, is that every qwen event names a lifecycle moment Suspenders actually has. Each event fires at the anchor below.

- **PreToolUse / PostToolUse / PostToolUseFailure** fire at the tool-dispatch seam: before a tool runs, after it succeeds, and after it fails. This is where a formatter, a guard, or an audit sink lives.
- **PermissionRequest** fires when an approval would be shown, feeding the ADR-0050 approval flow so a hook can decide it without a human.
- **Notification** fires when a terminal notification is sent (the same notify path an "agent is waiting" alert takes).
- **Stop / StopFailure** fire as a Run concludes: Stop on a clean end, StopFailure when an API error ended the turn. Both can produce a `StopReason`.
- **UserPromptSubmit** fires when the user submits from the composer, before the prompt reaches the model, so a hook can inject context or veto.
- **PreCompact / PostCompact** fire around compaction (ADR-0012): before, a hook can adjust the custom instructions; after, it observes the summary (PostCompact is observe-only, matching qwen, where its returned JSON produces no control effect).
- **TodoCreated / TodoCompleted** fire from the todo tool (ADR-0048) when an item is added or marked complete, each with qwen's validation/postWrite phase split so validation stays side-effect-free.
- **SessionStart / SessionEnd** fire at the session boundary (startup/resume/clear vs. clear/logout/exit).
- **SubagentStart / SubagentStop** fire around a subagent Run (ADR-0061), the child-Run analog of Stop.

Every one of these sixteen has a genuine anchor in the Suspenders loop, which is precisely why the whole set is wired rather than a subset: there is no qwen event that maps to nothing here.

## Trust and sandbox: trusted by configuration, visible on every fire

We adopt qwen's full trust model. There is NO per-fire approval gate on a hook. A hook is trusted by virtue of being in your `config.json` or in a skill you installed and the model invoked; asking the user to approve every fire would defeat the entire point of automation (a post-edit formatter that prompts on every edit is worse than no formatter). Trust is granted once, at configuration time.

That trust is bounded by two things. First, a **command** hook executes under the ADR-0023 process-group isolation: it runs in its own process group with a timeout, and on timeout or cancellation the whole group is killed cleanly, so a runaway hook cannot outlive its Run or leak child processes. Second, every hook fire is **visible**: each fire emits a transcript/launch line, and a *deciding* fire (one that blocked, denied, halted, or injected) is surfaced prominently, because a silent veto is worse than a loud one. This matches the fail-open-with-visibility ethos of ADR-0007 and ADR-0018: the model and the Run never fail because a hook is present, but the user always sees what a hook did.

Discovery and parse failures fail OPEN with a notice. A malformed `hooks` block in `config.json`, or a skill frontmatter `hooks:` that will not parse, is a skip-with-notice, not a fatal: the offending hook is dropped, a launch line records why, and the Run carries on. This mirrors the Skill (ADR-0058) and MCP (ADR-0056) discovery fronts exactly, where a broken manifest is a visible skip rather than a launch failure.

## The config schema

The on-disk shape is faithful to qwen's settings shape. Under the `hooks` key, each entry maps an event name to an array of hook-definition objects:

```
hooks:
  <EventName>:
    - matcher: <tool-name pattern>   # optional; only meaningful on tool events
      hooks:
        - type: command
          command: <shell command>
          timeout: <seconds>
        - type: http
          url: <endpoint>
          timeout: <seconds>
        - type: prompt
          prompt: <template with the event payload spliced in>
          timeout: <seconds>
```

The `matcher` is a tool-name pattern that scopes a tool event (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`) to matching tools; an absent matcher matches all tools, and matcher is inert on non-tool events. Each hook carries its `type` and the one field that type needs (`command`, `url`, or `prompt`) plus an optional `timeout`. A skill's frontmatter uses the SAME shape under its own `hooks:` key, so a hook reads identically whether it came from `config.json` or a `SKILL.md`; the only difference is scope (standing vs. session) and the `SUSPENDERS_SKILL_ROOT` a skill hook additionally sees.

One env var tunes the Stop path: `SUSPENDERS_STOP_HOOK_BLOCK_CAP` (qwen's `QWEN_CODE_STOP_HOOK_BLOCK_CAP`, renamed) caps how many times a Stop hook may force the Run to continue past a clean end before the cap wins and the Run stops anyway. It defaults to `8` and is clamped to a maximum of `100`; a missing, empty, non-integer, or below-`1` value falls back to the default. This bounds a Stop hook that keeps returning `continue:false` so it cannot loop the Run forever.

A `continue:false` stop from a Pre/PostToolUse hook is batch-granular: the current tool batch finishes before the Run stops, so every `tool_use` in the batch still gets its answering `tool_result` and none is left unanswered. The stop lands at the batch boundary, not mid-batch.

## What is rejected or deferred

- **The function hook type** is rejected, as argued above: an in-process SDK callback has no host to register it in a compiled Rust CLI, and porting it would mean an unusable plugin-callback ABI.
- **Async / non-blocking command hooks** (qwen's `async` flag, `PendingAsyncHook`, and the deferred-output collection) are qwen-specific plumbing for detaching a slow hook from the loop; Suspenders fires each hook inline and bounded by its timeout, so the async detachment machinery is out of this ADR.
- **The `sequential` per-definition flag** (qwen runs a definition's hooks concurrently unless it is set) is deferred; Suspenders' first cut fixes an ordering it can reason about rather than exposing the toggle.
- **Extension- and system-scoped hooks** (qwen's `Extensions` and `System` config sources) are out: Suspenders reads project, user, and session (skill) scopes only, matching the skill and MCP subsystems, which also load project + user rather than an extension level.
- **`allowedEnvVars` / per-hook `env` / `headers` / `shell` selection and the `once`/`if` guards** are qwen surface we may honor later; the first cut delivers the three types with a timeout and the full decision protocol, which is the load-bearing part.
- **http-URL validation (SSRF)** is a deferral. An **http** hook POSTs to whatever `url` its config names, with no allowlist or private-address guard, so a hook can reach an internal endpoint. This is consistent with the run_command shell tool under the no-approval trust model: a configured hook is already trusted to run arbitrary shell (ADR-0023), so a configured URL is no larger a grant than the shell it sits beside. The guard is deferred, not a claim that SSRF is impossible.
- **command-hook env inheritance** is a deferral. A **command** hook inherits the parent process environment rather than running under a scrubbed env, so it sees the same variables the harness does. This too matches the run_command shell tool: under the no-approval trust model a configured command hook is trusted like any shell-out, so env scrubbing is deferred surface rather than a security boundary this ADR draws.
