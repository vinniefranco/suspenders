//! Run - one agent iteration (prompt to settlement).
//!
//! [`run`] declares the Run's [`deps::RunDeps`] port (what a Run needs from its
//! host) and drives [`loop_::run`] over it. The host supplies both the concrete
//! [`deps::RunDeps`] adapter and the [`Capture`] (the Model + Llm snapshot the
//! Run took at spawn) it builds its tooling from - so the Run never depends on
//! any one host. The Agent's adapter is [`crate::agent::deps::AgentDeps`].

mod batch;
pub mod child;
pub mod deps;
mod finish;
pub mod loop_;
pub mod next_speaker;
pub mod settlement;
pub mod side_query;
pub mod subagent;

// Shared test fixtures for the split Loop (today only `loop_`'s tests; any
// test module `batch`/`finish` grow shares this set instead of drifting
// copies). Private suffices: descendants reach a private ancestor module.
#[cfg(test)]
mod fixtures;

use std::sync::Arc;

use crate::content::{ContentBlock, Message, Role};
use crate::conversation::{Conversation, ConversationOpts};
use crate::extensions;
use crate::llm::Llm;
use crate::llm::ToolCallStyle;
use crate::llm::model::Model;
use crate::run::child::{ChildDeps, ChildSink};
use crate::run::deps::RunDeps;
use crate::run::loop_::{Outcome, OutcomeStop, RunEnv, RunOpts};
use crate::session::Session;
use crate::session::log::StopReason;
use crate::tool::Tool;
use crate::tool::caps::{
    Capabilities, DecliningQuestioner, DenyingApprover, SubagentResult,
    UnavailableBackgroundShellSpawner, UnavailableSubagentSpawner,
};

/// The Model + Llm a Run captured at spawn (ADR-0033): the Model snapshot every
/// request travels to, and the Llm boundary the Run's tooling dispatches over.
/// The host builds this once and hands it to [`run`] alongside the
/// [`deps::RunDeps`] adapter, so the Run reads its tooling inputs from a plain
/// value rather than reaching into a particular host's deps.
pub struct Capture {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
    /// The tool-initiated Approval seam (F1, ADR-0055), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentApprover`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the registry it builds itself. `Arc<dyn Approver>` is Send+Sync,
    /// so the [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub approver: Arc<dyn crate::tool::caps::Approver>,
    /// The tool-initiated Question seam (P2a, ADR-0057), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentQuestioner`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the Approver. `Arc<dyn Questioner>` is Send+Sync, so the
    /// [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub questioner: Arc<dyn crate::tool::caps::Questioner>,
    /// The Session-stable tool set the Agent built once (F8, ADR-0056):
    /// built-ins plus any discovered MCP tools. The Run builds its per-Run
    /// [`crate::tool_registry::ToolRegistry`] over this shared `Arc` (fresh
    /// revealed set per Run), so MCP tools ride every Run without re-boxing.
    pub tools: Arc<[Box<dyn crate::tool::Tool>]>,
    /// The Subagent effect seam (P4/F4, ADR-0061), built by the host that owns
    /// the subagent machinery (the Agent builds a
    /// [`crate::run::subagent::DirectSubagentSpawner`] over its Llm + registry +
    /// providers). The Run assembles it into the Tool
    /// [`crate::tool::caps::Capabilities`] alongside the Approver/Questioner.
    /// `Arc<dyn SubagentSpawner>` is Send+Sync, so the [`Capture`] stays `Send`
    /// for the `tokio::spawn` at the Agent. A child Run (spawned by `run_child`)
    /// carries an [`crate::tool::caps::UnavailableSubagentSpawner`] here instead
    /// - the recursion guard.
    pub subagents: Arc<dyn crate::tool::caps::SubagentSpawner>,
    /// The Background-Shell effect seam (Phase 9, ADR-0063), built by the host
    /// that owns the process lifecycle (the Agent builds a tx-backed
    /// `AgentBackgroundShellSpawner`). The Run assembles it into the Tool
    /// [`crate::tool::caps::Capabilities`] alongside the subagents spawner. A child
    /// Run (spawned by `run_child`) carries an
    /// [`crate::tool::caps::UnavailableBackgroundShellSpawner`] here instead - the
    /// recursion guard (a subagent cannot background a shell).
    pub bg_shells: Arc<dyn crate::tool::caps::BackgroundShellSpawner>,
}

