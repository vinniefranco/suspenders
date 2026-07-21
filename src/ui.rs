//! UI - the ratatui frontend, confined to this module (ADR-0001, ADR-0019).
//!
//! The submodules split by testability: [`screen`] is the PURE TEA fold root
//! (The Elm Architecture, ADR-0001), [`transcript`] the display-history store
//! it delegates to, and [`composer`] the Composer it offers keys and events to
//! first (ADR-0034), each with its rules and its tests;
//! [`viewport`] is the pure, tested scroll state (bottom-anchored, clamped,
//! only user actions re-pin); [`components`] is the ONE semantic→terminal
//! color mapping (ADR-0008); and this file - the `run` adapter - is the
//! untested-by-design driver that owns the terminal, maps crossterm input to
//! the core's pure [`screen::Key`], carries out the [`screen::Effect`]s
//! the core returns, and renders via [`components`]. Only this module and
//! [`components`] `use ratatui` / `use crossterm` (ADR-0019 invariant).

pub mod command;
pub mod components;
pub mod composer;
pub mod draft;
pub mod history;
pub mod markdown;
pub mod model_command;
pub mod picker;
pub mod screen;
pub mod selector;
pub mod slash;
pub mod transcript;
pub mod viewport;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures_util::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::broadcast::error::RecvError;

use crate::agent::AgentHandle;
use crate::approvals::Decision as AgentDecision;
use crate::event::Event;
use crate::history::History;
use crate::session::log::SessionEntry;
use crate::session::{Session, default_config_path};
use picker::{Picker, PickerOutcome};
use screen::{
    AgentCommand, Busy, Decision, Effect, Idle, Key, Screen, ScreenOpts, ScrollStep, Status,
};
use viewport::{Viewport, WHEEL_LINES};

/// How often the status-bar spinner advances while a Turn is running (~10 fps).
const TICK_MS: u64 = 100;

/// The last draw's measured viewport geometry: `(total wrapped lines,
/// viewport height)`. Scroll effects execute BETWEEN draws, and the adapter
/// cannot know the wrap-aware line total outside one (only the render path
/// measures the built `Paragraph`), so it feeds the pure [`Viewport`] the
/// previous frame's numbers. At worst one frame stale - harmless, because the
/// draw-time [`Viewport::top_offset`] clamp against the fresh measure is
/// authoritative.
type Geometry = (usize, usize);

