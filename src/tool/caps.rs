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
//! variant, so its tx-backed impl is a live effect and proves the whole seam.
//! P2b lands [`SideQuery`], the one capability wired DIRECT to the [`Llm`]
//! boundary rather than the Agent mpsc: a bounded model side-query touches no
//! Agent/Conversation state (no next-speaker, no checkpoint), so its real impl
//! ([`crate::run::side_query::LlmSideQuery`]) is just the captured Llm called off
//! the main Conversation, and its degraded impl is [`DenyingSideQuery`]. P2a
//! lands [`Questioner`], a SECOND tx-backed capability (like the Approver, but
//! with NO Standing-Approval / auto path - every question opens a modal): its
//! real impl ([`crate::agent::deps::AgentQuestioner`]) relays a
//! [`crate::agent::RunMsg::AskQuestion`] and awaits the user's picks, and its
//! degraded impl is [`DecliningQuestioner`]. P4/F4 lands [`SubagentSpawner`],
//! the fourth capability and the SECOND wired DIRECT to the [`Llm`] boundary
//! (like [`SideQuery`], not the Agent mpsc): a foreground subagent is a child
//! Run driven inline off the captured Llm, touching no Agent/Conversation state,
//! so no round-trip is needed. Its real impl
//! ([`crate::run::subagent::DirectSubagentSpawner`]) resolves a subagent
//! definition and drives a child Run to settlement; its degraded impl is
//! [`UnavailableSubagentSpawner`], which is ALSO the recursion guard - a child
//! Run's own subagents capability is degraded, so a subagent cannot spawn a
//! subagent (ADR-0061). Every recorded-signature capability has now landed; the
//! deferral policy this module opened with is fully discharged.

use std::sync::Arc;

use crate::llm::model::Model;
use crate::tool::read_cache::FileReadCache;
use crate::tool_registry::ToolRegistry;

