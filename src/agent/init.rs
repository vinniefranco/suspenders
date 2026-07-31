//! The async second half of Agent construction (F8, ADR-0056).
//!
//! [`AgentHandle::start`](super::AgentHandle::start) resolves the Session's fixed
//! facts synchronously, then hands the raw pieces here as an [`AgentInit`]:
//! [`init_agent`] finishes assembly - attach the MCP servers, discover disk
//! skills, build the Session-stable tool set (built-ins + discovered MCP tools),
//! compose the system prompt's Deferred Tools + managed-memory sections, and
//! build the Conversation whose overhead and Deferred Tools section are sourced
//! from a LIVE per-session registry over that set. All of it must see the MCP
//! tools (the connect awaits), so none of it can run in the sync `start`; it
//! lives here beside the actor loop rather than inside it, one focused seam for
//! everything Agent construction does after the facts are fixed.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentState, Msg, ResumeInfo};
use crate::approvals::Approvals;
use crate::compaction::Compaction;
use crate::conversation::{Conversation, ConversationOpts};
use crate::event::Event;
use crate::llm::Llm;
use crate::llm::model::Model;
use crate::run::settlement::Settlement;
use crate::session::Session;
use crate::session::log::Log;
use crate::tools;

/// The raw Session pieces `start` resolves synchronously and hands to the async
/// [`init_agent`], which finishes assembly (MCP connect, the tool set, the
/// tool-spec overhead, the Deferred Tools system-prompt section, the
/// Conversation) - all of which must see the MCP tools, so they cannot run in
/// the sync `start` (the connect awaits).
pub(super) struct AgentInit {
    pub(super) session: Session,
    pub(super) model: Model,
    pub(super) llm: Arc<dyn Llm>,
    pub(super) system_prompt: String,
    pub(super) resumed_messages: Vec<crate::content::Message>,
    pub(super) log: Option<Log>,
    pub(super) resume_info: Option<ResumeInfo>,
    pub(super) plan: Option<String>,
    pub(super) events: broadcast::Sender<Event>,
    pub(super) self_tx: mpsc::UnboundedSender<Msg>,
}