/// Runs the Run: builds the Extension pipeline and Tool ctx and drives
/// [`loop_::run`]. Returns the Loop outcome.
pub async fn run(
    conversation: Conversation,
    session: Session,
    capture: Capture,
    mut deps: impl RunDeps,
    opts: RunOpts,
) -> Outcome {
    // Resolve the Session's ordered Extension names into the live pipeline. The
    // shipped config carries `["diff"]`, so the live app runs the Run with the
    // Diff extension; the test config carries `[]`.
    let extensions = extensions::configured(&session.extensions);

    // The Tool Registry, built once per Run (F3, F8). Reveals are Run-scoped: a
    // fresh registry per Run resets them, matching qwen's
    // clearRevealedDeferredTools on session reset. It shares the Agent's
    // Session-stable tool set (built-ins + discovered MCP tools, ADR-0056) via
    // `with_shared`, so MCP tools ride every Run. It rides the Tool ctx so
    // `tool_search` can reveal deferred tools into the next request's wire list.
    let registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::with_shared(
        Arc::clone(&capture.tools),
    ));

    // The file-read cache, built once per Run (F6, ADR-0060): fresh and empty at
    // Run start, like the registry, so a file read in a prior Run does not clear
    // this Run's read-before-edit enforcement. read_file records into it;
    // notebook_edit checks it before mutating a notebook.
    let read_cache = Arc::new(crate::tool::read_cache::FileReadCache::new());

    // The Side-Query effect seam (P2b, ADR-0055): the real impl is the captured
    // Llm boundary called OFF the main Conversation. Unlike the Approver, it does
    // NOT travel the Agent mpsc - a side-query touches no Agent/Conversation state
    // - so the Run builds it here from the Capture's own `llm`/`model` (no new
    // Capture field). `None` temperature defers a side-query to the server's own
    // default, matching qwen's side-query (which sets no temperature).
    let side_query: Arc<dyn crate::tool::caps::SideQuery> =
        Arc::new(crate::run::side_query::LlmSideQuery {
            llm: Arc::clone(&capture.llm),
            model: capture.model.clone(),
            temperature: None,
        });

    // The Tool Capability Context (F1, ADR-0055): the registry the Run built plus
    // the effect seams a Tool Call reaches its host through. P1b carries the
    // registry (concrete, Run-scoped state) and the Approver (the host's
    // tx-backed handle, threaded through the Capture); P2b adds the SideQuery
    // (built here at the Llm boundary). The other two capabilities land in their
    // consuming phases.
    let caps = crate::tool::caps::Capabilities {
        registry,
        read_cache,
        approver: Arc::clone(&capture.approver),
        side_query,
        // The Question seam (P2a, ADR-0057): the Agent's tx-backed handle,
        // threaded through the Capture like the Approver.
        questioner: Arc::clone(&capture.questioner),
        // The Subagent seam (P4/F4, ADR-0061): the Agent's DirectSubagentSpawner,
        // threaded through the Capture like the Approver/Questioner. A child Run
        // carries an UnavailableSubagentSpawner here instead - the recursion
        // guard.
        subagents: Arc::clone(&capture.subagents),
        // The Background-Shell seam (Phase 9, ADR-0063): the Agent's tx-backed
        // handle, threaded through the Capture like the subagents spawner. A child
        // Run carries an UnavailableBackgroundShellSpawner here instead - the
        // recursion guard.
        bg_shells: Arc::clone(&capture.bg_shells),
    };

    // The Tool ctx: the Session's Root and timeout, the Result Cap derived from
    // this Run's captured Model (ADR-0037), and the Run's Capabilities.
    let tool_ctx = session.tool_ctx(&capture.model, caps);

    loop_::run(
        conversation,
        &session,
        loop_::RunEnv {
            extensions: &extensions,
            tool_ctx: &tool_ctx,
        },
        &mut deps,
        opts,
    )
    .await
}

