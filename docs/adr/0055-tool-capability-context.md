# Tool Calls reach the host through a Capability Context on the ToolCtx

A handful of Tool Calls need to reach BACK to the host for a live decision the model cannot make alone: ask the user to approve a gated command, put a question to the user, run a bounded side-query against the model, spawn a subagent, background a shell, change the Approval Mode. Those are *effects*, and a tool cannot own the channel they travel over (the Agent owns the mpsc). Yet the tool is where the effect originates, mid-run, not at a loop control point.

Those effect handles ride the `ToolCtx` as a Parameter Object, `Capabilities`: `Arc<dyn Trait>` seams for the effects, concrete `Arc`s for Run-scoped state. The carrier holds:

- `registry: Arc<ToolRegistry>` - concrete Run-scoped state; `tool_search` reveals deferred tools through it (ADR-0054).
- `read_cache: Arc<FileReadCache>` - concrete Run-scoped state (ADR-0060); `read_file` records a successful read, `notebook_edit` checks for a prior FULL read before mutating.
- `approver: Arc<dyn Approver>` - the tool-initiated Approval seam. The batch gate drives Approval through `RunDeps::request_approval`; the capability shares the exact same request path (`AgentApprover` is a near-verbatim twin of `AgentDeps::request_approval`).
- `side_query: Arc<dyn SideQuery>` - a bounded model prompt off the main Conversation; `web_fetch` runs its prompt-guided extraction through it.
- `questioner: Arc<dyn Questioner>` - `ask_user_question`'s modal round-trip (ADR-0057).
- `subagents: Arc<dyn SubagentSpawner>` - the `agent` tool spawns a child Run and awaits its settlement (ADR-0061).
- `bg_shells: Arc<dyn BackgroundShellSpawner>` - `run_command` with `is_background: true` hands the process to the Agent, which owns the detached lifecycle (ADR-0064).
- `approval_mode: Arc<AtomicApprovalMode>` and `plan_exit_notice: Arc<PendingManualPlanExit>` - concrete shared state for the live Approval Mode mirror and the one-shot manual-plan-exit notice (ADR-0067).
- `plan_mode: Arc<dyn PlanMode>` - the plan-lifecycle tools (`enter_plan_mode`/`exit_plan_mode`) reach the Agent-owned mode through it (ADR-0067).

The `ToolCtx` carries `caps: Capabilities` with accessors (`registry()`, `read_cache()`, ...), so the field paths stay internal to the carrier.

## Two effect channels, one terminus

Suspenders has two distinct channels a Run reaches its host through, and they are deliberately different shapes:

- **`RunDeps` (ADR-0011)** is the *loop-owned* channel: a `&mut D` static-dispatch bundle for the effects the Loop itself drives at control points (`complete`, `compact`, `checkpoint`, `drain_steering`, and `request_approval` as the batch gate calls it). Static dispatch because the Loop is `async fn run<D: RunDeps>` and monomorphises per caller; its async methods use RPITIT (edition 2024), no boxing, no `async_trait`.
- **`Capabilities` (this ADR)** is the *tool-owned* channel: an `Arc<dyn>` bundle for effects a Tool Call initiates while it runs, deep inside a `dyn Tool::run` where no `&mut D` reaches. Dynamic dispatch because the seam is `dyn` (its real impl lives in the Agent while the Run and its tools do not depend on the Agent), so its async methods MUST use `async_trait` - RPITIT is not object-safe. This is the opposite tradeoff from `RunDeps`, and it is correct for each: do not "fix" `Capabilities` to RPITIT (it cannot be `dyn`), and do not "fix" `RunDeps` to `async_trait` (it does not need boxing).

Both channels terminate at the SAME Agent mpsc when the effect is Agent-owned; the Agent is still the single owner of Event order (ADR-0017).

## Concrete state, dynamic effects

Only the things that reach back to the host for a decision are `dyn`. The `ToolRegistry`, the `FileReadCache`, and the ADR-0067 mode carriers are Run-scoped state the tools and the loop READ, not effects - they stay concrete `Arc`s, built fresh per Run by `run::run` (a child Run gets fresh, default carriers - a subagent does not cycle the mode).

