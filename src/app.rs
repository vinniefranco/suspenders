//! Composition root / bootstrap.
//!
//! Wires the Session, the Agent, and a frontend, then runs it. Two entry
//! points, one per ADR-0019 shape:
//!
//! * [`run_tui`] - the interactive ratatui frontend (ADR-0001), owning the
//!   terminal for the session.
//! * [`run_headless`] - the stdout event-subscriber runner (ADR-0019): submit
//!   each prompt as a sequential Run in ONE session, stream every event to
//!   stdout, auto-approve run_command Approvals, and report the token estimate
//!   and message count on settlement.

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{AgentHandle, StartOpts};
use crate::approvals::Decision;
use crate::event::Event;
use crate::llm::Dispatcher;
use crate::session::{Session, SessionOpts};
use crate::ui::picker::PickerOutcome;
use crate::ui::theme::ActiveTheme;

#[cfg(test)]
#[path = "../tests/app.rs"]
mod tests;

/// Launches the interactive ratatui frontend (ADR-0001, ADR-0019). Builds the
/// Session from config/env (the resolved Provider set + launch Model behind a
/// real `Dispatcher`), starts the Agent, and hands the terminal to
/// [`crate::ui::run`] (which enters/leaves raw mode + the alternate screen
/// around itself).
/// Bare `--resume` (no value) arrives as this sentinel and opens the picker.
const PICK: &str = "pick";

pub async fn run_tui(root: Option<PathBuf>, resume: Option<String>) -> anyhow::Result<()> {
    let session = build_session(root)?;
    // The configured Theme resolves ONCE at this edge (ADR-0038): the themes
    // dir is captured like the config path, and a missing or broken name
    // falls back to the built-in dark with a notice - never a launch block.
    // Resolved before the picker so it, too, draws in the user's theme.
    let themes_dir = PathBuf::from(crate::session::default_themes_dir());
    let (themes, theme_notice) = ActiveTheme::launch(&session.theme, themes_dir);
    // The picker needs the Session first: the logs live in its session_dir.
    let ResumeAction::Start(resume) =
        resolved_resume(resume, &session.session_dir, themes.active()).await?
    else {
        // Leave without starting the Agent.
        return Ok(());
    };
    start_and_run(session, resume, themes, theme_notice).await
}

// The context files load once at launch, fail-open: the assembled prompt
// feeds the Agent, and a present-but-unusable file surfaces as one launch
// info line (never silently, never blocking the Session) - the theme
// fallback notice rides the same seam into the Transcript.
async fn start_and_run(
    session: Session,
    resume: Option<String>,
    themes: ActiveTheme,
    theme_notice: Option<String>,
) -> anyhow::Result<()> {
    let context =
        crate::context_files::load(&session.root, crate::voice::InteractionMode::Interactive);
    let mut launch_notices: Vec<String> = context.skipped.iter().map(|s| s.info_line()).collect();
    launch_notices.extend(theme_notice);
    let (session, llm) = boundary_and_enriched_session(session).await;
    let agent = start_agent(session.clone(), llm, resume, context.system_prompt)?;
    crate::ui::run(agent, &session, launch_notices, themes).await
}

// Runs the picker for a bare `--resume` and resolves what launch does. Only
// the terminal-touching pick_session call lives here; what the outcome MEANS
// is resolve_resume's (pure, tested).
async fn resolved_resume(
    resume: Option<String>,
    session_dir: &str,
    theme: &crate::ui::theme::Theme,
) -> anyhow::Result<ResumeAction> {
    let outcome = if resume.as_deref() == Some(PICK) {
        let entries = crate::session::log::list(session_dir);
        if entries.is_empty() {
            // No sessions to pick from is silently a fresh start - we're
            // pre-alt-screen, and a note would just flash and vanish.
            None
        } else {
            Some(crate::ui::pick_session(entries, theme).await?)
        }
    } else {
        None
    };
    Ok(resolve_resume(resume, outcome))
}

/// What launch does once the `--resume` argument is resolved.
#[derive(Debug, PartialEq, Eq)]
enum ResumeAction {
    /// Start the Agent, resuming from this Session Log path when `Some`.
    Start(Option<String>),
    /// Leave without starting the Agent (the picker's q / Ctrl-C).
    Quit,
}

/// The bare-`--resume` picker decision, pure. `outcome` is `None` when no
/// picker ran: either a concrete resume value (or none) was passed, or bare
/// `--resume` found no sessions to pick from - both of those pass straight
/// through, the latter as a silent fresh start.
fn resolve_resume(resume: Option<String>, outcome: Option<PickerOutcome>) -> ResumeAction {
    if resume.as_deref() != Some(PICK) {
        return ResumeAction::Start(resume);
    }
    match outcome {
        None | Some(PickerOutcome::FreshSession) => ResumeAction::Start(None),
        Some(PickerOutcome::Resume(path)) => ResumeAction::Start(Some(path)),
        Some(PickerOutcome::Quit) => ResumeAction::Quit,
    }
}

