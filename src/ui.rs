//! UI — the ratatui frontend, confined to this module (ADR-0001, ADR-0019).
//!
//! The submodules split by testability: [`transcript`] is the PURE TEA core
//! (The Elm Architecture, ADR-0001) with all the rules and all the tests;
//! [`components`] is the ONE semantic→terminal color mapping (ADR-0008); and
//! this file — the `run` adapter — is the untested-by-design driver that owns
//! the terminal, maps crossterm input to the core's pure [`transcript::Key`],
//! carries out the [`transcript::Effect`]s the core returns, and renders via
//! [`components`]. Only this module and [`components`] `use ratatui` /
//! `use crossterm` (ADR-0019 invariant).

pub mod components;
pub mod transcript;

use crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::broadcast::error::RecvError;

use crate::agent::AgentHandle;
use crate::approvals::Decision as AgentDecision;
use crate::history::History;
use crate::session::Session;
use transcript::{
    AgentCommand, Busy, Decision, Effect, Idle, Key, Status, Transcript, TranscriptOpts,
};

/// How often the status-bar spinner advances while a Turn is running (~10 fps).
const TICK_MS: u64 = 100;

/// Viewport scroll state the adapter tracks (the pure core only emits
/// [`Effect::ScrollUp`]/[`Effect::ScrollDown`]/[`Effect::PinBottom`]).
struct Viewport {
    /// Top-line offset; `0` follows the tail (pinned to bottom).
    scroll: u16,
    /// Whether the viewport is pinned to the tail.
    pinned: bool,
}

impl Viewport {
    fn new() -> Self {
        Viewport {
            scroll: 0,
            pinned: true,
        }
    }

    fn scroll_up(&mut self) {
        self.pinned = false;
        self.scroll = self.scroll.saturating_add(4);
    }

    fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_sub(4);
        if self.scroll == 0 {
            self.pinned = true;
        }
    }

    fn pin_bottom(&mut self) {
        self.pinned = true;
        self.scroll = 0;
    }
}

/// Runs the ratatui frontend against a live [`AgentHandle`], returning when the
/// user quits (Ctrl-C / Ctrl-Q). Enters raw mode + the alternate screen for the
/// duration, restoring the terminal on the way out (even on error).
///
/// The loop is a `tokio::select!` over crossterm's async [`EventStream`] and the
/// Agent's broadcast [`Receiver`](tokio::sync::broadcast::Receiver): key presses
/// fold through the Transcript core, agent events fold through it too, and the
/// returned [`Effect`]s are executed here (Agent calls, scroll/focus, history).
pub async fn run(agent: AgentHandle, session: &Session) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, agent, session).await;
    ratatui::restore();
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent: AgentHandle,
    session: &Session,
) -> anyhow::Result<()> {
    let mut events = agent.subscribe();
    let mut input = EventStream::new();

    // The base_url is a Session fact the status bar shows; the pure Transcript
    // core never sees it, so the adapter carries it into the render.
    let base_url = session.connection.base_url.clone();

    // Persistent prompt history (up/down recall ACROSS Sessions). The store
    // lives beside the Session Logs; the pure core keeps the in-memory ring —
    // the adapter loads it at mount and appends on each submit (HistoryAppend).
    let history_store = history_path(session).and_then(|p| crate::history::open(&p).ok());
    let history = history_store.as_ref().map(History::read).unwrap_or_default();

    let mut transcript = Some(Transcript::new(TranscriptOpts {
        context_budget: Some(session.context_budget),
        eviction_slack: session.eviction_slack,
        plugins: crate::plugins::configured(&session.plugins),
        history,
    }));
    let mut viewport = Viewport::new();

    // Drives the running-spinner animation: the event loop is otherwise idle
    // while the model thinks, so nothing would repaint. `spinner` is the frame
    // counter (only meaningful while running).
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner: u64 = 0;

    // Initial paint.
    draw(terminal, transcript.as_ref().unwrap(), &viewport, &base_url, spinner)?;

    loop {
        tokio::select! {
            // Animation tick: advance the spinner and repaint, but ONLY while a
            // Turn is running — an idle UI does no work between events.
            _ = ticker.tick() => {
                if transcript.as_ref().unwrap().status == Status::Running {
                    spinner = spinner.wrapping_add(1);
                    draw(terminal, transcript.as_ref().unwrap(), &viewport, &base_url, spinner)?;
                }
                continue;
            }

            // Terminal input.
            maybe_input = input.next() => {
                match maybe_input {
                    Some(Ok(CtEvent::Key(key_event))) => {
                        if is_quit(&key_event) {
                            return Ok(());
                        }
                        if key_event.kind == KeyEventKind::Release {
                            continue;
                        }
                        // Text input edits the composer directly; named keys go
                        // through the pure core. `edit_composer` hands the
                        // Transcript back (Err) when the key is not a composer
                        // edit, so it is never dropped.
                        match edit_composer(transcript.take().unwrap(), &key_event) {
                            Ok(edited) => transcript = Some(edited),
                            Err(core) => {
                                let (core, effects) = core.handle_key(map_key(&key_event));
                                transcript = Some(run_effects(core, effects, &agent, &mut viewport, history_store.as_ref()).await);
                            }
                        }
                    }
                    Some(Ok(_)) => {} // resize/mouse/etc.
                    Some(Err(_)) => {} // read error; keep going
                    None => return Ok(()), // input stream ended
                }
            }

            // Agent events.
            recv = events.recv() => {
                match recv {
                    Ok(event) => {
                        let core = transcript.take().unwrap();
                        let (core, effects) = core.apply_event(event);
                        transcript = Some(run_effects(core, effects, &agent, &mut viewport, history_store.as_ref()).await);
                    }
                    // The broadcast lagged; resync by continuing (the next
                    // events carry the accumulated snapshot).
                    Err(RecvError::Lagged(_)) => {}
                    // The Agent's sender is gone — it crashed/stopped. Reset to
                    // a truthful idle state (agent-down) and keep the UI up.
                    Err(RecvError::Closed) => {
                        let core = transcript.take().unwrap();
                        let (core, effects) = core.agent_down();
                        transcript = Some(run_effects(core, effects, &agent, &mut viewport, history_store.as_ref()).await);
                        draw(terminal, transcript.as_ref().unwrap(), &viewport, &base_url, spinner)?;
                        // Nothing more will arrive; wait only on input now.
                        return drain_input(terminal, input, transcript.take().unwrap(), viewport, base_url).await;
                    }
                }
            }
        }

        draw(terminal, transcript.as_ref().unwrap(), &viewport, &base_url, spinner)?;
    }
}

