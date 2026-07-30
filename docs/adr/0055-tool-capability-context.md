# Tool Calls reach the host through a Capability Context on the ToolCtx

A handful of Tool Calls need to reach BACK to the host for a live decision the model cannot make alone: ask the user to approve a gated command, put a question to the user, run a bounded side-query against the model, spawn a subagent. Those are *effects*, and a tool cannot own the channel they travel over (the Agent owns the mpsc). Yet the tool is where the effect originates, mid-run, not at a loop control point.

We carry those effect handles onto the `ToolCtx` as a Parameter Object, `Capabilities`, holding `Arc<dyn Trait>` seams (F1):

```rust
pub struct Capabilities {
    pub registry: Arc<ToolRegistry>,   // concrete - Run-scoped state, not an effect
    pub approver: Arc<dyn Approver>,   // dyn - the tool-initiated effect seam
    // P2a: questioner, P2b: side_query, P4: subagents (deferred, see below)
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

## Deferral policy

Only the carrier plus `Approver` land in P1b. The other three capabilities (Questioner, SideQuery, SubagentSpawner) land in the phase that consumes them, NOT now. Building them now would mean non-functional stub impls, because their `RunMsg` variants do not exist yet - dead code against the quality floor. Their trait signatures are recorded here as the contract those phases implement:

```rust
// P2a - Questioner (faithful to qwen askUserQuestion.ts).
struct QuestionOption { label: String, description: String }
struct Question {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
}
#[async_trait]
trait Questioner: Send + Sync {
    async fn ask(&self, questions: Vec<Question>) -> Result<Vec<(usize, String)>, String>;
}
// Degraded (non-interactive) string, VERBATIM:
// "Cannot ask user questions in non-interactive mode without ACP support. \
//  Please run in interactive mode or enable ACP mode to use this tool."
// Decline (user cancelled) string, VERBATIM:
// "User declined to answer the questions."

// P2b - SideQuery (faithful to qwen web-fetch.ts).
// NOTE: qwen's runSideQuery takes multi-part `contents`; `user_content: String`
// narrows to web_fetch's single text part. Multimodal is FULL scope (D3), so P2b
// widens this to a parts list if a second consumer needs it.
struct SideQueryRequest {
    system: String,
    user_content: String,
    model: Option<Model>,
    max_attempts: u32,
}
#[async_trait]
trait SideQuery: Send + Sync {
    async fn run(&self, request: SideQueryRequest) -> Result<String, String>;
}

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

- **Adding all four capabilities now with stub degraded impls.** Three of the four would be dead code (no `RunMsg` variant, no consumer), and a stub that only ever returns the degraded answer is untested behavior against the quality floor. The signatures recorded above give the later phases their contract without the dead code.
- **A second `dyn RunDeps` instead of a separate `Capabilities` carrier.** `RunDeps` is loop-owned `&mut D` at control points; a Tool Call has neither the `&mut D` nor a control point. The two channels are genuinely different shapes; merging them would force `RunDeps` to be `dyn` (losing its RPITIT static dispatch) for no gain.
- **A `dyn` registry on `Capabilities`.** The registry is state, not an effect; making it `dyn` would hide a concrete type behind a seam that buys nothing.

Consequence: the `ToolCtx` is the single place a Tool Call reads BOTH its Run-scoped state and its tool-initiated effect seams, and the DI mechanism is proven live in P1b by the one capability whose wire already exists.