/// Runs the ratatui frontend against a live [`AgentHandle`], returning when the
/// user quits (Ctrl-C / Ctrl-Q). Enters raw mode + the alternate screen for the
/// duration, restoring the terminal on the way out (even on error).
///
/// The loop is a `tokio::select!` over crossterm's async [`EventStream`] and the
/// Agent's broadcast [`Receiver`](tokio::sync::broadcast::Receiver): key presses
/// fold through the Screen core, agent events fold through it too, and the
/// returned [`Effect`]s are executed here (Agent calls, scroll/focus, history).
///
/// Mouse capture is enabled for the duration so the wheel scrolls the viewport
/// ([`Key::WheelUp`]/[`Key::WheelDown`]). A deliberate trade: capturing the
/// mouse disables the terminal's native text selection (shift-click usually
/// bypasses the capture). Capture is released before the terminal is restored,
/// on the error path too.
///
/// `launch_notices` are info lines from before the terminal existed (a
/// context-file skip at load); the Screen records them right after the
/// greeting.
pub async fn run(
    agent: AgentHandle,
    session: &Session,
    launch_notices: Vec<String>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    // Best-effort: a terminal without mouse support still gets a working TUI.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = run_loop(&mut terminal, agent, session, launch_notices).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Runs the `--resume` Session Picker full-screen and returns how it resolved.
/// Same terminal lifecycle as [`run`] - raw mode + alternate screen entered and
/// restored around the loop, mouse capture enabled/released symmetrically -
/// because the picker runs BEFORE the Agent starts, on its own screen.
///
/// Crossterm input folds through the pure [`picker::Picker`] core via the same
/// [`map_key`]/[`map_mouse`] mappings the Screen uses; Ctrl-C/Ctrl-Q
/// ([`is_quit`]) resolve as [`PickerOutcome::Quit`].
pub async fn pick_session(entries: Vec<SessionEntry>) -> anyhow::Result<PickerOutcome> {
    let mut terminal = ratatui::init();
    // Best-effort, like `run`: no mouse support still means a working picker.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = pick_loop(&mut terminal, entries).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn pick_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    entries: Vec<SessionEntry>,
) -> anyhow::Result<PickerOutcome> {
    let mut input = EventStream::new();
    let mut picker = Picker::new(entries);

    terminal.draw(|frame| components::render_picker(frame, &picker))?;

    loop {
        // An ended input stream means the terminal is gone: quit, don't spin.
        let Some(event) = input.next().await else {
            return Ok(PickerOutcome::Quit);
        };
        let outcome = match event {
            Ok(CtEvent::Key(key_event)) => {
                if is_quit(&key_event) {
                    return Ok(PickerOutcome::Quit);
                }
                if key_event.kind == KeyEventKind::Release {
                    continue;
                }
                picker.handle_key(map_key(&key_event))
            }
            Ok(CtEvent::Mouse(mouse)) => match map_mouse(&mouse) {
                Some(key) => picker.handle_key(key),
                None => continue,
            },
            Ok(_) => None,  // resize/etc.: repaint below
            Err(_) => None, // read error; keep going
        };
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        terminal.draw(|frame| components::render_picker(frame, &picker))?;
    }
}

/// The adapter-side context the [`Effect`] handlers need beyond the pure core:
/// the Agent, the config path a `/model` pick persists to, and the sender that
/// injects a fetched [`Event::SelectorReady`]/[`SelectorFailed`] back into the
/// [`run_loop`] select (ADR-0033). Bundled so the [`Effect`] plumbing stays a
/// few params, not a dozen.
pub(crate) struct AdapterCtx<'a> {
    pub(crate) agent: &'a AgentHandle,
    pub(crate) config_path: String,
    /// The loop's own event injection channel: a `/model` fetch runs in a
    /// spawned task (never blocking the select loop, ADR-0011) and posts its
    /// SelectorReady/SelectorFailed here, where the loop folds it like an Agent
    /// event.
    pub(crate) selector_tx: tokio::sync::mpsc::UnboundedSender<Event>,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent: AgentHandle,
    session: &Session,
    launch_notices: Vec<String>,
) -> anyhow::Result<()> {
    let mut events = agent.subscribe();
    let mut input = EventStream::new();

    // Adapter-injected events (a `/model` fetch's result). The fetch spawns a
    // task that awaits `agent.list_models()` off the select loop and posts the
    // resulting SelectorReady/SelectorFailed here; the loop folds it exactly
    // like an Agent event (ADR-0033). A local mpsc, not the Agent's broadcast:
    // the adapter owns this fetch, so it owns the channel.
    let (selector_tx, mut selector_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    // The connection facts the status bar shows - the fixed endpoint and the
    // mutable Active Model (ADR-0033). The pure Screen core stays
    // command-agnostic and holds neither, so the adapter carries them into the
    // render as one named-field carrier (never a position-coupled pair). The
    // model is seeded from the connection, then refreshed from the Agent after
    // any batch that could have changed it (a `/model` pick), never on the tick.
    let mut conn = components::ConnectionFacts {
        base_url: session.connection.base_url.clone(),
        model: session.connection.model.clone(),
    };
    let ctx = AdapterCtx {
        agent: &agent,
        config_path: default_config_path(),
        selector_tx,
    };

    // Persistent prompt history (up/down recall ACROSS Sessions). The store
    // lives beside the Session Logs; the pure core keeps the in-memory ring -
    // the adapter loads it at mount and appends on each submit (HistoryAppend).
    let history_store = history_path(session).and_then(|p| crate::history::open(&p).ok());
    let history = history_store
        .as_ref()
        .map(History::read)
        .unwrap_or_default();

    let mut screen = Some(Screen::new(ScreenOpts {
        context_budget: Some(session.context_budget),
        eviction_slack: session.eviction_slack,
        plugins: crate::plugins::configured(&session.plugins),
        history,
        notices: launch_notices,
    }));
    let mut viewport = Viewport::new();

    // The per-item render cache: settled items' lines and wrapped counts are
    // built once (per width / Ctrl-T state) instead of on every frame. Owned
    // here - it holds ratatui `Line`s, so it lives in the adapter/components
    // layer (ADR-0019), never in the pure core.
    let mut cache = components::RenderCache::new();

    // Drives the running-spinner animation: the event loop is otherwise idle
    // while the model thinks, so nothing would repaint. `spinner` is the frame
    // counter (only meaningful while running).
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut spinner: u64 = 0;

    // Initial paint; `geometry` tracks the last draw's measure for the scroll
    // effects (see [`Geometry`]).
    let mut geometry = draw(
        terminal,
        screen.as_ref().unwrap(),
        &viewport,
        &conn,
        spinner,
        &mut cache,
    )?;

    loop {
        tokio::select! {
            // Animation tick: advance the spinner and repaint, but ONLY while a
            // Turn is running - an idle UI does no work between events.
            _ = ticker.tick() => {
                if screen.as_ref().unwrap().status == Status::Running {
                    spinner = spinner.wrapping_add(1);
                    geometry = draw(terminal, screen.as_ref().unwrap(), &viewport, &conn, spinner, &mut cache)?;
                }
                continue;
            }

            // Terminal input. Bursts (wheel ticks, paste, held keys) are
            // coalesced: after handling one event, any IMMEDIATELY-available
            // events drain through the exact same path - quit checks, key
            // mapping, effects - and the batch pays for ONE draw, not one per
            // event. `dirty` mirrors the old per-event behavior: only
            // Release-kind keys skipped the repaint.
            maybe_input = input.next() => {
                let mut pending = maybe_input;
                let mut dirty = false;
                loop {
                    match pending {
                        Some(Ok(CtEvent::Key(key_event))) => {
                            if is_quit(&key_event) {
                                return Ok(());
                            }
                            if key_event.kind != KeyEventKind::Release {
                                // EVERY key folds through the pure core - Composer
                                // editing included - so all the rules (modal gating,
                                // edge-triggered history, cursor editing) live in one
                                // tested place.
                                let core = screen.take().unwrap();
                                let (core, effects) = core.handle_key(map_key(&key_event));
                                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_ref()).await);
                                dirty = true;
                            }
                        }
                        // Wheel scroll folds through the pure core like
                        // PageUp/PageDown (line steps rather than page steps);
                        // other mouse kinds are ignored.
                        Some(Ok(CtEvent::Mouse(mouse))) => {
                            if let Some(key) = map_mouse(&mouse) {
                                let core = screen.take().unwrap();
                                let (core, effects) = core.handle_key(key);
                                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_ref()).await);
                            }
                            dirty = true;
                        }
                        Some(Ok(_)) => dirty = true, // resize/etc.
                        Some(Err(_)) => dirty = true, // read error; keep going
                        None => return Ok(()), // input stream ended
                    }
                    // Drain whatever is already buffered; the first
                    // not-yet-ready poll ends the batch.
                    match next_if_ready(&mut input).await {
                        Some(next) => pending = next,
                        None => break,
                    }
                }
                // Committing a `/model` pick is a key press, so it lands in this
                // batch; refresh the Active Model the status bar shows (a cheap
                // in-process actor query, ADR-0017/0033 - never on the tick).
                conn.model = agent.active_model().await;
                if !dirty {
                    continue;
                }
            }

            // Agent events.
            recv = events.recv() => {
                match recv {
                    Ok(event) => {
                        let core = screen.take().unwrap();
                        let (core, effects) = core.apply_event(event);
                        screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_ref()).await);
                    }
                    // The broadcast lagged; resync by continuing (the next
                    // events carry the accumulated snapshot).
                    Err(RecvError::Lagged(_)) => {}
                    // The Agent's sender is gone - it crashed/stopped. Reset to
                    // a truthful idle state (agent-down) and keep the UI up.
                    Err(RecvError::Closed) => {
                        let core = screen.take().unwrap();
                        let (core, effects) = core.agent_down();
                        screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_ref()).await);
                        let geometry = draw(terminal, screen.as_ref().unwrap(), &viewport, &conn, spinner, &mut cache)?;
                        // Nothing more will arrive; wait only on input now.
                        return drain_input(terminal, input, screen.take().unwrap(), viewport, geometry, conn, cache).await;
                    }
                }
            }

            // Adapter-injected events: a `/model` fetch's SelectorReady/Failed,
            // posted by its spawned task (ADR-0033). Folded exactly like an Agent
            // event - the pure core's guarded SelectorReady/Failed arms flip the
            // Loading overlay (or ignore a stale delivery). The sender is held in
            // `ctx`, so this side never ends while the loop runs.
            Some(event) = selector_rx.recv() => {
                let core = screen.take().unwrap();
                let (core, effects) = core.apply_event(event);
                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_ref()).await);
                // The fetch result never changes the model, but a pick could
                // have raced in; refresh to stay truthful (still off the tick).
                conn.model = agent.active_model().await;
            }
        }

        geometry = draw(
            terminal,
            screen.as_ref().unwrap(),
            &viewport,
            &conn,
            spinner,
            &mut cache,
        )?;
    }
}

