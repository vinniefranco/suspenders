//! Tool Capability Context - the DI seam a Tool Call reaches the host through.
//!
//! A Tool Call needs two very different kinds of thing from its host. Most of
//! what a tool touches is data the Session already resolved (the Project Root,
//! the Result Cap, the command timeout) - those ride the [`crate::tool::ToolCtx`]
//! as plain fields. But a handful of tools need to reach BACK to the host for a
//! live decision the model cannot make alone: ask the user to approve a gated
//! command, put a question to the user, run a side-query against the model, spawn
//! a subagent. Those are *effects*, and a tool cannot own the channel they travel
//! (the Agent does). [`Capabilities`] is the Parameter Object that carries those
//! effect handles onto the ToolCtx as `Arc<dyn Trait>` seams (ADR-0055).
//!
//! ## Two effect channels, one terminus
//!
//! The Run already reaches its host through [`crate::run::deps::RunDeps`] - a
//! loop-owned `&mut D` static-dispatch bundle for the effects the Loop itself
//! drives (`complete`, `compact`, `checkpoint`, `request_approval` as the batch
//! gate calls it). Capabilities is the OTHER channel: the tool-owned `Arc<dyn>`
//! bundle for effects a Tool Call initiates while it runs. Both channels
//! terminate at the same Agent mpsc - a Capability's real impl sends the same
//! [`crate::agent::RunMsg`] a RunDeps method would (ADR-0011, ADR-0055).
//!
//! ## Concrete registry, dynamic effects
//!
//! The [`crate::tool_registry::ToolRegistry`] rides Capabilities as a concrete
//! `Arc` (`tool_search` reveals deferred tools through it; every other tool
//! ignores it). It is not an effect - it is Run-scoped state the tools read - so
//! it stays concrete. The effect handles are `dyn` because their real impl lives
//! in the Agent (which owns the channel) while the Run and the tools that consume
//! them do not depend on the Agent (Ports and Adapters).
//!
//! ## Degraded posture (headless)
//!
//! Every effect capability has a *degraded* impl for a host with no channel to
//! answer it (a headless run, a test). The degraded posture never silently does
//! the risky thing: [`DenyingApprover`] denies rather than approves. This mirrors
//! the single-binary headless seam (ADR-0019): a capability the host cannot
//! fulfil returns the safe answer, not a panic.
//!
//! ## Deferred capabilities (contract, not code)
//!
//! P1b lands the carrier plus the one capability whose wire already exists -
//! [`Approver`], because [`crate::agent::RunMsg::RequestApproval`] is already a
//! variant, so its tx-backed impl is a live effect and proves the whole seam. The
//! other three capabilities land in the phase that consumes them, because their
//! `RunMsg` variants do not exist yet and a stub impl would be dead code against
//! the quality floor. Their trait signatures are recorded here (and in ADR-0055)
//! as the contract those phases implement - text only, no trait code:
//!
//! ```text
//! // P2a - Questioner (faithful to qwen askUserQuestion.ts). Puts one or more
//! // questions to the user and yields their (option-index, label) picks.
//! struct QuestionOption { label: String, description: String }
//! struct Question {
//!     question: String,
//!     header: String,
//!     options: Vec<QuestionOption>,
//!     multi_select: bool,
//! }
//! #[async_trait]
//! trait Questioner: Send + Sync {
//!     async fn ask(&self, questions: Vec<Question>) -> Result<Vec<(usize, String)>, String>;
//! }
//! // Degraded (non-interactive) string, VERBATIM:
//! // "Cannot ask user questions in non-interactive mode without ACP support. \
//! //  Please run in interactive mode or enable ACP mode to use this tool."
//! // Decline (user cancelled) string, VERBATIM:
//! // "User declined to answer the questions."
//!
//! // P2b - SideQuery (faithful to qwen web-fetch.ts). Runs a bounded model
//! // side-query (a prompt against the model, off the main Conversation).
//! // NOTE: qwen's runSideQuery takes multi-part `contents`; `user_content:
//! // String` is narrowed to web_fetch's single text part. Multimodal is FULL
//! // scope (D3), so P2b widens this to a parts list if a second consumer needs it.
//! struct SideQueryRequest {
//!     system: String,
//!     user_content: String,
//!     model: Option<Model>,
//!     max_attempts: u32,
//! }
//! #[async_trait]
//! trait SideQuery: Send + Sync {
//!     async fn run(&self, request: SideQueryRequest) -> Result<String, String>;
//! }
//!
//! // P4/F4 - SubagentSpawner. Spawns a child Run and awaits its settlement. The
//! // `model` field is the F4 per-subagent seam (Opus-main / Qwen-scout): a
//! // subagent may run a different Model than the Run that spawned it.
//! struct SubagentRequest { prompt: String, model: Option<Model> }
//! struct SubagentResult { terminate_reason: String, result: String }
//! #[async_trait]
//! trait SubagentSpawner: Send + Sync {
//!     async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String>;
//! }
//! ```

use std::sync::Arc;

use crate::tool_registry::ToolRegistry;

