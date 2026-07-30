# Tool Calls reach the host through a Capability Context on the ToolCtx

A handful of Tool Calls need to reach BACK to the host for a live decision the model cannot make alone: ask the user to approve a gated command, put a question to the user, run a bounded side-query against the model, spawn a subagent. Those are *effects*, and a tool cannot own the channel they travel over (the Agent owns the mpsc). Yet the tool is where the effect originates, mid-run, not at a loop control point.

We carry those effect handles onto the `ToolCtx` as a Parameter Object, `Capabilities`, holding `Arc<dyn Trait>` seams (F1):

```rust
pub struct Capabilities {
    pub registry: Arc<ToolRegistry>,      // concrete - Run-scoped state, not an effect
    pub approver: Arc<dyn Approver>,      // dyn - the tool-initiated effect seam (P1b)
    pub side_query: Arc<dyn SideQuery>,   // dyn - the bounded-model side-query seam (P2b)
    pub questioner: Arc<dyn Questioner>,  // dyn - the ask_user_question seam (P2a)
    // P4: subagents (deferred, see below)
}
```

The `ToolCtx` swaps its bare `registry` field for a `caps: Capabilities` and gains a `registry()` accessor, so the three registry-reading sites (`tool_search`, the Tools dispatch, the Loop's wire list) read through one method and the field path stays internal to the carrier.

## Two effect channels, one terminus

Suspenders now has two distinct channels a Run reaches its host through, and they are deliberately different shapes:

- **`RunDeps` (ADR-0011)** is the *loop-owned* channel: a `&mut D` static-dispatch bundle for the effects the Loop itself drives at control points (`complete`, `compact`, `checkpoint`, `drain_steering`, and `request_approval` as the batch gate calls it). Static dispatch because the Loop is `async fn run<D: RunDeps>` and monomorphises per caller; its async methods use RPITIT (edition 2024), no boxing, no `async_trait`.
- **`Capabilities` (this ADR)** is the *tool-owned* channel: an `Arc<dyn>` bundle for effects a Tool Call initiates while it runs, deep inside a `dyn Tool::run` where no `&mut D` reaches. Dynamic dispatch because the seam is `dyn` (its real impl lives in the Agent while the Run and its tools do not depend on the Agent), so its async methods MUST use `async_trait` - RPITIT is not object-safe. This is the opposite tradeoff from `RunDeps`, and it is correct for each: do not "fix" `Capabilities` to RPITIT (it cannot be `dyn`), and do not "fix" `RunDeps` to `async_trait` (it does not need boxing).

Both channels terminate at the SAME Agent mpsc. A Capability's real impl sends the same `RunMsg` a `RunDeps` method would; the Agent is still the single owner of Event order (ADR-0017).

## Concrete registry, dynamic effects

The `ToolRegistry` rides `Capabilities` as a concrete `Arc`, not a `dyn` seam, because it is not an effect - it is Run-scoped state the tools READ (`tool_search` reveals deferred tools through it; every other tool ignores it). Only the things that reach back to the host for a decision are `dyn`.

## The degraded / real duality (headless posture)

Every effect capability has two impls:

- a **real** impl the Agent builds, tx-backed over its mpsc (`AgentApprover`);
- a **degraded** impl for a host with no channel to answer it - a headless run (ADR-0019) or a test (`DenyingApprover`).

The degraded posture never silently does the risky thing: `DenyingApprover::approve` returns `false`. A headless run with no approval channel must not execute a gated command; the safe answer is to deny, not to panic. This is the headless seam (ADR-0019) applied to tool-initiated effects: the degraded impl IS the headless posture for this class of effect.

## The Approver proves the seam with a live wire

`Approver` is the one capability P1b lands, because its wire already exists: `RunMsg::RequestApproval` is already a variant, so `AgentApprover::approve` is a live effect (a near-verbatim lift of `AgentDeps::request_approval`), and it proves the whole DI mechanism end to end. It is threaded from the Agent to the Run through the `Capture` snapshot (the Agent owns the tx, so it builds the handle; the Run assembles it into `Capabilities` alongside the registry it builds itself).

P1b is *seam only, no behavior change*: no tool consumes `Capabilities.approver` yet - the batch gate still drives approval through `RunDeps::request_approval`. The `AgentApprover` and the gate share the exact request path; a later phase collapses that transient duplication once a tool initiates its own Approval. The seam is proven by unit tests, and every existing approval / batch / tool test passes unchanged.

## SideQuery lands wired DIRECT to the Llm, not the Agent (P2b)

`SideQuery` is the second capability to land (P2b, web_fetch's prompt-guided extraction), and it lands DIFFERENTLY from the Approver - a distinction that sharpens what "two channels, one terminus" means. The Approver's real impl (`AgentApprover`) relays over the Agent mpsc because approval is an Agent-OWNED decision: the Agent consults the Standing Approvals, opens the modal, and forwards the reply. A side-query owns none of that. Its only effect is a completion the Run ALREADY captured (the `Arc<dyn Llm>` and the `Model` on the `Capture`), and it mutates no Agent/Conversation state - it checkpoints nothing, logs nothing, and never touches the next-speaker fold. So its real impl, `LlmSideQuery`, is just that captured Llm boundary called with a transient `LlmRequest` (Thinking off, no tools, a no-op stream sink), off the main Conversation - exactly the shape `run::next_speaker` already uses for the other genuine side-query.

Because the effect terminates at the Llm boundary rather than the Agent mpsc, `LlmSideQuery` lives at that boundary (`run::side_query`), built by `run::run` from the `Capture`'s own `llm`/`model` - no new `Capture` field, no `RunMsg` variant, and crucially no Agent round-trip. This keeps `caps.rs` free of any `agent`/`run` import (it names only `Model`), and it means the degraded posture is a plain `DenyingSideQuery` (a host with no model channel returns an `Err` the tool folds into its own error result), symmetric with `DenyingApprover`.

One structural consequence: `SideQueryRequest.model` is `Option<Model>`, so the `tool` capability layer now names `llm::model::Model`. Paired with the pre-existing `LlmRequest.tools: Vec<ToolSpec>`, that would close a `tool <-> llm` cycle. `ToolSpec` therefore moved to the `content` leaf (the shared wire-shapes home, alongside `ContentBlock`'s `ToolUse`/`ToolResult`) and `tool` re-exports it, so the boundary carries `ToolSpec` with no `llm -> tool` edge while the tool authoring contract still reads `crate::tool::ToolSpec`. The `tool -> llm` edge (naming `Model`) is now one-directional and acyclic.

web_fetch consumes it live: it fetches, caps the content at 100 000 chars, wraps it in qwen's verbatim fallback prompt, and runs the extraction through `caps.side_query` with `model: None` (defer to the captured MAIN model, faithful to qwen) and `max_attempts: 1`. web_fetch does NOT call the Approver - its Approval is upstream in the batch gate (now domain-scoped, ADR-0024).

## Questioner LANDED tx-backed like the Approver (P2a, ADR-0057)

`Questioner` is the third capability to land (P2a, `ask_user_question`), and it lands like the Approver, NOT like SideQuery - the CONTRAST that completes the picture. A question is an Agent-relayed, USER-owned decision: the Agent broadcasts the request (opening the modal) and forwards the reply the user gives. So its real impl, `AgentQuestioner`, is tx-backed over the Agent mpsc (a `RunMsg::AskQuestion` and a reply oneshot), a near-twin of `AgentApprover` - where SideQuery, an Llm-owned effect, bypassed the mpsc entirely. The one place it DIVERGES from the Approver is that there is no auto/standing path: `ask_question` is unconditionally the pending leg (every question opens a modal), so the Agent holds a plain `question_replies` map with none of the `Approvals` fold beside it (see ADR-0057). Its degraded impl is `DecliningQuestioner`, which returns the VERBATIM qwen non-interactive string - symmetric with `DenyingApprover`/`DenyingSideQuery` as the headless posture. Threaded from the Agent to the Run through the `Capture` snapshot, exactly like the Approver.

## Deferral policy

The carrier plus `Approver` land in P1b; `SideQuery` lands in P2b; `Questioner` lands in P2a (above). The remaining capability (SubagentSpawner) lands in the phase that consumes it, NOT now. Building it now would mean a non-functional stub impl, because its `RunMsg` variant does not exist yet - dead code against the quality floor. Its trait signature is recorded here as the contract that phase implements:

```rust
// P4/F4 - SubagentSpawner. The `model` field is the F4 per-subagent seam
// (Opus-main / Qwen-scout): a subagent may run a different Model than the Run
// that spawned it.
struct SubagentRequest { prompt: String, model: Option<Model> }
struct SubagentResult { terminate_reason: String, result: String }
#[async_trait]
trait SubagentSpawner: Send + Sync {
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String>;
}
```

Considered and rejected:

- **Adding all four capabilities up front with stub degraded impls.** A capability with no consumer yet (SubagentSpawner) would be dead code (no `RunMsg` variant, no consumer), and a stub that only ever returns the degraded answer is untested behavior against the quality floor. Each lands with its consumer instead: `Approver` in P1b (its wire already existed), `SideQuery` in P2b (web_fetch consumes it), `Questioner` in P2a (`ask_user_question` consumes it), and the signature recorded above gives the remaining phase its contract without the dead code.
- **Routing `SideQuery` through the Agent like `Approver`.** A side-query touches no Agent-owned state (no Standing Approvals, no modal, no Conversation mutation), so an mpsc round-trip and a `RunMsg` variant would be ceremony around a completion the Run already captured. `LlmSideQuery` calls the captured Llm boundary directly instead - the effect terminates at the Llm, not the Agent (see the P2b section).
- **A second `dyn RunDeps` instead of a separate `Capabilities` carrier.** `RunDeps` is loop-owned `&mut D` at control points; a Tool Call has neither the `&mut D` nor a control point. The two channels are genuinely different shapes; merging them would force `RunDeps` to be `dyn` (losing its RPITIT static dispatch) for no gain.
- **A `dyn` registry on `Capabilities`.** The registry is state, not an effect; making it `dyn` would hide a concrete type behind a seam that buys nothing.

Consequence: the `ToolCtx` is the single place a Tool Call reads BOTH its Run-scoped state and its tool-initiated effect seams. The DI mechanism is proven live in P1b by the `Approver` (whose wire already existed) and consumed for the first time in P2b by `SideQuery` (web_fetch's extraction), which also shows the seam is not Agent-bound: a capability whose effect terminates at the Llm boundary wires there directly.