/// After the Agent is gone we keep the TUI responsive to quit/scroll only. The
/// Active Model can no longer change (no Agent to swap it), so the connection
/// facts are frozen - carried as one owned [`components::ConnectionFacts`].
async fn drain_input(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut input: EventStream,
    screen: Screen,
    mut viewport: Viewport,
    mut geometry: Geometry,
    conn: components::ConnectionFacts,
    mut cache: components::RenderCache,
) -> anyhow::Result<()> {
    loop {
        let (total_lines, height) = geometry;
        match input.next().await {
            Some(Ok(CtEvent::Key(key_event))) => {
                if is_quit(&key_event) {
                    return Ok(());
                }
                match map_key(&key_event) {
                    Key::PageUp => viewport.page_up(total_lines, height),
                    Key::PageDown => viewport.page_down(total_lines, height),
                    _ => {}
                }
                geometry = draw(terminal, &screen, &viewport, &conn, 0, &mut cache)?;
            }
            // The wheel still scrolls after the Agent is gone; other mouse
            // kinds are ignored.
            Some(Ok(CtEvent::Mouse(mouse))) => {
                match map_mouse(&mouse) {
                    Some(Key::WheelUp) => viewport.scroll_up(WHEEL_LINES, total_lines, height),
                    Some(Key::WheelDown) => viewport.scroll_down(WHEEL_LINES, total_lines, height),
                    _ => continue,
                }
                geometry = draw(terminal, &screen, &viewport, &conn, 0, &mut cache)?;
            }
            Some(_) => {}
            None => return Ok(()),
        }
    }
}