/// The async second half of Agent construction (F8, ADR-0056): attach the MCP
/// servers, assemble the Session-stable tool set (built-ins + discovered MCP
/// tools), and build the Conversation whose tool-spec overhead and Deferred
/// Tools section are sourced from a LIVE per-session registry over that set - so
/// both now count the MCP tools. Fail-open: a broken server is recorded on the
/// manager and skipped, never fatal.
pub(super) async fn init_agent(init: AgentInit) -> AgentState {
    let AgentInit {
        session,
        model,
        llm,
        system_prompt,
        resumed_messages,
        log,
        resume_info,
        plan,
        events,
        self_tx,
    } = init;

    // Attach the MCP servers once (fail-open). The discovered tools join the
    // built-ins to form the Session-stable set every Run shares.
    let (mcp, adapters) = crate::mcp::manager::McpManager::connect(&session.mcp_servers).await;

    // Surface each fail-open connect skip as a launch notice (ADR-0007's
    // fail-open report seam, the same line an Extension crash takes): a broken
    // MCP server is a visible skip, not a silent one. Server-name-sorted, so the
    // notices are stable across runs. This is the only production reader of
    // `mcp.failures()`.
    for (server, reason) in mcp.failures() {
        let _ = events.send(Event::extension_error(
            format!("mcp server {server}"),
            crate::event::Stage::PreRun,
            format!("could not connect - {reason}"),
        ));
    }

    // Discover disk skills once (fail-open, ADR-0058), the same shape as the MCP
    // attach above: `<root>/.suspenders/skills/<name>/SKILL.md` (project) +
    // `~/.suspenders/skills/<name>/SKILL.md` (user). A malformed/invalid manifest
    // is recorded on the manager and skipped, surfaced below as a launch notice.
    // The one `skill` tool holds the manager and embeds its `<available_skills>`
    // catalog in its description, so the tool is ALWAYS registered (even with an
    // empty catalog) - the catalog is how the model learns skills can exist.
    let project_skills = std::path::Path::new(&session.root)
        .join(".suspenders")
        .join("skills");
    let user_skills = crate::session::default_user_skills_dir();
    let skill_manager = Arc::new(crate::skills::SkillManager::discover(
        &project_skills,
        Some(std::path::Path::new(&user_skills)),
    ));

    // Surface each fail-open skill parse skip as a launch notice, exactly like an
    // MCP connect failure (ADR-0007's fail-open report seam): a broken SKILL.md is
    // a visible skip, not a silent one.
    for (name, reason) in skill_manager.failures() {
        let _ = events.send(Event::extension_error(
            format!("skill {name}"),
            crate::event::Stage::PreRun,
            reason.clone(),
        ));
    }

    // The subagent registry (P4/F4, ADR-0061): the built-in defs, built once
    // here. The `agent` tool holds it (for its dynamic schema/description, the
    // way the `skill` tool holds a SkillManager) AND each Run's Capture threads
    // it into the DirectSubagentSpawner.
    let subagents = Arc::new(crate::subagents::SubagentRegistry::new(
        crate::subagents::builtins(),
    ));

    let mut all = tools::tools();
    all.extend(adapters);
    all.push(Box::new(crate::tools::skill::SkillTool::new(Arc::clone(
        &skill_manager,
    ))));
    all.push(Box::new(crate::tools::agent::AgentTool::new(Arc::clone(
        &subagents,
    ))));
    let session_tools: Arc<[Box<dyn crate::tool::Tool>]> = all.into();

    // A live per-session registry over the Session-stable set, used ONLY to
    // source the overhead + Deferred Tools section here at launch (the Runs
    // build their own `with_shared` registries). It includes the MCP tools, so
    // both figures now count them.
    let session_registry =
        crate::tool_registry::ToolRegistry::with_shared(Arc::clone(&session_tools));

    // The tool specs ride with every request but live outside the messages; the
    // estimate has to count them or Eviction fires late (baud's
    // `String.length(JSON.encode!(Baud.Tools.specs()))`). Sourced from the live
    // registry's `specs()` - the BASE (non-revealed) wire list. MCP tools are
    // all deferred, so they are excluded here: overhead is unchanged and
    // correct, exactly as if no MCP server were attached (F8, ADR-0054).
    let overhead = serde_json::to_string(&session_registry.specs())
        .map(|s| s.chars().count() as u64)
        .unwrap_or(0);

    // Append the Deferred Tools section to the system prompt (F3, F8): the model
    // needs to know which tools exist off the wire list so it can reach them via
    // `tool_search`. Sourced from the live registry's `deferred_summary()`,
    // which now INCLUDES the discovered `mcp__*` tools (ADR-0054, ADR-0056). The
    // summary is static across reveals (a reveal moves a name onto the wire
    // list, it does not change what is deferred), so it is computed once here.
    // F5 SEAM (ADR-0054 revision, ADR-0062): this is where prompt sections
    // compose - the Deferred Tools section here, then the managed-auto-memory
    // suffix just below, each `\n\n---\n\n`-joined onto the system prompt.
    let deferred = session_registry.deferred_summary();
    let system_prompt = format!(
        "{system_prompt}{}",
        crate::context_files::deferred_tools_section(&deferred)
    );

    // Append the managed-auto-memory suffix (P5, F5, ADR-0062): the same
    // `\n\n---\n\n`-joined composition point as the Deferred Tools section, now
    // that F5 owns prompt-section composition. The index is loaded ONCE here at
    // Session start and never refreshed mid-Session - faithful to qwen: the
    // model's own writes during this Session land in the files but only re-enter
    // the prompt on the NEXT Session's load. The memory dir is scaffolded
    // (mkdir) fail-open, exactly like the MCP/skill attach: a scaffold failure
    // is a visible launch notice, never fatal.
    // Load from the Session's ALREADY-resolved memory root (P5, ADR-0062): the
    // SAME fact the ToolCtx confinement carries, so the dir the model is told to
    // write to is exactly the dir confinement permits - one resolution, no drift.
    let memory = crate::memory::MemoryStore::load_at(std::path::Path::new(&session.memory_root));
    if let Err(reason) = memory.ensure_scaffold() {
        let _ = events.send(Event::extension_error(
            "memory".to_string(),
            crate::event::Stage::PreRun,
            format!(
                "could not scaffold memory dir {} - {reason}",
                memory.memory_dir
            ),
        ));
    }
    let system_prompt = format!("{system_prompt}{}", memory.prompt_suffix());

    // The budget figures derive from the launch Model here and are re-derived
    // from the captured Model at every Run start (ADR-0037, `reset_run_state`).
    let mut conversation = Conversation::new(
        system_prompt,
        ConversationOpts::new(
            session.context_budget_for(&model),
            session.reply_reserve_for(&model),
        )
        .overhead_chars(overhead)
        .compaction_slack(session.compaction_slack)
        .compaction_keep(session.compaction_keep),
    );
    // A Resume seeds the messages verbatim ahead of the (empty) fresh ones.
    let mut seeded = resumed_messages.clone();
    seeded.extend(std::mem::take(&mut conversation.messages));
    conversation.messages = seeded;

    AgentState {
        session,
        run_provenance: model.provenance(),
        model,
        llm,
        conversation,
        log,
        resume_info,
        plan,
        events,
        task: None,
        cancel_flag: false,
        settlement: Settlement::new(),
        approvals: Approvals::new(),
        approval_replies: HashMap::new(),
        question_replies: HashMap::new(),
        steering: Vec::new(),
        compaction: Compaction::new(),
        self_tx,
        mcp,
        session_tools,
        subagents,
        background: HashMap::new(),
        notifications: Vec::new(),
        background_counter: 0,
        background_shells: HashMap::new(),
        background_shell_counter: 0,
    }
}