/// The request that drives one child Run to settlement (P4/F4, ADR-0061). A
/// self-contained bundle: the child's Model, the shared [`Llm`] boundary, the
/// child's verbatim system prompt + first-user `prompt`, its tool subset, its
/// turn bound (`max_turns`), the Session's request settings, and the parent
/// [`Session`] the child derives its Root/timeout/budget knobs from. A foreground
/// subagent passes `sink: None` (its whole run is invisible until it settles);
/// the background path (DEFERRED) would pass a `sink`.
///
/// `session` is the parent's Session cloned - a child Run is the parent's Run
/// over a FRESH Conversation with a narrowed tool set and its own turn bound, so
/// it reuses the parent's Root, command timeout, budget knobs, and Provider set.
/// `run_child` overrides only the turn bound (`run_limit = max_turns`) and the
/// captured Model. `depth` rides for defence: at `depth >= 1` the child's own
/// subagents capability is already degraded (the recursion guard), so `depth` is
/// a belt-and-braces record, not the primary guard.
pub struct ChildRunRequest {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
    pub system_prompt: String,
    pub tools: Vec<Box<dyn Tool>>,
    pub prompt: String,
    pub max_turns: usize,
    pub temperature: Option<f64>,
    pub thinking_budget: Option<u64>,
    pub tool_call_style: ToolCallStyle,
    pub session: Session,
    pub sink: Option<ChildSink>,
    pub depth: usize,
}

/// Drives one child Run to settlement and maps its [`Outcome`] to a
/// [`SubagentResult`] (P4/F4, ADR-0061). The re-entrant, self-contained Run
/// driver: it needs no Agent actor - it assembles a fresh child [`Conversation`]
/// (the system prompt plus the task as the first user message), a child
/// [`crate::tool_registry::ToolRegistry`] over the request's tool subset, a fresh
/// [`crate::tool::read_cache::FileReadCache`], child [`Capabilities`] (a denying
/// Approver, a declining Questioner, an [`crate::run::side_query::LlmSideQuery`]
/// over the child's own Llm/Model so web_fetch still works, and an
/// [`UnavailableSubagentSpawner`] - the RECURSION GUARD), a child
/// [`crate::tool::ToolCtx`], a [`ChildDeps`], and drives [`loop_::run`] with the
/// child turn bound. Every child effect that is not `complete` is a no-op or the
/// safe answer, so the child's run touches no Agent/Conversation state (ADR-0061).
pub async fn run_child(req: ChildRunRequest) -> SubagentResult {
    // Derive the child Session from the parent's: same Root, command timeout,
    // budget knobs, and Provider set, but the child's own turn bound and Model.
    let mut session = req.session;
    session.run_limit = req.max_turns as u64;
    session.model = req.model.clone();

    // The child extension pipeline: a child Run runs extension-free (the parent
    // owns the Diff extension's presenter seam; a subagent's edits are its own).
    let extensions = extensions::configured(&[]);

    // The child Tool Registry over the request's narrowed tool subset (built-ins
    // minus the excluded set, per the def's selector). `with_shared` wants an
    // `Arc<[..]>`, so the boxed tool Vec converts into one.
    let tools: Arc<[Box<dyn Tool>]> = req.tools.into();
    let registry = Arc::new(crate::tool_registry::ToolRegistry::with_shared(Arc::clone(
        &tools,
    )));

    let read_cache = Arc::new(crate::tool::read_cache::FileReadCache::new());

    // The child SideQuery: the child's own Llm/Model boundary, so web_fetch's
    // prompt-guided extraction still works inside a subagent.
    let side_query: Arc<dyn crate::tool::caps::SideQuery> =
        Arc::new(crate::run::side_query::LlmSideQuery {
            llm: Arc::clone(&req.llm),
            model: req.model.clone(),
            temperature: None,
        });

    // The child Capabilities: a denying Approver + declining Questioner (a
    // foreground subagent has no interactive channel), the child SideQuery, and
    // the UnavailableSubagentSpawner - the recursion guard that makes a nested
    // `agent` call fail rather than spawn a grandchild.
    let caps = Capabilities {
        registry,
        read_cache,
        approver: Arc::new(DenyingApprover),
        side_query,
        questioner: Arc::new(DecliningQuestioner),
        subagents: Arc::new(UnavailableSubagentSpawner),
        // The recursion guard: a subagent's own background-shell capability is
        // degraded, so a subagent cannot background a shell (ADR-0063).
        bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
    };

    let tool_ctx = session.tool_ctx(&req.model, caps);

    // The fresh child Conversation: the def's verbatim system prompt, seeded with
    // the task as the first user message. Budget figures derive from the child
    // Model like any Run (ADR-0037).
    let mut conversation = Conversation::new(
        req.system_prompt,
        ConversationOpts::new(
            session.context_budget_for(&req.model),
            session.reply_reserve_for(&req.model),
        )
        .compaction_slack(session.compaction_slack)
        .compaction_keep(session.compaction_keep),
    );
    conversation.add_user_text(req.prompt);

    let mut deps = ChildDeps {
        llm: Arc::clone(&req.llm),
        model: req.model.clone(),
        temperature: req.temperature,
        thinking_budget: req.thinking_budget,
        tool_call_style: req.tool_call_style,
        sink: req.sink,
    };

    let outcome = loop_::run(
        conversation,
        &session,
        RunEnv {
            extensions: &extensions,
            tool_ctx: &tool_ctx,
        },
        &mut deps,
        RunOpts::default(),
    )
    .await;

    // `depth` is defence in depth alongside the degraded subagents capability;
    // it is recorded on the request and read here so the field is live.
    let _ = req.depth;

    outcome_to_result(outcome)
}