/// The effect handles and Run-scoped state a Tool Call reaches its host through
/// (ADR-0055). A Parameter Object carried on the [`crate::tool::ToolCtx`]: the
/// concrete Tool Registry the Run built plus the `dyn` effect seams a tool
/// initiates through. All fields are `Arc`, so a `Clone` is a handful of refcount
/// bumps and the ToolCtx stays cheap to clone per Tool Call.
///
/// P1b carries the registry and the [`Approver`]; P2b adds the [`SideQuery`];
/// P2a adds the [`Questioner`]; P4/F4 adds the [`SubagentSpawner`]. Every effect
/// capability has now landed - the carrier is complete.
#[derive(Clone)]
pub struct Capabilities {
    /// The Tool Registry the Run built once at its start (F3): concrete, because
    /// it is Run-scoped state the tools read, not an effect. `tool_search`
    /// reveals deferred tools through it; every other tool ignores it.
    pub registry: Arc<ToolRegistry>,
    /// The Run-scoped file-read cache (F6, ADR-0060): concrete, like the
    /// registry, because it is Run-scoped state the tools read/write, not an
    /// effect. read_file records a successful read into it; notebook_edit checks
    /// it for a prior FULL read before mutating a notebook. Enforcement only -
    /// the qwen `file_unchanged` read fast-path is DEFERRED (ADR-0060).
    pub read_cache: Arc<FileReadCache>,
    /// The Approval effect seam: the tool-initiated path to the user's decision
    /// on a gated action. `dyn` because its real impl lives in the Agent (which
    /// owns the mpsc), while the Run and its tools do not depend on the Agent.
    pub approver: Arc<dyn Approver>,
    /// The Side-Query effect seam (P2b): a tool runs a bounded model prompt off
    /// the main Conversation (web_fetch's prompt-guided extraction). `dyn` like
    /// the Approver, but its real impl ([`crate::run::side_query::LlmSideQuery`])
    /// wires DIRECT to the captured [`crate::llm::Llm`], not the Agent mpsc - the
    /// side-query touches no Agent/Conversation state, so it needs no round-trip.
    pub side_query: Arc<dyn SideQuery>,
    /// The Question effect seam (P2a, `ask_user_question`): a tool puts one or
    /// more questions to the user and reads back their picks. `dyn` and tx-backed
    /// like the Approver (its real impl lives in the Agent which owns the mpsc),
    /// but with NO Standing-Approval / auto path - every question opens a modal.
    pub questioner: Arc<dyn Questioner>,
    /// The Subagent effect seam (P4/F4, ADR-0061): the `agent` tool spawns a
    /// child Run and awaits its settlement through this. `dyn` and, like the
    /// SideQuery, wired DIRECT to the [`Llm`] boundary rather than the Agent mpsc
    /// (a foreground subagent touches no Agent/Conversation state), so its real
    /// impl ([`crate::run::subagent::DirectSubagentSpawner`]) drives the child
    /// Run inline off the captured Llm. The degraded impl
    /// ([`UnavailableSubagentSpawner`]) is ALSO the recursion guard: a child
    /// Run's own subagents capability is degraded, so a subagent cannot spawn a
    /// subagent.
    pub subagents: Arc<dyn SubagentSpawner>,
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn Approver>` is not `Debug`, so this is hand-written (mirroring
        // `ToolRegistry`'s own Debug): the registry prints in full, the effect
        // seam prints as an opaque marker. Keeps `ToolCtx` on `#[derive(Debug)]`.
        f.debug_struct("Capabilities")
            .field("registry", &self.registry)
            .field("read_cache", &self.read_cache)
            .field("approver", &"<dyn>")
            .field("side_query", &"<dyn>")
            .field("questioner", &"<dyn>")
            .field("subagents", &"<dyn>")
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

/// One bounded model side-query as a Tool Call assembles it (faithful to qwen's
/// `runSideQuery`, narrowed to web_fetch's shape). `system` is the extraction
/// instruction, `user_content` the single text part (qwen's multi-part
/// `contents` narrowed to one text part - ADR-0055 D3 widens this if a second
/// consumer needs multimodal). `model` pins a specific Model or, `None`, defers
/// to the Run's captured main Model (web_fetch pins the main model by passing
/// `None`). `max_attempts` caps the retry loop (web_fetch passes 1: a
/// best-effort extraction the outer error path already handles).
pub struct SideQueryRequest {
    pub system: String,
    pub user_content: String,
    pub model: Option<Model>,
    pub max_attempts: u32,
}

/// The Side-Query effect: a Tool Call runs a bounded model prompt off the main
/// Conversation and reads back the reply text (qwen's `runSideQuery`). Unlike
/// [`Approver`], its real impl ([`crate::run::side_query::LlmSideQuery`]) does
/// NOT travel the Agent mpsc: a side-query mutates no Agent/Conversation state
/// (no checkpoint, no next-speaker), so it calls the captured [`crate::llm::Llm`]
/// boundary directly. `dyn` all the same, so the degraded posture can answer a
/// host with no model channel.
///
/// Object-safe and `async_trait`-boxed for the same reason as [`Approver`]: a
/// `dyn` seam's async method must return a boxed future (RPITIT is not
/// object-safe). See ADR-0055 for the two-channel tradeoff.
#[async_trait::async_trait]
pub trait SideQuery: Send + Sync {
    /// Runs the side-query and returns the reply text, or an `Err` describing
    /// the failure (the caller wraps it into its own error shape - web_fetch's
    /// `Error during fetch for {url}: {message}`).
    async fn run(&self, request: SideQueryRequest) -> Result<String, String>;
}

/// The degraded [`SideQuery`]: a host with no model channel (a headless run
/// without an Llm, a test) cannot run one, so every side-query is an `Err`.
///
/// Mirrors [`DenyingApprover`] as the headless/test posture (ADR-0019): the
/// degraded impl returns the safe answer - here a plain failure the tool folds
/// into its own error result - rather than panicking or silently succeeding.
pub struct DenyingSideQuery;

#[async_trait::async_trait]
impl SideQuery for DenyingSideQuery {
    async fn run(&self, _request: SideQueryRequest) -> Result<String, String> {
        Err("side queries are unavailable in this environment".into())
    }
}

/// One selectable answer for a [`Question`] (qwen `QuestionOption`): the `label`
/// the user reads and picks, and a `description` that explains what the choice
/// means. The label is what a pick records as the answer value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// One question the model puts to the user (qwen `Question`): the full
/// `question` text, a short `header` chip (<= 12 chars, the tool validates it),
/// the 2-4 `options` to choose from, and whether the user may pick more than one
/// (`multi_select`). qwen ALWAYS appends an "Other" row the UI offers on top of
/// these, so a user can always answer free-form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// The Question effect: a Tool Call puts one or more [`Question`]s to the user
/// and reads back their picks (qwen `askUserQuestion`). The `Ok` is one
/// `(question_index, answer_value)` per answered question - `answer_value` is the
/// chosen option's label (or, for a `multi_select` question, the joined selected
/// labels, one String per question). The `Err` is the VERBATIM degraded/decline
/// string the tool returns as its content.
///
/// Tx-backed like [`Approver`] (its real impl lives in the Agent which owns the
/// mpsc), but with NO Standing-Approval / auto path: every question opens a
/// modal. Object-safe and `async_trait`-boxed for the same reason as the other
/// `dyn` seams (RPITIT is not object-safe; see ADR-0055).
#[async_trait::async_trait]
pub trait Questioner: Send + Sync {
    /// Asks the user the given questions. Blocks until they answer (there is no
    /// timeout - the user decides). Returns the `(question_index, answer_value)`
    /// picks, or an `Err` carrying the degraded/decline string.
    async fn ask(&self, questions: Vec<Question>) -> Result<Vec<(usize, String)>, String>;
}

/// The degraded [`Questioner`]: a host with no question channel (a headless run,
/// a test) cannot put a question to the user, so every ask is the VERBATIM qwen
/// non-interactive `Err` the tool returns as its content.
///
/// Mirrors [`DenyingApprover`]/[`DenyingSideQuery`] as the headless/test posture
/// (ADR-0019): the degraded impl returns the safe answer - here the exact string
/// qwen's `askUserQuestion` returns in non-interactive mode without ACP - rather
/// than panicking or silently succeeding.
pub struct DecliningQuestioner;

/// The VERBATIM qwen non-interactive string (askUserQuestion.ts): what the tool
/// returns when there is no interactive channel to ask the user through. Shared
/// by [`DecliningQuestioner`] and the tool's own headless guard.
pub const NON_INTERACTIVE_MESSAGE: &str = "Cannot ask user questions in non-interactive mode without ACP support. Please run in interactive mode or enable ACP mode to use this tool.";

#[async_trait::async_trait]
impl Questioner for DecliningQuestioner {
    async fn ask(&self, _questions: Vec<Question>) -> Result<Vec<(usize, String)>, String> {
        Err(NON_INTERACTIVE_MESSAGE.to_string())
    }
}

/// One subagent launch as the `agent` tool assembles it (P4/F4, ADR-0061).
/// `subagent_type` names the subagent definition to run (the one contract
/// amendment over the recorded signature - qwen's `agent` tool routes by a
/// `subagent_type` param, so the spawner resolves the def by name rather than
/// running a single fixed child). `prompt` is the task the child Run opens on.
/// `model` is the F4 per-subagent seam (Opus-main / Qwen-scout): an explicit
/// Model override for this launch, or `None` to defer to the def's own model
/// choice (Inherit -> the parent's Model). The SAME shared `Arc<dyn Llm>`
/// (Dispatcher) routes whichever Model to its Provider, so no per-subagent Llm
/// is ever built.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentRequest {
    pub subagent_type: String,
    pub prompt: String,
    pub model: Option<Model>,
}