/// Polls `stream` ONCE with the real task context and returns immediately:
/// `Some(item)` when something was ready, `None` when it was not (the inner
/// `Option` is the stream's own end-of-stream signal).
///
/// This deliberately is NOT `FutureExt::now_or_never`. Crossterm's
/// [`EventStream`] hands the FIRST Pending poll's waker to its wake-up thread
/// and DISCARDS wakers from later polls until that thread fires - and
/// `now_or_never` polls with a no-op waker. A batch-ending `now_or_never`
/// poll therefore parked a waker that wakes nobody: the next keystroke woke
/// nothing and sat buffered until the 100ms animation tick happened to
/// re-poll the stream, so typing felt quantized/choppy. Polling with the real
/// context parks the run loop's own waker instead.
async fn next_if_ready<S: Stream + Unpin>(stream: &mut S) -> Option<Option<S::Item>> {
    std::future::poll_fn(|cx| {
        std::task::Poll::Ready(match std::pin::Pin::new(&mut *stream).poll_next(cx) {
            std::task::Poll::Ready(item) => Some(item),
            std::task::Poll::Pending => None,
        })
    })
    .await
}

/// Ctrl-C / Ctrl-Q quit the app (baud's global keybindings).
fn is_quit(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
}

/// Maps a crossterm [`KeyEvent`] to the pure core's [`Key`]. Text characters
/// (`y`/`n`/`a` matter to the modal; everything else edits the Composer) come
/// through as [`Key::Char`]; the navigation/edit keys map to their named
/// variants. The core handles ALL of them - the adapter never edits the
/// Composer itself.
fn map_key(key: &KeyEvent) -> Key {
    match key.code {
        // Alt-Enter inserts a newline into the draft. Terminals that send
        // Esc-prefixed Enter for Alt-Enter are normalized by crossterm, so
        // matching the ALT modifier is enough.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => Key::InsertNewline,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Up => Key::ArrowUp,
        KeyCode::Down => Key::ArrowDown,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Backspace => Key::Backspace,
        // Modifier-aware arms come BEFORE the generic Char arm, which would
        // otherwise swallow the keypress as a plain character.
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::ToggleThinking,
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::ToggleTools,
        // Any other Ctrl chord is a command, never text: since the core now
        // inserts every `Key::Char` into the Composer, letting e.g. Ctrl-X
        // through as Char('x') would type an 'x'.
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Other,
        KeyCode::Char(c) => Key::Char(c),
        _ => Key::Other,
    }
}

