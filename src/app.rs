//! Composition root / bootstrap.
//!
//! Wires the Session, the Agent, and a frontend, then runs it. Two entry
//! points, one per ADR-0019 shape:
//!
//! * [`run_tui`] - the interactive ratatui frontend (ADR-0001), owning the
//!   terminal for the session.
//! * [`run_headless`] - the stdout event-subscriber runner (ADR-0019): submit
//!   each prompt as a sequential Turn in ONE session, stream every event to
//!   stdout, auto-approve run_command Approvals, and report the token estimate
//!   and message count on settlement.

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{AgentHandle, StartOpts};
use crate::approvals::Decision;
use crate::event::Event;
use crate::llm::AnthropicLlm;
use crate::session::{Session, SessionOpts};
use crate::ui::picker::PickerOutcome;

/// Launches the interactive ratatui frontend (ADR-0001, ADR-0019). Builds the
/// Session from config/env (a real `AnthropicLlm` + `Connection`), starts the
/// Agent, and hands the terminal to [`crate::ui::run`] (which enters/leaves raw
/// mode + the alternate screen around itself).
/// Bare `--resume` (no value) arrives as this sentinel and opens the picker.
const PICK: &str = "pick";

pub async fn run_tui(root: Option<PathBuf>, resume: Option<String>) -> anyhow::Result<()> {
    let session = build_session(root)?;
    let resume = if resume.as_deref() == Some(PICK) {
        // The picker needs the Session first: the logs live in its
        // session_dir. No sessions to pick from is silently a fresh start -
        // we're pre-alt-screen, and a note would just flash and vanish.
        let entries = crate::session::log::list(&session.session_dir);
        if entries.is_empty() {
            None
        } else {
            match crate::ui::pick_session(entries).await? {
                PickerOutcome::Resume(path) => Some(path),
                PickerOutcome::FreshSession => None,
                // Leave without starting the Agent.
                PickerOutcome::Quit => return Ok(()),
            }
        }
    } else {
        resume
    };
    let agent = start_agent(session.clone(), resume)?;
    crate::ui::run(agent, &session).await
}

/// The stdout event-subscriber runner (ports `scripts/drive.exs`, ADR-0019).
/// Starts the Agent, subscribes, and submits each prompt as a sequential Turn
/// in the SAME session; every event streams to stdout, run_command Approvals are
/// auto-approved (a diagnostic harness, not a session for untrusted work), and
/// each settle prints the token estimate and message count. An empty `prompts`
/// defaults to a single "evaluate this project" Turn (drive.exs's default).
pub async fn run_headless(
    root: Option<PathBuf>,
    resume: Option<String>,
    prompts: Vec<String>,
) -> anyhow::Result<()> {
    if resume.as_deref() == Some(PICK) {
        anyhow::bail!(r#"--resume without a value needs the TUI; pass a path or "latest""#);
    }

    let prompts = if prompts.is_empty() {
        vec!["evaluate this project".to_string()]
    } else {
        prompts
    };

    let session = build_session(root)?;
    let root_label = session.root.clone();
    let agent = start_agent(session, resume)?;
    let mut events = agent.subscribe();

    for prompt in prompts {
        let started = std::time::Instant::now();
        println!("\n== submit (root={root_label}): {prompt}");
        // A Turn boundary race with a still-running previous Turn is not
        // possible here: we drive Turns strictly sequentially, awaiting each
        // one's settlement before submitting the next.
        if let Err(_busy) = agent.submit(prompt).await {
            println!("!! agent busy; skipping");
            continue;
        }

        // Drain events until this Turn settles.
        loop {
            match events.recv().await {
                Ok(event) => {
                    let settled = is_settled(&event);
                    handle_event(&agent, &event, started).await;
                    if settled {
                        print_estimate(&agent).await;
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    println!("!! agent stopped");
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
/// rooted at `root` (or the current dir), with a real `Connection`.
fn build_session(root: Option<PathBuf>) -> anyhow::Result<Session> {
    let opts = SessionOpts {
        root: root.map(|p| p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    Session::new(opts).map_err(|e| anyhow::anyhow!("session: {e}"))
}

/// Starts the Agent with the real `AnthropicLlm` boundary (ADR-0020), resuming
/// a prior Session Log when asked.
fn start_agent(session: Session, resume: Option<String>) -> anyhow::Result<AgentHandle> {
    let llm = Arc::new(AnthropicLlm::new());
    let mut opts = StartOpts::new(session, llm);
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
        Event::TurnFinished { .. } | Event::TurnError { .. } | Event::TurnCancelled
    )
}

async fn handle_event(agent: &AgentHandle, event: &Event, started: std::time::Instant) {
    let t = elapsed(started);
    match event {
        Event::MessageStart { pass } => {
            println!("\n-- pass {pass} (t={t}s) model call");
        }
        Event::MessageUpdate { .. } => {}
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
            println!("   message_end (t={t}s) tools={tools:?}");
            let text = text.join(" | ");
            if !text.is_empty() {
                println!("   text: {}", trunc(&text, 500));
            }
        }
        Event::ToolCall { name, input, .. } => {
            println!("   -> {name} {} (t={t}s)", trunc(&input.to_string(), 200));
        }
        Event::ToolResult {
            content, is_error, ..
        } => {
            let flag = if *is_error { "ERR" } else { "ok" };
            println!(
                "   <- {flag} {}B (t={t}s): {}",
                content.len(),
                trunc(content, 160)
            );
        }
        Event::ContextPressure {
            token_estimate,
            context_budget,
            max_tokens_reserve,
        } => {
            println!(
                "   ## pressure token_estimate={token_estimate} context_budget={context_budget} max_tokens_reserve={max_tokens_reserve} (t={t}s)"
            );
        }
        Event::CompactionProgress { status } => {
            println!("\n   ## COMPACTION {status} (t={t}s)");
        }
        Event::ApprovalRequest {
            approval_id,
            command,
        } => {
            println!("   ?? approval for: {command} -- auto-approving");
            agent.approve(approval_id.clone(), Decision::Approve).await;
        }
        Event::TurnFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } => {
            println!(
                "\n== turn_finished (t={t}s): stop_reason={stop_reason} token_estimate={token_estimate} context_budget={context_budget}"
            );
        }
        Event::TurnError { reason } => {
            println!("\n== TURN ERROR (t={t}s): {reason}");
        }
        Event::TurnCancelled => {
            println!("\n== turn_cancelled (t={t}s)");
        }
        other => {
            println!("   .. {} (t={t}s)", trunc(&format!("{other:?}"), 200));
        }
    }
}

async fn print_estimate(agent: &AgentHandle) {
    let conv = agent.conversation().await;
    println!("   token_estimate={}", conv.token_estimate());
    println!("   messages={}", conv.messages.len());
    println!("   plan={:?}", agent.plan().await);
}

fn elapsed(started: std::time::Instant) -> String {
    format!("{:.1}", started.elapsed().as_secs_f64())
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