/// A settled subagent's result (P4/F4, ADR-0061), the child Run's outcome mapped
/// to qwen's `AgentTerminateMode` vocabulary. `terminate_reason` is `"GOAL"`
/// (the child completed), `"MAX_TURNS"` (it hit its turn bound), or `"ERROR"`
/// (it failed or exhausted its budget) - TIMEOUT/CANCELLED are deferred with the
/// background path (ADR-0061). `result` is the child's last assistant text (the
/// answer it relays to the parent), with any trailing Voice close marker
/// stripped so the parent never sees Suspenders' internal marker as the
/// subagent's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentResult {
    pub terminate_reason: String,
    pub result: String,
}

/// The Subagent effect: the `agent` tool spawns a child Run and awaits its
/// settlement (qwen's `AgentTool` launching an `AgentHeadless`). Wired DIRECT to
/// the [`Llm`](crate::llm::Llm) boundary like [`SideQuery`], not the Agent mpsc:
/// a foreground subagent drives a child Run inline off the captured Llm and
/// touches no Agent/Conversation state, so it needs no round-trip. `dyn` all the
/// same, so the degraded posture can answer a host with no subagent channel -
/// which is also the recursion guard (a child Run's own subagents capability is
/// degraded).
///
/// Object-safe and `async_trait`-boxed for the same reason as the other `dyn`
/// seams (RPITIT is not object-safe; see ADR-0055/ADR-0061).
#[async_trait::async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// Spawns the subagent and awaits its settlement, yielding the
    /// [`SubagentResult`], or an `Err` describing why it could not launch (an
    /// unknown subagent type, an unresolvable Model) - the `agent` tool folds the
    /// `Err` into its own error result.
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String>;
}