/// The stdout event-subscriber runner (ports `scripts/drive.exs`, ADR-0019).
/// Starts the Agent, subscribes, and submits each prompt as a sequential Run
/// in the SAME session; every event streams to stdout, run_command Approvals are
/// auto-approved (a diagnostic harness, not a session for untrusted work), and
/// each settle prints the token estimate and message count. An empty `prompts`
/// defaults to a single "evaluate this project" Run (drive.exs's default).
pub async fn run_headless(
    root: Option<PathBuf>,
    resume: Option<String>,
    prompts: Vec<String>,
) -> anyhow::Result<()> {
    if resume.as_deref() == Some(PICK) {
        anyhow::bail!(r#"--resume without a value needs the TUI; pass a path or "latest""#);
    }

    let session = build_session(root)?;
    let root_label = session.root.clone();
    let system_prompt = headless_context(&session.root);
    let (session, llm) = boundary_and_enriched_session(session).await;
    let agent = start_agent(session, llm, resume, system_prompt)?;
    drive(&agent, &root_label, prompts, &mut |line| println!("{line}")).await
}

// Same load as the TUI; headless has no Transcript, so a skip prints as a
// plain line in the event stream instead.
fn headless_context(root: &str) -> String {
    let context = crate::context_files::load(root, crate::voice::InteractionMode::Headless);
    for skip in &context.skipped {
        println!("!! {}", skip.info_line());
    }
    context.system_prompt
}

/// The per-prompt drive loop: submit each prompt as a sequential Run and
/// drain events until it settles. An empty `prompts` defaults to a single
/// "evaluate this project" Run (drive.exs's default). Every printed line
/// goes through `out` - `run_headless` passes `println!`, tests capture the
/// lines - so the loop runs against an Agent over any Llm double.
async fn drive(
    agent: &AgentHandle,
    root_label: &str,
    prompts: Vec<String>,
    out: &mut impl FnMut(String),
) -> anyhow::Result<()> {
    let prompts = if prompts.is_empty() {
        vec!["evaluate this project".to_string()]
    } else {
        prompts
    };
    let mut events = agent.subscribe();

    for prompt in prompts {
        let started = std::time::Instant::now();
        out(format!("\n== submit (root={root_label}): {prompt}"));
        // A Run boundary race with a still-running previous Run is not
        // possible here: we drive Runs strictly sequentially, awaiting each
        // one's settlement before submitting the next.
        if let Err(_busy) = agent.submit(prompt).await {
            out("!! agent busy; skipping".to_string());
            continue;
        }

        // Drain events until this Run settles (ADR-0019: headless drives the
        // same Agent seam). A settle event normally leaves the Agent idle; the
        // status recheck is a defensive backstop - if the Agent still reports
        // Running, keep draining until it truly settles.
        loop {
            match events.recv().await {
                Ok(event) => {
                    let settled = is_settled(&event);
                    handle_event(agent, &event, started, &mut *out).await;
                    if settled {
                        print_estimate(agent, &mut *out).await;
                        if agent.status().await == crate::agent::Status::Running {
                            out("   .. agent still running; draining until it settles".to_string());
                            continue;
                        }
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    out("!! agent stopped".to_string());
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Wiring.
// ---------------------------------------------------------------------------

/// Builds the Session's fixed facts from config/env (the single env seam),
/// rooted at `root` (or the current dir).
fn build_session(root: Option<PathBuf>) -> anyhow::Result<Session> {
    let opts = SessionOpts {
        root: root.map(|p| p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    Session::new(opts).map_err(|e| anyhow::anyhow!("session: {e}"))
}

/// Starts the Agent with the real `Dispatcher` boundary (ADR-0020, ADR-0037)
/// over the Session's resolved Provider set, and the context-file-assembled
/// system prompt, resuming a prior Session Log when asked.
/// Builds the real `Dispatcher` boundary over the Session's Provider set and
/// enriches the launch Model's context window from the server (ADR-0037): the
/// sync-resolved window is a config/fallback guess; for a custom Provider the
/// host's live `n_ctx` is authoritative. The enriched `session` and the built
/// boundary come back together so the Agent AND the UI both read the true
/// window (the UI's initial Context Budget reads `session.model`). A down host
/// leaves the guessed window - discovery failure is never fatal.
async fn boundary_and_enriched_session(mut session: Session) -> (Session, Arc<Dispatcher>) {
    let llm = Arc::new(Dispatcher::new(session.providers.clone()));
    session.model = session
        .enrich_model_window(llm.as_ref(), session.model.clone())
        .await;
    (session, llm)
}

fn start_agent(
    session: Session,
    llm: Arc<Dispatcher>,
    resume: Option<String>,
    system_prompt: String,
) -> anyhow::Result<AgentHandle> {
    let mut opts = StartOpts::new(session, llm).with_system_prompt(system_prompt);
    if let Some(resume) = resume {
        opts.resume = Some(parse_resume(&resume));
    }
    AgentHandle::start(opts).map_err(|e| anyhow::anyhow!("{e}"))
}

fn parse_resume(resume: &str) -> crate::agent::Resume {
    if resume == "latest" {
        crate::agent::Resume::Latest
    } else {
        crate::agent::Resume::Path(resume.to_string())
    }
}

// ---------------------------------------------------------------------------
// Headless event streaming (ports Drive.handle/3).
// ---------------------------------------------------------------------------

fn is_settled(event: &Event) -> bool {
    matches!(
        event,
        Event::RunFinished { .. } | Event::RunError { .. } | Event::RunCancelled
    )
}

/// The thin edge for one event: the pure [`event_lines`] decides what to say,
/// this emits it - and answers the ONE arm that talks back to the Agent
/// (run_command Approvals are auto-approved: a diagnostic harness, not a
/// session for untrusted work).
async fn handle_event(
    agent: &AgentHandle,
    event: &Event,
    started: std::time::Instant,
    out: &mut impl FnMut(String),
) {
    for line in event_lines(event, started.elapsed().as_secs_f64()) {
        out(line);
    }
    if let Event::ApprovalRequest { approval_id, .. } = event {
        agent.approve(approval_id.clone(), Decision::Approve).await;
    }
}

/// The stdout lines for one event (ports Drive.handle/3), pure: elapsed time
/// arrives as seconds, not a clock, so a test can pin the formatting. Leading
/// blank lines ride inside the returned strings (each element is exactly one
/// `println!` at the edge).
fn event_lines(event: &Event, elapsed_secs: f64) -> Vec<String> {
    let t = format!("{elapsed_secs:.1}");
    match event {
        Event::MessageStart { pass } => vec![format!("\n-- pass {pass} (t={t}s) model call")],
        Event::MessageUpdate { .. } => vec![],
        Event::MessageEnd { content, .. } => {
            let text: Vec<&str> = content
                .iter()
                .filter_map(|b| match b {
                    crate::content::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let tools: Vec<&str> = content
                .iter()
                .filter_map(|b| match b {
                    crate::content::ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            let mut lines = vec![format!("   message_end (t={t}s) tools={tools:?}")];
            let text = text.join(" | ");
            if !text.is_empty() {
                lines.push(format!("   text: {}", trunc(&text, 500)));
            }
            lines
        }
        Event::ToolCall { name, input, .. } => {
            vec![format!(
                "   -> {name} {} (t={t}s)",
                trunc(&input.to_string(), 200)
            )]
        }
        Event::ToolResult {
            content, is_error, ..
        } => {
            let flag = if *is_error { "ERR" } else { "ok" };
            vec![format!(
                "   <- {flag} {}B (t={t}s): {}",
                content.len(),
                trunc(content, 160)
            )]
        }
        Event::ContextPressure {
            token_estimate,
            context_budget,
            max_tokens_reserve,
        } => {
            vec![format!(
                "   ## pressure token_estimate={token_estimate} context_budget={context_budget} max_tokens_reserve={max_tokens_reserve} (t={t}s)"
            )]
        }
        Event::CompactionProgress { status } => {
            vec![format!("\n   ## COMPACTION {status} (t={t}s)")]
        }
        Event::ApprovalRequest { command, .. } => {
            vec![format!("   ?? approval for: {command} -- auto-approving")]
        }
        Event::RunFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } => {
            vec![format!(
                "\n== turn_finished (t={t}s): stop_reason={stop_reason} token_estimate={token_estimate} context_budget={context_budget}"
            )]
        }
        Event::RunError { reason } => vec![format!("\n== TURN ERROR (t={t}s): {reason}")],
        Event::RunCancelled => vec![format!("\n== turn_cancelled (t={t}s)")],
        other => vec![format!(
            "   .. {} (t={t}s)",
            trunc(&format!("{other:?}"), 200)
        )],
    }
}

async fn print_estimate(agent: &AgentHandle, out: &mut impl FnMut(String)) {
    let conv = agent.conversation().await;
    out(format!("   token_estimate={}", conv.token_estimate()));
    out(format!("   messages={}", conv.messages.len()));
    out(format!("   plan={:?}", agent.plan().await));
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}...", &s[..floor_char_boundary(s, n)])
    } else {
        s.to_string()
    }
}

// str::floor_char_boundary is unstable; a small local equivalent keeps the
// truncation from splitting a multi-byte char.
fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}