/// Maps a crossterm [`MouseEvent`] to the pure core's [`Key`]: the wheel
/// becomes [`Key::WheelUp`]/[`Key::WheelDown`]; every other mouse kind
/// (clicks, drags, moves) is ignored.
fn map_mouse(mouse: &MouseEvent) -> Option<Key> {
    match mouse.kind {
        MouseEventKind::ScrollUp => Some(Key::WheelUp),
        MouseEventKind::ScrollDown => Some(Key::WheelDown),
        _ => None,
    }
}

/// Carries out the Effects the pure core returned, threading the Screen
/// through the Agent retries (submit↔steer) the core asks for.
async fn run_effects(
    mut screen: Screen,
    effects: Vec<Effect>,
    ctx: &AdapterCtx<'_>,
    viewport: &mut Viewport,
    geometry: Geometry,
    history: Option<&History>,
) -> Screen {
    for effect in effects {
        screen = run_effect(screen, effect, ctx, viewport, geometry, history).await;
    }
    screen
}

async fn run_effect(
    screen: Screen,
    effect: Effect,
    ctx: &AdapterCtx<'_>,
    viewport: &mut Viewport,
    geometry: Geometry,
    history: Option<&History>,
) -> Screen {
    let agent = ctx.agent;
    // Scroll effects clamp against the LAST draw's measure (see [`Geometry`]);
    // the draw-time `top_offset` clamp corrects any staleness.
    let (total_lines, height) = geometry;
    match effect {
        Effect::Agent(AgentCommand::Submit(prompt)) => {
            let result = agent.submit(prompt.clone()).await;
            // The core records the outcome (ok appends the user line; busy
            // retries as steer) and may emit MORE effects.
            let outcome = result.map_err(|_| Busy);
            let (core, effects) = screen.submitted(prompt, outcome);
            Box::pin(run_effects(core, effects, ctx, viewport, geometry, history)).await
        }
        Effect::Agent(AgentCommand::Steer(text)) => {
            let result = agent.steer(text.clone()).await;
            let outcome = result.map_err(|_| Idle);
            let (core, effects) = screen.steered(text, outcome);
            Box::pin(run_effects(core, effects, ctx, viewport, geometry, history)).await
        }
        Effect::Agent(AgentCommand::Approve(id, decision)) => {
            agent.approve(id, to_agent_decision(decision)).await;
            screen
        }
        Effect::Agent(AgentCommand::Cancel) => {
            agent.cancel().await;
            screen
        }
        Effect::PinBottom => {
            viewport.pin_bottom();
            screen
        }
        Effect::ScrollUp(ScrollStep::Line) => {
            viewport.scroll_up(WHEEL_LINES, total_lines, height);
            screen
        }
        Effect::ScrollUp(ScrollStep::Page) => {
            viewport.page_up(total_lines, height);
            screen
        }
        Effect::ScrollDown(ScrollStep::Line) => {
            viewport.scroll_down(WHEEL_LINES, total_lines, height);
            screen
        }
        Effect::ScrollDown(ScrollStep::Page) => {
            viewport.page_down(total_lines, height);
            screen
        }
        // Focus effects are a no-op in the ratatui adapter: there is no separate
        // focusable widget tree; the modal captures keys via the pure core's
        // pending_approval, and the composer is always the input target.
        Effect::FocusModal | Effect::FocusComposer => screen,
        // Persist the submitted prompt so up/down recall survives across
        // Sessions. The pure core already added it to its in-memory ring; this
        // writes it through to the on-disk store (best-effort, never fatal).
        Effect::HistoryAppend(prompt) => {
            if let Some(store) = history {
                store.append(&prompt);
            }
            screen
        }
        // A committed Slash Command (ADR-0032/0033). The adapter routes it
        // through the single `command::run` seam - `is_handled` reflects exactly
        // what it routes, so an unwired registry entry is a visible info line,
        // never a silent drop.
        Effect::Command { name, generation } => command::run(screen, ctx, &name, generation).await,
        // A row was chosen from a command's selector (ADR-0033): routed through
        // the same seam as the command itself.
        Effect::SelectorChosen {
            command: cmd,
            value,
        } => command::choose(screen, ctx, &cmd, value).await,
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

/// Draws one frame and returns the measured viewport [`Geometry`]: the render
/// path syncs the [`components::RenderCache`] (settled items build once, per
/// width), sums the cached wrap-aware total, asks the pure [`Viewport`] for
/// the clamped offset, and draws only the visible slice + scrollbar - all
/// inside [`components::render`]; the adapter only stores the measure for the
/// scroll effects that arrive before the next draw.
fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    screen: &Screen,
    viewport: &Viewport,
    conn: &components::ConnectionFacts,
    spinner: u64,
    cache: &mut components::RenderCache,
) -> anyhow::Result<Geometry> {
    let mut geometry: Geometry = (0, 0);
    terminal.draw(|frame| {
        geometry = components::render(frame, screen, conn.view(), spinner, viewport, cache);
    })?;
    Ok(geometry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // The composer-editing rules themselves (insert/backspace/modal gating)
    // live in the pure core and are tested there; these tests only guard the
    // crossterm→Key mapping the adapter owns.

    // Regression: Ctrl-T must map to ToggleThinking, not be swallowed by the
    // generic Char arm as a plain 't' - the modifier arms must come first.
    #[test]
    fn ctrl_t_maps_to_toggle_thinking() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::ToggleThinking);
    }

    // Regression: Ctrl-O must map to ToggleTools, not be swallowed by the
    // generic Char arm as a plain 'o' - the modifier arms must come first.
    #[test]
    fn ctrl_o_maps_to_toggle_tools() {
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::ToggleTools);
    }

    #[test]
    fn plain_t_is_still_a_typed_char() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(map_key(&key), Key::Char('t'));
    }

    // Since the core inserts every Key::Char into the Composer, a Ctrl chord
    // leaking through as Char would TYPE its letter.
    #[test]
    fn other_ctrl_chords_are_commands_not_text() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::Other);
    }

    #[test]
    fn alt_enter_maps_to_insert_newline_plain_enter_to_enter() {
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(map_key(&alt), Key::InsertNewline);
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_key(&plain), Key::Enter);
    }

    #[test]
    fn cursor_navigation_keys_map_to_their_named_variants() {
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Key::Left
        );
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            Key::Right
        );
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Key::Home
        );
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            Key::End
        );
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_maps_to_wheel_keys_other_mouse_kinds_are_ignored() {
        assert_eq!(
            map_mouse(&mouse(MouseEventKind::ScrollUp)),
            Some(Key::WheelUp)
        );
        assert_eq!(
            map_mouse(&mouse(MouseEventKind::ScrollDown)),
            Some(Key::WheelDown)
        );
        assert_eq!(
            map_mouse(&mouse(MouseEventKind::Down(
                crossterm::event::MouseButton::Left
            ))),
            None
        );
        assert_eq!(map_mouse(&mouse(MouseEventKind::Moved)), None);
    }

    // next_if_ready must return without suspending in ALL three stream states -
    // a ready item, an ended stream, and (the one that matters) a stream with
    // nothing buffered. Suspending on the empty case would stall the input
    // batch loop until the next event instead of ending the batch.
    #[tokio::test]
    async fn next_if_ready_returns_a_buffered_item() {
        let mut stream = futures_util::stream::iter(vec![1u8]);
        assert_eq!(next_if_ready(&mut stream).await, Some(Some(1)));
    }

    #[tokio::test]
    async fn next_if_ready_reports_an_ended_stream() {
        let mut stream = futures_util::stream::iter(Vec::<u8>::new());
        assert_eq!(next_if_ready(&mut stream).await, Some(None));
    }

    #[tokio::test]
    async fn next_if_ready_returns_none_without_suspending_when_nothing_is_buffered() {
        let mut stream = futures_util::stream::pending::<u8>();
        assert_eq!(next_if_ready(&mut stream).await, None);
    }
}