/// After the Agent is gone we keep the TUI responsive to quit/scroll only.
async fn drain_input(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut input: EventStream,
    mut transcript: Transcript,
    mut viewport: Viewport,
    base_url: String,
) -> anyhow::Result<()> {
    loop {
        match input.next().await {
            Some(Ok(CtEvent::Key(key_event))) => {
                if is_quit(&key_event) {
                    return Ok(());
                }
                match map_key(&key_event) {
                    Key::PageUp => viewport.scroll_up(),
                    Key::PageDown => viewport.scroll_down(),
                    _ => {}
                }
                draw(terminal, &transcript, &viewport, &base_url, 0)?;
                let _ = &mut transcript;
            }
            Some(_) => {}
            None => return Ok(()),
        }
    }
}

/// Ctrl-C / Ctrl-Q quit the app (baud's global keybindings).
fn is_quit(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
}

/// Applies plain text editing directly to the composer (character insert /
/// backspace), returning `Ok(updated)`. Returns `Err(transcript)` — the value
/// handed back unchanged — when the key is not a composer edit, so the caller
/// routes it through the pure core WITHOUT losing the Transcript.
///
/// While an Approval modal is open, NOTHING edits the composer — every key must
/// reach the pure core so it can swallow all but y/n/a/Escape.
// `Err` intentionally carries the whole Transcript back (ownership hand-off, not
// a propagating error), so a large Err variant is exactly what we want here.
#[allow(clippy::result_large_err)]
fn edit_composer(transcript: Transcript, key: &KeyEvent) -> Result<Transcript, Transcript> {
    if transcript.pending_approval.is_some() {
        return Err(transcript);
    }
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut value = transcript.input_value.clone();
            value.push(c);
            let cursor = value.chars().count();
            Ok(transcript.input_changed(value, cursor))
        }
        KeyCode::Backspace => {
            let mut value = transcript.input_value.clone();
            value.pop();
            let cursor = value.chars().count();
            Ok(transcript.input_changed(value, cursor))
        }
        _ => Err(transcript),
    }
}

/// Maps a crossterm [`KeyEvent`] to the pure core's [`Key`]. Text characters
/// (`y`/`n`/`a` matter to the modal) come through as [`Key::Char`]; the
/// navigation/control keys map to their named variants.
fn map_key(key: &KeyEvent) -> Key {
    match key.code {
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Up => Key::ArrowUp,
        KeyCode::Down => Key::ArrowDown,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char(c) => Key::Char(c),
        _ => Key::Other,
    }
}

/// Carries out the Effects the pure core returned, threading the Transcript
/// through the Agent retries (submit↔steer) the core asks for.
async fn run_effects(
    mut transcript: Transcript,
    effects: Vec<Effect>,
    agent: &AgentHandle,
    viewport: &mut Viewport,
    history: Option<&History>,
) -> Transcript {
    for effect in effects {
        transcript = run_effect(transcript, effect, agent, viewport, history).await;
    }
    transcript
}