/// Maps a child Run's [`Outcome`] to a [`SubagentResult`] (qwen's
/// `AgentTerminateMode`, ADR-0061). `EndTurn`/`MaxTokens` -> `"GOAL"` with the
/// last assistant text; `RunLimit` -> `"MAX_TURNS"`; every other stop (a stuck
/// loop, a custom after-Pass stop, a failed Run, an exhausted budget) -> `"ERROR"`.
/// The `result` is the child's last assistant text with any trailing Voice close
/// marker stripped, so the parent never reads Suspenders' internal marker as the
/// subagent's answer. TIMEOUT/CANCELLED are DEFERRED with the background path.
fn outcome_to_result(outcome: Outcome) -> SubagentResult {
    match outcome {
        Outcome::Ok(conversation, stop) => {
            let result = last_assistant_text(&conversation);
            let terminate_reason = match stop {
                OutcomeStop::Reason(StopReason::EndTurn)
                | OutcomeStop::Reason(StopReason::MaxTokens)
                | OutcomeStop::Reason(StopReason::ToolUse)
                | OutcomeStop::Reason(StopReason::StopSequence) => "GOAL",
                OutcomeStop::Reason(StopReason::RunLimit) => "MAX_TURNS",
                // A stuck loop, a failed/unknown stop, or any after-Pass custom
                // Stop all read as ERROR (qwen collapses these to a non-GOAL
                // terminate mode the parent treats as a failure).
                OutcomeStop::Reason(_) | OutcomeStop::Custom(_) => "ERROR",
            };
            SubagentResult {
                terminate_reason: terminate_reason.to_string(),
                result,
            }
        }
        // The LLM error algebra closed the Run with the partial text; surface it
        // as the ERROR result with that partial text (qwen returns finalText on a
        // non-GOAL terminate).
        Outcome::Failed(_reason, conversation) => SubagentResult {
            terminate_reason: "ERROR".to_string(),
            result: last_assistant_text(&conversation),
        },
        // Eviction + Compaction could not fit the request: no text was produced.
        Outcome::Error => SubagentResult {
            terminate_reason: "ERROR".to_string(),
            result: String::new(),
        },
    }
}