/// The effect handles and Run-scoped state a Tool Call reaches its host through
/// (ADR-0055). A Parameter Object carried on the [`crate::tool::ToolCtx`]: the
/// concrete Tool Registry the Run built plus the `dyn` effect seams a tool
/// initiates through. All fields are `Arc`, so a `Clone` is a handful of refcount
/// bumps and the ToolCtx stays cheap to clone per Tool Call.
///
/// P1b carries the registry and the [`Approver`]. The other three effect
/// capabilities (Questioner, SideQuery, SubagentSpawner) land in the phase that
/// consumes them - their signatures are recorded in this module's docs and in
/// ADR-0055.
#[derive(Clone)]
pub struct Capabilities {
    /// The Tool Registry the Run built once at its start (F3): concrete, because
    /// it is Run-scoped state the tools read, not an effect. `tool_search`
    /// reveals deferred tools through it; every other tool ignores it.
    pub registry: Arc<ToolRegistry>,
    /// The Approval effect seam: the tool-initiated path to the user's decision
    /// on a gated action. `dyn` because its real impl lives in the Agent (which
    /// owns the mpsc), while the Run and its tools do not depend on the Agent.
    pub approver: Arc<dyn Approver>,
    // P2a: questioner, P2b: side_query, P4: subagents (see ADR-0055; not added
    // now - each lands in its consuming phase so no field is dead code).
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn Approver>` is not `Debug`, so this is hand-written (mirroring
        // `ToolRegistry`'s own Debug): the registry prints in full, the effect
        // seam prints as an opaque marker. Keeps `ToolCtx` on `#[derive(Debug)]`.
        f.debug_struct("Capabilities")
            .field("registry", &self.registry)
            .field("approver", &"<dyn>")
            .finish()
    }
}

/// The Approval effect: a Tool Call asks the host to approve a gated action.
///
/// `command` is the exact string the user reads and a Standing Approval matches
/// against (ADR-0005) - the same wording the batch gate shows, so a tool-initiated
/// Approval and a gate Approval are indistinguishable to the user. `id` is the
/// per-call reference (baud's `make_ref()`). The result is the user's decision:
/// `true` approves, `false` denies.
///
/// Object-safe and `async_trait`-boxed on purpose: this is a `dyn` seam, so its
/// async method must return a boxed future (RPITIT is not object-safe). That is
/// the opposite tradeoff from [`crate::run::deps::RunDeps`], which is static
/// dispatch and uses RPITIT - see ADR-0055 for why the two channels differ.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// Asks the host to approve the gated action described by `command`. Blocks
    /// until the user answers (there is no timeout - the user decides). Returns
    /// the decision.
    async fn approve(&self, id: String, command: String) -> bool;
}

/// The degraded [`Approver`]: denies every Approval.
///
/// The headless posture (ADR-0019) and the test posture share this: a host with
/// no approval channel must not silently execute a gated command, so the safe
/// answer is to deny. Nothing consumes it in P1b anyway (the batch gate still
/// drives approval through [`crate::run::deps::RunDeps`]); it exists so the
/// carrier can always be built and so `Capabilities::for_test` has a real handle.
pub struct DenyingApprover;

#[async_trait::async_trait]
impl Approver for DenyingApprover {
    async fn approve(&self, _id: String, _command: String) -> bool {
        false
    }
}

#[cfg(test)]
impl Capabilities {
    /// Capabilities over the full built-in registry and a denying Approver, for
    /// tests that need a real [`crate::tool::ToolCtx`] but no live approval
    /// channel. The single test construction site, so a future capability touches
    /// one place rather than every tool test helper.
    pub fn for_test() -> Self {
        Capabilities {
            registry: crate::tool_registry::test_registry(),
            approver: Arc::new(DenyingApprover),
        }
    }

    /// Capabilities over a caller-supplied registry (and a denying Approver), for
    /// the `tool_search` tests that build a registry with specific deferral
    /// flags. Mirrors [`Capabilities::for_test`] but lets the caller pin the
    /// registry contents.
    pub fn for_test_with_registry(registry: Arc<ToolRegistry>) -> Self {
        Capabilities {
            registry,
            approver: Arc::new(DenyingApprover),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;

    /// A fake real [`Approver`] that answers with a fixed decision. Stands in for
    /// the tx-backed `AgentApprover` in a unit test: proves `Capabilities` holds
    /// a real answer behind `Arc<dyn Approver>` without wiring an Agent mpsc.
    struct FakeApprover {
        answer: bool,
    }

    #[async_trait::async_trait]
    impl Approver for FakeApprover {
        async fn approve(&self, _id: String, _command: String) -> bool {
            self.answer
        }
    }

    #[tokio::test]
    async fn denying_approver_denies() {
        let approver = DenyingApprover;
        assert!(!approver.approve("id".to_string(), "rm -rf /".to_string()).await);
    }

    #[tokio::test]
    async fn a_real_approver_returns_its_injected_decision() {
        let caps = Capabilities {
            registry: crate::tool_registry::test_registry(),
            approver: Arc::new(FakeApprover { answer: true }),
        };
        // The seam is proven with a live wire: the decision travels back through
        // the `Arc<dyn Approver>` the carrier holds.
        assert!(
            caps.approver
                .approve("id".to_string(), "ls".to_string())
                .await
        );
    }

    #[test]
    fn capabilities_clones_and_debug_prints() {
        let caps = Capabilities {
            registry: crate::tool_registry::test_registry(),
            approver: Arc::new(FakeApprover { answer: false }),
        };
        let cloned = caps.clone();
        // The clone shares the same handles (Arc), and both print with the
        // registry expanded and the effect seam opaque.
        let rendered = format!("{cloned:?}");
        assert!(rendered.contains("Capabilities"));
        assert!(rendered.contains("approver"));
        assert!(rendered.contains("<dyn>"));
    }

    #[test]
    fn capabilities_is_send() {
        // Compile-proof: the carrier must cross the `tokio::spawn` at the Agent,
        // so `Arc<dyn Approver>` must be Send + Sync.
        fn assert_send<T: Send>() {}
        assert_send::<Capabilities>();
    }

    #[test]
    fn tool_ctx_for_test_constructs_clones_and_debug_prints() {
        let ctx = ToolCtx::for_test("/nowhere".into(), 10_000);
        let cloned = ctx.clone();
        let rendered = format!("{cloned:?}");
        assert!(rendered.contains("ToolCtx"));
        assert!(rendered.contains("Capabilities"));
    }
}