/// The degraded [`SubagentSpawner`]: a host with no subagent channel (a headless
/// run, a test) cannot spawn one, so every spawn is an `Err`. It is ALSO the
/// recursion guard (ADR-0061): a child Run's Capabilities carry this degraded
/// impl, so a subagent that tried to spawn a subagent gets the same `Err` a
/// headless host would - a subagent cannot recurse.
///
/// Mirrors [`DenyingApprover`]/[`DenyingSideQuery`]/[`DecliningQuestioner`] as
/// the headless/test posture (ADR-0019): the degraded impl returns the safe
/// answer - here a plain failure the tool folds into its own error result -
/// rather than panicking or silently succeeding.
pub struct UnavailableSubagentSpawner;

#[async_trait::async_trait]
impl SubagentSpawner for UnavailableSubagentSpawner {
    async fn spawn(&self, _request: SubagentRequest) -> Result<SubagentResult, String> {
        Err("subagents are unavailable in this environment".into())
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
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
        }
    }

    /// Capabilities over a caller-supplied registry (and denying effect seams),
    /// for the `tool_search` tests that build a registry with specific deferral
    /// flags. Mirrors [`Capabilities::for_test`] but lets the caller pin the
    /// registry contents.
    pub fn for_test_with_registry(registry: Arc<ToolRegistry>) -> Self {
        Capabilities {
            registry,
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`SideQuery`] (and the full built-in
    /// registry + a denying Approver), for the web_fetch tests that inject a
    /// scripted side-query and assert what it received. The single side-query
    /// construction site, so a future capability touches one place.
    pub fn for_test_with_side_query(side_query: Arc<dyn SideQuery>) -> Self {
        Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query,
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`Questioner`] (and the full built-in
    /// registry + denying Approver/SideQuery), for the `ask_user_question` tests
    /// that inject a scripted questioner and assert what it received/returned. The
    /// single questioner construction site, so a future capability touches one
    /// place.
    pub fn for_test_with_questioner(questioner: Arc<dyn Questioner>) -> Self {
        Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner,
            subagents: Arc::new(UnavailableSubagentSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`SubagentSpawner`] (and the full
    /// built-in registry + denying Approver/SideQuery/Questioner), for the
    /// `agent`-tool tests that inject a scripted spawner and assert what it
    /// received/returned. The single subagents construction site, so a future
    /// consumer touches one place.
    pub fn for_test_with_subagents(subagents: Arc<dyn SubagentSpawner>) -> Self {
        Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents,
        }
    }

    /// Capabilities over a caller-supplied [`FileReadCache`] (and the full
    /// built-in registry + denying effect seams), for the notebook_edit tests
    /// that pre-populate the cache (or share it with read_file) and assert the
    /// read-before-edit enforcement. The single read-cache construction site, so
    /// a future consumer touches one place.
    pub fn for_test_with_read_cache(read_cache: Arc<FileReadCache>) -> Self {
        Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache,
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
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

    /// A fake real [`SideQuery`] that answers with a fixed reply text. Proves
    /// `Capabilities` holds a real answer behind `Arc<dyn SideQuery>` without
    /// wiring an Llm.
    struct FakeSideQuery {
        reply: String,
    }

    #[async_trait::async_trait]
    impl SideQuery for FakeSideQuery {
        async fn run(&self, _request: SideQueryRequest) -> Result<String, String> {
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn a_real_approver_returns_its_injected_decision() {
        let caps = Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(FakeApprover { answer: true }),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
        };
        // The seam is proven with a live wire: the decision travels back through
        // the `Arc<dyn Approver>` the carrier holds.
        assert!(
            caps.approver
                .approve("id".to_string(), "ls".to_string())
                .await
        );
    }

    #[tokio::test]
    async fn denying_side_query_errs() {
        let sq = DenyingSideQuery;
        let err = sq
            .run(SideQueryRequest {
                system: "sys".into(),
                user_content: "content".into(),
                model: None,
                max_attempts: 1,
            })
            .await
            .unwrap_err();
        assert_eq!(err, "side queries are unavailable in this environment");
    }

    #[tokio::test]
    async fn a_real_side_query_returns_its_injected_reply() {
        let caps = Capabilities::for_test_with_side_query(Arc::new(FakeSideQuery {
            reply: "extracted".into(),
        }));
        // The seam is proven with a live wire: the reply travels back through the
        // `Arc<dyn SideQuery>` the carrier holds.
        let out = caps
            .side_query
            .run(SideQueryRequest {
                system: "sys".into(),
                user_content: "content".into(),
                model: None,
                max_attempts: 1,
            })
            .await
            .unwrap();
        assert_eq!(out, "extracted");
    }

    #[test]
    fn capabilities_clones_and_debug_prints() {
        let caps = Capabilities {
            registry: crate::tool_registry::test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(FakeApprover { answer: false }),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
        };
        let cloned = caps.clone();
        // The clone shares the same handles (Arc), and both print with the
        // registry expanded and the effect seams opaque.
        let rendered = format!("{cloned:?}");
        assert!(rendered.contains("Capabilities"));
        assert!(rendered.contains("read_cache"));
        assert!(rendered.contains("approver"));
        assert!(rendered.contains("side_query"));
        assert!(rendered.contains("questioner"));
        assert!(rendered.contains("subagents"));
        assert!(rendered.contains("<dyn>"));
    }

    /// A fake real [`SubagentSpawner`] that answers with a fixed result. Proves
    /// `Capabilities` holds a real answer behind `Arc<dyn SubagentSpawner>`
    /// without wiring a child Run.
    struct FakeSpawner {
        result: String,
    }

    #[async_trait::async_trait]
    impl SubagentSpawner for FakeSpawner {
        async fn spawn(&self, _request: SubagentRequest) -> Result<SubagentResult, String> {
            Ok(SubagentResult {
                terminate_reason: "GOAL".into(),
                result: self.result.clone(),
            })
        }
    }

    #[tokio::test]
    async fn unavailable_subagent_spawner_errs() {
        let spawner = UnavailableSubagentSpawner;
        let err = spawner
            .spawn(SubagentRequest {
                subagent_type: "general-purpose".into(),
                prompt: "do a thing".into(),
                model: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err, "subagents are unavailable in this environment");
    }

    #[tokio::test]
    async fn a_real_subagent_spawner_returns_its_injected_result() {
        let caps = Capabilities::for_test_with_subagents(Arc::new(FakeSpawner {
            result: "the findings".into(),
        }));
        // The seam is proven with a live wire: the result travels back through
        // the `Arc<dyn SubagentSpawner>` the carrier holds.
        let out = caps
            .subagents
            .spawn(SubagentRequest {
                subagent_type: "general-purpose".into(),
                prompt: "investigate".into(),
                model: None,
            })
            .await
            .unwrap();
        assert_eq!(out.terminate_reason, "GOAL");
        assert_eq!(out.result, "the findings");
    }

    /// A fake real [`Questioner`] that answers with fixed picks. Proves
    /// `Capabilities` holds a real answer behind `Arc<dyn Questioner>` without
    /// wiring an Agent mpsc.
    struct FakeQuestioner {
        answers: Vec<(usize, String)>,
    }

    #[async_trait::async_trait]
    impl Questioner for FakeQuestioner {
        async fn ask(
            &self,
            _questions: Vec<Question>,
        ) -> Result<Vec<(usize, String)>, String> {
            Ok(self.answers.clone())
        }
    }

    fn a_question() -> Question {
        Question {
            question: "Which library should we use?".into(),
            header: "Library".into(),
            options: vec![
                QuestionOption {
                    label: "serde".into(),
                    description: "the de-facto standard".into(),
                },
                QuestionOption {
                    label: "miniserde".into(),
                    description: "smaller, fewer features".into(),
                },
            ],
            multi_select: false,
        }
    }

    #[tokio::test]
    async fn declining_questioner_returns_the_verbatim_non_interactive_string() {
        let q = DecliningQuestioner;
        let err = q.ask(vec![a_question()]).await.unwrap_err();
        assert_eq!(
            err,
            "Cannot ask user questions in non-interactive mode without ACP support. \
             Please run in interactive mode or enable ACP mode to use this tool."
        );
    }

    #[tokio::test]
    async fn a_real_questioner_returns_its_injected_answers() {
        let caps = Capabilities::for_test_with_questioner(Arc::new(FakeQuestioner {
            answers: vec![(0, "serde".into())],
        }));
        // The seam is proven with a live wire: the picks travel back through the
        // `Arc<dyn Questioner>` the carrier holds.
        let out = caps.questioner.ask(vec![a_question()]).await.unwrap();
        assert_eq!(out, vec![(0, "serde".to_string())]);
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