/// The child's answer: the text blocks of the LAST assistant message, joined and
/// trimmed, with a trailing pure-Voice close marker dropped. `finish`/`close`
/// append a marker-only assistant message when the Loop closes a Run itself (the
/// Run-Limit/stall/stopped markers) or a `[turn failed]` marker on the error
/// path; that marker is Suspenders' internal signal, not the subagent's answer,
/// so a last assistant message that is ONLY a close marker is skipped in favour
/// of the model's real prior text.
fn last_assistant_text(conversation: &Conversation) -> String {
    let assistant_texts = |message: &Message| -> Option<String> {
        if message.role != Role::Assistant {
            return None;
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    // Walk assistant messages newest-first, skipping any whose only text is a
    // pure Voice close marker (a self-authored `[...]` marker with no model text).
    conversation
        .messages
        .iter()
        .rev()
        .filter_map(assistant_texts)
        .find(|text| !is_pure_close_marker(text))
        .unwrap_or_default()
}

/// Whether `text` is exactly one of the Voice close markers the Loop appends as
/// a marker-only assistant message (ADR-0061). Compared against the markers a
/// `finish`/`close`/`fail` can author so the parent never sees them as the
/// subagent's answer.
fn is_pure_close_marker(text: &str) -> bool {
    use crate::voice;
    [
        voice::run_limit_marker(),
        voice::loop_stall_marker(),
        voice::run_stopped_marker(),
        voice::run_failed_marker(),
        voice::run_cancelled_marker(),
        voice::truncation_marker(),
        voice::empty_response_marker(),
    ]
    .contains(&text)
}

#[cfg(test)]
mod child_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::content::ContentBlock;
    use crate::llm::LlmRequest;
    use crate::llm::model::Api;
    use crate::llm::response::{Response, StopReason as RespStop};
    use crate::session::{SessionConfig, SessionOpts};
    use crate::subagents::{ToolSelector, subagent_tools};
    use crate::test_support::{Entry, FakeLlm};
    // `super::*` already brings the `log::StopReason` the outcome mapping uses.

    fn session() -> Session {
        let opts = SessionOpts {
            root: Some(std::env::temp_dir().to_string_lossy().to_string()),
            ..SessionOpts::default()
        };
        Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
    }

    fn model() -> Model {
        Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
    }

    fn text_response(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: RespStop::EndTurn,
            usage: Default::default(),
            error: None,
        }
    }

    fn tool_use_response(id: &str, name: &str, input: serde_json::Value) -> Response {
        Response {
            content: vec![ContentBlock::tool_use(id, name, input)],
            stop_reason: RespStop::ToolUse,
            usage: Default::default(),
            error: None,
        }
    }

    fn request(llm: Arc<dyn Llm>, sink: Option<ChildSink>) -> ChildRunRequest {
        ChildRunRequest {
            model: model(),
            llm,
            system_prompt: "You are a subagent.".to_string(),
            tools: subagent_tools(&ToolSelector::All),
            prompt: "do the task".to_string(),
            max_turns: 5,
            temperature: None,
            thinking_budget: None,
            tool_call_style: ToolCallStyle::default(),
            session: session(),
            sink,
            depth: 1,
        }
    }

    #[tokio::test]
    async fn a_final_text_pass_settles_as_goal_with_that_text() {
        let fake = FakeLlm::script(vec![Entry::just(text_response("the answer"))]);
        let out = run_child(request(Arc::new(fake), None)).await;
        assert_eq!(out.terminate_reason, "GOAL");
        assert_eq!(out.result, "the answer");
    }

    #[tokio::test]
    async fn a_tool_use_pass_then_final_text_settles_as_goal() {
        // One tool_use (todo_write) then a final text: the child loops once,
        // answers the call, then finishes on the text.
        let fake = FakeLlm::script(vec![
            Entry::just(tool_use_response(
                "call-1",
                "todo_write",
                serde_json::json!({"todos": [{"content": "step", "status": "pending"}]}),
            )),
            Entry::just(text_response("done exploring")),
        ]);
        let out = run_child(request(Arc::new(fake), None)).await;
        assert_eq!(out.terminate_reason, "GOAL");
        assert_eq!(out.result, "done exploring");
    }

    #[tokio::test]
    async fn a_child_never_sees_the_agent_tool_the_recursion_guard() {
        // Guard 1 (the tool subset): `subagent_tools(All)` drops `agent`, so the
        // child's wire tool list never carries the delegation tool - the model
        // cannot even name it. Capture the first Pass's wire request and assert.
        let saw_agent = Arc::new(Mutex::new(None::<bool>));
        let saw = Arc::clone(&saw_agent);
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &LlmRequest, _m| {
            if saw.lock().unwrap().is_none() {
                let present = req.tools.iter().any(|t| t.name == "agent");
                *saw.lock().unwrap() = Some(present);
            }
            text_response("done")
        })]);
        let out = run_child(request(Arc::new(fake), None)).await;
        assert_eq!(out.terminate_reason, "GOAL");
        assert!(
            !saw_agent.lock().unwrap().expect("the child made a request"),
            "the child's wire list must not carry the `agent` tool"
        );
    }

    #[tokio::test]
    async fn a_child_agent_call_folds_the_unavailable_err_the_recursion_guard() {
        // Guard 2 (defence in depth): even if the child's tool subset DID carry
        // an `agent` tool (bypass the exclusion by adding it explicitly), its
        // `spawn` reaches the child's `UnavailableSubagentSpawner`, so a nested
        // `agent` call fails rather than spawning a grandchild. The child emits an
        // `agent` tool_use on Pass 1; the (error) tool result rides into Pass 2's
        // wire messages, where we assert the verbatim Unavailable string.
        use crate::content::ContentBlock as CB;
        use crate::subagents::builtins;
        use crate::tools::agent::AgentTool;

        let seen_result = Arc::new(Mutex::new(None::<String>));
        let seen = Arc::clone(&seen_result);
        let fake = FakeLlm::script(vec![
            // Pass 1: the child model emits a nested `agent` tool_use.
            Entry::just(tool_use_response(
                "call-1",
                "agent",
                serde_json::json!({
                    "description": "nested spawn",
                    "prompt": "recurse",
                    "subagent_type": "general-purpose",
                }),
            )),
            // Pass 2: scan the wire messages for the nested call's tool result.
            Entry::dynamic(vec![], move |req: &LlmRequest, _m| {
                for msg in &req.messages {
                    for block in &msg.content {
                        if let CB::ToolResult { content, .. } = block {
                            let text = crate::content::result_blocks_text(content.as_slice());
                            if !text.is_empty() {
                                *seen.lock().unwrap() = Some(text);
                            }
                        }
                    }
                }
                text_response("gave up")
            }),
        ]);

        // Build the child tools = the All subset PLUS an AgentTool bolted on, so
        // the exclusion is deliberately bypassed to exercise the second guard.
        let mut tools = subagent_tools(&ToolSelector::All);
        tools.push(Box::new(AgentTool::new(Arc::new(
            crate::subagents::SubagentRegistry::new(builtins()),
        ))));
        let mut req = request(Arc::new(fake), None);
        req.tools = tools;

        let out = run_child(req).await;
        assert_eq!(out.result, "gave up");
        let result_text = seen_result
            .lock()
            .unwrap()
            .clone()
            .expect("the child's `agent` call produced a tool result");
        assert!(
            result_text.contains("subagents are unavailable in this environment"),
            "a nested `agent` call must fold the UnavailableSubagentSpawner Err, got: {result_text}"
        );
    }

    #[tokio::test]
    async fn a_child_emits_nothing_to_a_no_op_sink_and_can_feed_a_supplied_one() {
        // Foreground path (sink None): the child's whole run is invisible - no
        // parent channel is touched. A supplied sink DOES receive the child's
        // events (the background path's seam), proving the emitter routing.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink: ChildSink = Box::new(move |event| sink_seen.lock().unwrap().push(event));
        let fake = FakeLlm::script(vec![Entry::just(text_response("hi"))]);
        let out = run_child(request(Arc::new(fake), Some(sink))).await;
        assert_eq!(out.terminate_reason, "GOAL");
        // The child produced at least a message grammar into the supplied sink.
        assert!(!seen.lock().unwrap().is_empty());
    }

    #[test]
    fn outcome_run_limit_maps_to_max_turns() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
        conv.add_assistant_response(
            vec![ContentBlock::text("real work so far")],
            model().provenance(),
        );
        // The Loop appends a marker-only assistant message on a Run-Limit close.
        conv.add_assistant_blocks(vec![ContentBlock::text(crate::voice::run_limit_marker())]);
        let out = outcome_to_result(Outcome::Ok(conv, OutcomeStop::Reason(StopReason::RunLimit)));
        assert_eq!(out.terminate_reason, "MAX_TURNS");
        // The trailing pure-marker message is skipped; the real text is the answer.
        assert_eq!(out.result, "real work so far");
    }

    #[test]
    fn outcome_failed_maps_to_error_with_partial_text() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
        conv.add_assistant_response(vec![ContentBlock::text("partial")], model().provenance());
        conv.add_assistant_blocks(vec![ContentBlock::text(crate::voice::run_failed_marker())]);
        let out = outcome_to_result(Outcome::Failed("boom".to_string(), conv));
        assert_eq!(out.terminate_reason, "ERROR");
        assert_eq!(out.result, "partial");
    }

    #[test]
    fn outcome_budget_error_maps_to_error_with_no_text() {
        let out = outcome_to_result(Outcome::Error);
        assert_eq!(out.terminate_reason, "ERROR");
        assert_eq!(out.result, "");
    }

    #[test]
    fn outcome_stuck_loop_maps_to_error() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
        conv.add_assistant_blocks(vec![ContentBlock::text(crate::voice::loop_stall_marker())]);
        let out = outcome_to_result(Outcome::Ok(
            conv,
            OutcomeStop::Reason(StopReason::RunLimitStuck),
        ));
        assert_eq!(out.terminate_reason, "ERROR");
    }
}