## The degraded / real duality (headless posture)

Every effect capability has two impls:

- a **real** impl the Agent builds, tx-backed over its mpsc (`AgentApprover`, `AgentQuestioner`) or wired direct to the captured Llm (below);
- a **degraded** impl for a host with no channel to answer it - a headless run (ADR-0019), a test, or a child Run (`DenyingApprover`, `DecliningQuestioner`, `DenyingSideQuery`, `UnavailableSubagentSpawner`, `UnavailableBackgroundShellSpawner`, `SubagentPlanMode`).

The degraded posture never silently does the risky thing: `DenyingApprover::approve` returns `false` - a headless run with no approval channel must not execute a gated command; the safe answer is to deny, not to panic. `DecliningQuestioner` returns the VERBATIM qwen non-interactive string. `UnavailableSubagentSpawner` doubles as the RECURSION GUARD: a child Run's own `subagents` capability is degraded, so a subagent cannot spawn a subagent (ADR-0061). This is the headless seam (ADR-0019) applied to tool-initiated effects.

## Tx-backed or direct-to-Llm, by who owns the effect

A capability's real impl wires to whichever boundary OWNS the effect:

- **Agent-relayed** (tx-backed over the mpsc): `Approver` and `Questioner` - approval and questions are Agent/user-owned decisions (Standing Approvals, modals, forwarded replies); `BackgroundShellSpawner` and `PlanMode` - the Agent owns the detached-process registry and the Approval Mode.
- **Direct to the captured Llm** (no mpsc round-trip, no `RunMsg` variant): `SideQuery` (`LlmSideQuery` in `run::side_query`) and `SubagentSpawner` (`DirectSubagentSpawner` in `run::subagent`) - a side-query or a foreground child Run is just completions over the `Arc<dyn Llm>` and `Model` the Run already captured, mutating no Agent/Conversation state (no checkpoint, no next-speaker, no Conversation edit on the parent).

This keeps `caps.rs` free of any `agent`/`run` import. One structural consequence: `SideQueryRequest.model` is `Option<Model>` (a `None` defers to the captured main model), so the `tool` capability layer names `llm::model::Model`; to keep `tool <-> llm` acyclic, `ToolSpec` lives in the `content` leaf (the shared wire-shapes home) and `tool` re-exports it, so the tool authoring contract still reads `crate::tool::ToolSpec` with no `llm -> tool` edge.

The subagent contract:

```rust
struct SubagentRequest { subagent_type: String, prompt: String, model: Option<Model> }
struct SubagentResult { terminate_reason: String, result: String }
#[async_trait]
trait SubagentSpawner: Send + Sync {
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String>;
}
```

qwen's `agent` tool routes by `subagent_type` among the available subagent definitions, so the spawner resolves a def by name; `model: Option<Model>` is the per-subagent model seam, paired with the def's own `SubagentModel` (Inherit / Scoped).

Considered and rejected:

- **Routing `SideQuery` through the Agent like `Approver`.** A side-query touches no Agent-owned state (no Standing Approvals, no modal, no Conversation mutation), so an mpsc round-trip and a `RunMsg` variant would be ceremony around a completion the Run already captured.
- **A second `dyn RunDeps` instead of a separate `Capabilities` carrier.** `RunDeps` is loop-owned `&mut D` at control points; a Tool Call has neither the `&mut D` nor a control point. The two channels are genuinely different shapes; merging them would force `RunDeps` to be `dyn` (losing its RPITIT static dispatch) for no gain.
- **A `dyn` registry on `Capabilities`.** The registry is state, not an effect; making it `dyn` would hide a concrete type behind a seam that buys nothing.

Consequence: the `ToolCtx` is the single place a Tool Call reads BOTH its Run-scoped state and its tool-initiated effect seams, and the seam is not Agent-bound - a capability whose effect terminates at the Llm boundary wires there directly.