async fn run_effect(
    transcript: Transcript,
    effect: Effect,
    agent: &AgentHandle,
    viewport: &mut Viewport,
    history: Option<&History>,
) -> Transcript {
    match effect {
        Effect::Agent(AgentCommand::Submit(prompt)) => {
            let result = agent.submit(prompt.clone()).await;
            // The core records the outcome (ok appends the user line; busy
            // retries as steer) and may emit MORE effects.
            let outcome = result.map_err(|_| Busy);
            let (core, effects) = transcript.submitted(prompt, outcome);
            Box::pin(run_effects(core, effects, agent, viewport, history)).await
        }
        Effect::Agent(AgentCommand::Steer(text)) => {
            let result = agent.steer(text.clone()).await;
            let outcome = result.map_err(|_| Idle);
            let (core, effects) = transcript.steered(text, outcome);
            Box::pin(run_effects(core, effects, agent, viewport, history)).await
        }
        Effect::Agent(AgentCommand::Approve(id, decision)) => {
            agent.approve(id, to_agent_decision(decision)).await;
            transcript
        }
        Effect::Agent(AgentCommand::Cancel) => {
            agent.cancel().await;
            transcript
        }
        Effect::PinBottom => {
            viewport.pin_bottom();
            transcript
        }
        Effect::ScrollUp => {
            viewport.scroll_up();
            transcript
        }
        Effect::ScrollDown => {
            viewport.scroll_down();
            transcript
        }
        // Focus effects are a no-op in the ratatui adapter: there is no separate
        // focusable widget tree; the modal captures keys via the pure core's
        // pending_approval, and the composer is always the input target.
        Effect::FocusModal | Effect::FocusComposer => transcript,
        // Persist the submitted prompt so up/down recall survives across
        // Sessions. The pure core already added it to its in-memory ring; this
        // writes it through to the on-disk store (best-effort, never fatal).
        Effect::HistoryAppend(prompt) => {
            if let Some(store) = history {
                store.append(&prompt);
            }
            transcript
        }
    }
}

/// The on-disk prompt-history path for this Session: a `history` file beside the
/// Session Log directory (e.g. `~/.local/share/suspenders/history`). `None` only
/// if `session_dir` has no parent (never, in practice).
fn history_path(session: &Session) -> Option<String> {
    std::path::Path::new(&session.session_dir)
        .parent()
        .map(|dir| dir.join("history").to_string_lossy().into_owned())
}

fn to_agent_decision(decision: Decision) -> AgentDecision {
    match decision {
        Decision::Approve => AgentDecision::Approve,
        Decision::Deny => AgentDecision::Deny,
        Decision::ApproveAlways => AgentDecision::ApproveAlways,
    }
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    transcript: &Transcript,
    viewport: &Viewport,
    base_url: &str,
    spinner: u64,
) -> anyhow::Result<()> {
    let scroll = if viewport.pinned { u16::MAX } else { viewport.scroll };
    terminal.draw(|frame| {
        components::render(frame, transcript, base_url, spinner, tail_scroll(frame, transcript, scroll))
    })?;
    Ok(())
}

// When pinned, scroll so the tail is visible; ratatui's Paragraph scroll is a
// top-offset, so we approximate "follow the tail" by scrolling past the top by
// the overflow. A conservative estimate keeps the newest lines on screen.
fn tail_scroll(
    frame: &ratatui::Frame,
    transcript: &Transcript,
    scroll: u16,
) -> u16 {
    if scroll != u16::MAX {
        return scroll;
    }
    let height = frame.area().height.saturating_sub(2); // status + input rows
    let lines = transcript.messages.len() as u16;
    lines.saturating_sub(height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn empty_transcript() -> Transcript {
        Transcript::new(TranscriptOpts {
            context_budget: Some(64_000),
            eviction_slack: 0.0,
            plugins: Vec::new(),
            history: Vec::new(),
        })
    }

    // Regression: on Enter (and any non-edit key) `edit_composer` must hand the
    // Transcript BACK as Err, never consume/drop it — the caller then routes the
    // key through the pure core. Dropping it made the adapter panic on the next
    // `transcript.take().unwrap()` when the user pressed Enter to submit.
    #[test]
    fn enter_returns_the_transcript_unconsumed() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(edit_composer(empty_transcript(), &key).is_err());
    }

    #[test]
    fn a_typed_char_edits_the_composer() {
        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        match edit_composer(empty_transcript(), &key) {
            Ok(t) => assert_eq!(t.input_value, "h"),
            Err(_) => panic!("a char edits the composer"),
        }
    }

    #[test]
    fn backspace_edits_the_composer() {
        let seeded = empty_transcript().input_changed("hi".to_string(), 2);
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        match edit_composer(seeded, &key) {
            Ok(t) => assert_eq!(t.input_value, "h"),
            Err(_) => panic!("backspace edits the composer"),
        }
    }
}
