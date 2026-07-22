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
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::sync::broadcast::error::RecvError;

use crate::agent::AgentHandle;
use crate::approvals::Decision as AgentDecision;
use crate::event::Event;
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
    let result = pick_loop(&mut terminal, EventStream::new(), entries).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn pick_loop<B, S>(
    terminal: &mut Terminal<B>,
    mut input: S,
    entries: Vec<SessionEntry>,
) -> anyhow::Result<PickerOutcome>
where
    B: Backend,
    S: Stream<Item = std::io::Result<CtEvent>> + Unpin,
{
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

    // The connection facts the status bar shows - the launch Model's Provider
    // endpoint and the mutable Active Model's scoped id (ADR-0033/0037). The
    // pure Screen core stays command-agnostic and holds neither, so the
    // adapter carries them into the render as one named-field carrier (never a
    // position-coupled pair). The model is seeded from the launch Model, then
    // refreshed from the Agent after any batch that could have changed it (a
    // `/model` pick), never on the tick.
    let mut conn = components::ConnectionFacts {
        base_url: session
            .provider_of(&session.model)
            .map(|p| p.base_url.clone())
            .unwrap_or_default(),
        model: session.model.scoped_id(),
    };
    let ctx = AdapterCtx {
        agent: &agent,
        config_path: default_config_path(),
        selector_tx,
    };

    // Persistent prompt history (up/down recall ACROSS Sessions). The store
    // lives beside the Session Logs; the pure core keeps the in-memory ring -
    // the adapter loads it at mount and appends on each submit (HistoryAppend).
    let history_store = history_path(session).and_then(|p| open_history(&p));
    let history = history_store
        .as_deref()
        .map(read_history)
        .unwrap_or_default();

    let mut screen = Some(Screen::new(ScreenOpts {
        context_budget: Some(session.context_budget_for(&session.model)),
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
                                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_deref()).await);
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
                                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_deref()).await);
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
                // The endpoint follows the scoped id's Provider (ADR-0037): a
                // cross-Provider pick must not leave a stale base_url up.
                conn.model = agent.active_model().await;
                conn.base_url = provider_base_url(session, &conn.model);
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
                        screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_deref()).await);
                    }
                    // The broadcast lagged; resync by continuing (the next
                    // events carry the accumulated snapshot).
                    Err(RecvError::Lagged(_)) => {}
                    // The Agent's sender is gone - it crashed/stopped. Reset to
                    // a truthful idle state (agent-down) and keep the UI up.
                    Err(RecvError::Closed) => {
                        let core = screen.take().unwrap();
                        let (core, effects) = core.agent_down();
                        screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_deref()).await);
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
                screen = Some(run_effects(core, effects, &ctx, &mut viewport, geometry, history_store.as_deref()).await);
                // The fetch result never changes the model, but a pick could
                // have raced in; refresh to stay truthful (still off the tick).
                conn.model = agent.active_model().await;
                conn.base_url = provider_base_url(session, &conn.model);
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
async fn drain_input<B, S>(
    terminal: &mut Terminal<B>,
    mut input: S,
    screen: Screen,
    mut viewport: Viewport,
    mut geometry: Geometry,
    conn: components::ConnectionFacts,
    mut cache: components::RenderCache,
) -> anyhow::Result<()>
where
    B: Backend,
    S: Stream<Item = std::io::Result<CtEvent>> + Unpin,
{
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

/// The endpoint the status bar shows for a scoped Active Model id: the id's
/// Provider looked up in the Session's fixed set (ADR-0037). Empty when the
/// id does not resolve (it always does - `set_model` validated it).
fn provider_base_url(session: &Session, scoped: &str) -> String {
    crate::llm::model::split_scoped(scoped)
        .ok()
        .and_then(|(provider, _)| crate::llm::provider::find(&session.providers, provider))
        .map(|p| p.base_url.clone())
        .unwrap_or_default()
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
    history: Option<&str>,
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
    history: Option<&str>,
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
            if let Some(path) = history {
                append_history(path, &prompt);
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

// The on-disk prompt-history store: a size-capped newline-delimited file (a
// wrap log) - the oldest entries are discarded once the file exceeds the cap,
// so history stays bounded without unbounded growth. baud backs this with
// Erlang's `:disk_log` (a two-file wrap log at ~100 kB each); Rust has no
// `:disk_log`, so this port keeps the same *contract* - a bounded,
// append-only, order-preserving, crash-tolerant prompt ring - over a single
// file trimmed to the cap. Prompts are line-oriented user text, so one prompt
// per line round-trips without escaping; a torn tail is dropped on read,
// never load-bearing. Reads and appends re-open the file by path, so a crash
// between calls never loses committed rows.

/// The combined cap across the wrap log (~200 kB, matching baud's 2×100 kB).
const HISTORY_MAX_BYTES: usize = 200_000;

/// Opens (or creates) the history store at `path`, returning the path on
/// success. Safe to call multiple times per session. `None` on failure to
/// create the parent directory (the session then runs without persistence).
fn open_history(path: &str) -> Option<String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    // Touch the file so a subsequent read of a fresh log succeeds.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    Some(path.to_string())
}

/// Reads all entries from oldest to newest. Returns an empty list when the
/// store is empty or on any error (the history ring starts fresh).
fn read_history(path: &str) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(|l| l.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Appends one prompt. Does not deduplicate or cap the *count* - that is the
/// in-memory ring's job in the pure core - but trims the oldest rows to keep
/// the file under the byte cap (the wrap). Silently ignores errors so a full
/// disk or corrupted store never crashes the UI.
fn append_history(path: &str, text: &str) {
    use std::io::Write;

    let mut rows = read_history(path);
    rows.push(text.to_string());

    // Wrap: drop oldest rows until the serialized size fits the cap.
    while serialized_len(&rows) > HISTORY_MAX_BYTES && rows.len() > 1 {
        rows.remove(0);
    }

    let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(body.as_bytes());
    }
}

fn serialized_len(rows: &[String]) -> usize {
    rows.iter().map(|r| r.len() + 1).sum()
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
fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
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
    fn escape_paging_arrows_and_backspace_map_to_their_named_variants() {
        let cases = [
            (KeyCode::Esc, Key::Escape),
            (KeyCode::PageUp, Key::PageUp),
            (KeyCode::PageDown, Key::PageDown),
            (KeyCode::Up, Key::ArrowUp),
            (KeyCode::Down, Key::ArrowDown),
            (KeyCode::Backspace, Key::Backspace),
        ];
        for (code, expected) in cases {
            assert_eq!(map_key(&KeyEvent::new(code, KeyModifiers::NONE)), expected);
        }
    }

    #[test]
    fn keys_without_a_mapping_fall_through_to_other() {
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Key::Other
        );
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Key::Other
        );
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

    // The persistent prompt-history store (the on-disk wrap log the adapter
    // owns): open/read/append over a size-capped newline-delimited file.

    use tempfile::TempDir;

    fn store(dir: &TempDir) -> String {
        let path = dir.path().join("nested/history.log");
        open_history(&path.to_string_lossy()).unwrap()
    }

    #[test]
    fn open_creates_the_parent_directory_and_a_fresh_store_reads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        assert_eq!(read_history(&path), Vec::<String>::new());
    }

    #[test]
    fn append_then_read_returns_oldest_to_newest() {
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        append_history(&path, "first prompt");
        append_history(&path, "second prompt");
        append_history(&path, "third prompt");

        assert_eq!(
            read_history(&path),
            vec![
                "first prompt".to_string(),
                "second prompt".to_string(),
                "third prompt".to_string(),
            ]
        );
    }

    #[test]
    fn append_does_not_deduplicate() {
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        append_history(&path, "same");
        append_history(&path, "same");
        assert_eq!(
            read_history(&path),
            vec!["same".to_string(), "same".to_string()]
        );
    }

    #[test]
    fn reading_a_missing_store_yields_an_empty_list() {
        assert_eq!(
            read_history("/nonexistent/dir/history.log"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_wrap_discards_the_oldest_entries_past_the_cap() {
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);

        // One long-lived marker, then enough bulk to blow past the cap.
        append_history(&path, "OLDEST");
        let bulk = "x".repeat(10_000);
        for _ in 0..30 {
            append_history(&path, &bulk);
        }
        append_history(&path, "NEWEST");

        let rows = read_history(&path);
        // The newest survives; the oldest was wrapped out; the file stays bounded.
        assert_eq!(rows.last().unwrap(), "NEWEST");
        assert!(!rows.contains(&"OLDEST".to_string()));
        assert!(serialized_len(&rows) <= HISTORY_MAX_BYTES);
    }

    #[test]
    fn open_is_idempotent_and_preserves_existing_rows() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("history.log");
        let p = path.to_string_lossy().into_owned();

        let opened = open_history(&p).unwrap();
        append_history(&opened, "kept");

        let reopened = open_history(&p).unwrap();
        assert_eq!(read_history(&reopened), vec!["kept".to_string()]);
    }

    // -----------------------------------------------------------------------
    // pick_loop / drain_input - the input loops, driven end-to-end over a
    // ratatui TestBackend and a synthetic event stream (the same
    // `io::Result<CtEvent>` items crossterm's EventStream yields). Outcomes
    // and rendered state are asserted, never mere execution (ADR-0021).
    // -----------------------------------------------------------------------

    use crate::session::log::SessionEntry;
    use futures_util::stream;
    use ratatui::backend::TestBackend;

    type InputEvents = Vec<std::io::Result<CtEvent>>;

    fn press(code: KeyCode) -> std::io::Result<CtEvent> {
        Ok(CtEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl_press(c: char) -> std::io::Result<CtEvent> {
        Ok(CtEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )))
    }

    fn release(code: KeyCode) -> std::io::Result<CtEvent> {
        Ok(CtEvent::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )))
    }

    fn mouse_event(kind: MouseEventKind) -> std::io::Result<CtEvent> {
        Ok(CtEvent::Mouse(mouse(kind)))
    }

    fn test_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, height)).unwrap()
    }

    /// The last drawn frame as plain rows of text (styling dropped).
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let cells: Vec<&str> = buffer.content.iter().map(|cell| cell.symbol()).collect();
        cells
            .chunks(buffer.area.width as usize)
            .map(|row| row.concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn session_entries(n: usize) -> Vec<SessionEntry> {
        (0..n)
            .map(|i| SessionEntry {
                path: format!("/logs/{i}.jsonl"),
                stamp: format!("2026-07-1{i} 00:00"),
                label: format!("prompt {i}"),
            })
            .collect()
    }

    async fn pick(events: InputEvents, n: usize) -> PickerOutcome {
        let mut terminal = test_terminal(80, 24);
        pick_loop(&mut terminal, stream::iter(events), session_entries(n))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn pick_loop_ctrl_c_and_ctrl_q_quit() {
        assert_eq!(pick(vec![ctrl_press('c')], 2).await, PickerOutcome::Quit);
        assert_eq!(pick(vec![ctrl_press('q')], 2).await, PickerOutcome::Quit);
    }

    #[tokio::test]
    async fn pick_loop_an_ended_input_stream_quits() {
        assert_eq!(pick(vec![], 2).await, PickerOutcome::Quit);
    }

    #[tokio::test]
    async fn pick_loop_arrow_navigation_then_enter_resumes_the_selected_row() {
        let outcome = pick(vec![press(KeyCode::Down), press(KeyCode::Enter)], 3).await;
        assert_eq!(outcome, PickerOutcome::Resume("/logs/1.jsonl".into()));
    }

    #[tokio::test]
    async fn pick_loop_escape_starts_a_fresh_session() {
        assert_eq!(
            pick(vec![press(KeyCode::Esc)], 2).await,
            PickerOutcome::FreshSession
        );
    }

    #[tokio::test]
    async fn pick_loop_release_keys_are_skipped() {
        // A Release-kind Enter must NOT resume; the stream then ends, so the
        // loop quits - proof the release was skipped rather than folded.
        assert_eq!(
            pick(vec![release(KeyCode::Enter)], 2).await,
            PickerOutcome::Quit
        );
    }

    #[tokio::test]
    async fn pick_loop_the_wheel_moves_the_cursor() {
        let outcome = pick(
            vec![
                mouse_event(MouseEventKind::ScrollDown),
                press(KeyCode::Enter),
            ],
            3,
        )
        .await;
        assert_eq!(outcome, PickerOutcome::Resume("/logs/1.jsonl".into()));
    }

    #[tokio::test]
    async fn pick_loop_ignores_non_wheel_mouse_and_survives_resize_and_read_errors() {
        let outcome = pick(
            vec![
                mouse_event(MouseEventKind::Moved),
                Ok(CtEvent::Resize(80, 24)),
                Err(std::io::Error::other("tty gone")),
                press(KeyCode::Enter),
            ],
            2,
        )
        .await;
        // None of the noise moved the cursor or resolved the picker; Enter
        // still resumes the first (newest) row.
        assert_eq!(outcome, PickerOutcome::Resume("/logs/0.jsonl".into()));
    }

    #[tokio::test]
    async fn pick_loop_renders_the_rows_into_the_terminal() {
        let mut terminal = test_terminal(80, 24);
        let outcome = pick_loop(
            &mut terminal,
            stream::iter(vec![ctrl_press('c')]),
            session_entries(2),
        )
        .await
        .unwrap();
        assert_eq!(outcome, PickerOutcome::Quit);
        let text = buffer_text(&terminal);
        assert!(text.contains("prompt 0"), "rows are drawn:\n{text}");
    }

    // drain_input keeps the TUI alive for quit/scroll only after the Agent is
    // gone. The screen carries enough notice lines to overflow the viewport,
    // so scrolling has observable effect on the drawn frame.

    fn drained_screen() -> Screen {
        Screen::new(ScreenOpts {
            notices: (1..=40).map(|i| format!("notice-{i:02}")).collect(),
            ..ScreenOpts::default()
        })
    }

    fn facts() -> components::ConnectionFacts {
        components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "test-model".into(),
        }
    }

    async fn drain(
        terminal: &mut Terminal<TestBackend>,
        events: InputEvents,
    ) -> anyhow::Result<()> {
        let screen = drained_screen();
        let viewport = Viewport::new();
        let mut cache = components::RenderCache::new();
        let conn = facts();
        // A real first draw, exactly like run_loop's hand-off: the measured
        // geometry is what the scroll arms clamp against.
        let geometry = draw(terminal, &screen, &viewport, &conn, 0, &mut cache)?;
        drain_input(
            terminal,
            stream::iter(events),
            screen,
            viewport,
            geometry,
            conn,
            cache,
        )
        .await
    }

    #[tokio::test]
    async fn drain_input_ctrl_q_quits() {
        let mut terminal = test_terminal(40, 12);
        assert!(drain(&mut terminal, vec![ctrl_press('q')]).await.is_ok());
    }

    #[tokio::test]
    async fn drain_input_an_ended_stream_quits() {
        let mut terminal = test_terminal(40, 12);
        assert!(drain(&mut terminal, vec![]).await.is_ok());
    }

    #[tokio::test]
    async fn drain_input_page_up_scrolls_the_tail_out_of_view() {
        let mut terminal = test_terminal(40, 12);
        // Pinned bottom shows the newest notice; a PageUp must scroll it out.
        drain(&mut terminal, vec![press(KeyCode::PageUp), ctrl_press('q')])
            .await
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(
            !text.contains("notice-40"),
            "the tail scrolled out:\n{text}"
        );
    }

    #[tokio::test]
    async fn drain_input_page_down_returns_to_the_tail() {
        let mut terminal = test_terminal(40, 12);
        drain(
            &mut terminal,
            vec![
                press(KeyCode::PageUp),
                press(KeyCode::PageDown),
                ctrl_press('q'),
            ],
        )
        .await
        .unwrap();
        assert!(buffer_text(&terminal).contains("notice-40"));
    }

    #[tokio::test]
    async fn drain_input_the_wheel_scrolls_up_and_back_down() {
        let mut up = test_terminal(40, 12);
        drain(
            &mut up,
            vec![mouse_event(MouseEventKind::ScrollUp), ctrl_press('c')],
        )
        .await
        .unwrap();
        assert!(!buffer_text(&up).contains("notice-40"));

        let mut round_trip = test_terminal(40, 12);
        drain(
            &mut round_trip,
            vec![
                mouse_event(MouseEventKind::ScrollUp),
                mouse_event(MouseEventKind::ScrollDown),
                ctrl_press('c'),
            ],
        )
        .await
        .unwrap();
        assert!(buffer_text(&round_trip).contains("notice-40"));
    }

    #[tokio::test]
    async fn drain_input_other_keys_and_events_leave_the_tail_alone() {
        let mut terminal = test_terminal(40, 12);
        drain(
            &mut terminal,
            vec![
                press(KeyCode::Char('x')),          // not a scroll key: redraw only
                mouse_event(MouseEventKind::Moved), // non-wheel mouse: ignored
                Ok(CtEvent::Resize(40, 12)),        // other event kinds: ignored
                Err(std::io::Error::other("read")), // read error: keep going
                ctrl_press('q'),
            ],
        )
        .await
        .unwrap();
        assert!(buffer_text(&terminal).contains("notice-40"));
    }

    // -----------------------------------------------------------------------
    // run_effect - the Effect executor, over a REAL AgentHandle spawned on the
    // FakeLlm test double (the same harness as src/agent/tests.rs).
    // -----------------------------------------------------------------------

    use crate::agent::StartOpts;
    use crate::content::ContentBlock;
    use crate::llm::response::{Response, StopReason};
    use crate::session::{SessionConfig, SessionOpts};
    use crate::test_support::{Entry, FakeLlm};
    use crate::ui::transcript::TranscriptItem;
    use std::sync::Arc;
    use std::time::Duration;

    fn agent_session(dir: &TempDir) -> Session {
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds")
    }

    fn start_agent(dir: &TempDir, fake: FakeLlm) -> AgentHandle {
        AgentHandle::start(
            StartOpts::new(agent_session(dir), Arc::new(fake))
                .with_system_prompt("You are a test agent."),
        )
        .expect("agent starts")
    }

    fn end_turn(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            error: None,
        }
    }

    fn adapter_ctx(agent: &AgentHandle) -> AdapterCtx<'_> {
        let (selector_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AdapterCtx {
            agent,
            config_path: "/nonexistent/config.json".into(),
            selector_tx,
        }
    }

    fn has_user_line(screen: &Screen, text: &str) -> bool {
        screen
            .transcript()
            .items()
            .iter()
            .any(|item| matches!(item, TranscriptItem::User { text: t } if t == text))
    }

    fn last_info(screen: &Screen) -> Option<String> {
        screen
            .transcript()
            .items()
            .iter()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::Info { text } => Some(text.clone()),
                _ => None,
            })
    }

    #[test]
    fn provider_base_url_follows_the_scoped_ids_provider() {
        let dir = TempDir::new().unwrap();
        let session = agent_session(&dir);
        // The custom Provider's endpoint for its own scoped ids (the model id
        // may itself contain slashes; the scope is the first segment only).
        assert_eq!(
            provider_base_url(&session, "local/qwen/Qwen3.6-27B-MTP-GGUF"),
            "http://localhost:0/v1"
        );
        // A cross-Provider pick moves the endpoint with it (ADR-0037).
        assert_eq!(
            provider_base_url(&session, "anthropic/claude-fable-5"),
            "https://api.anthropic.com/v1"
        );
        // Unresolvable ids degrade to empty, never panic.
        assert_eq!(provider_base_url(&session, "unscoped"), "");
        assert_eq!(provider_base_url(&session, "nowhere/m"), "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_submit_records_the_user_line_pins_bottom_and_appends_history() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let hist_dir = TempDir::new().unwrap();
        let history = store(&hist_dir);

        // Unpin first, so the threaded PinBottom is observable.
        let mut viewport = Viewport::new();
        viewport.scroll_up(5, 100, 20);
        assert_eq!(viewport.top_offset(100, 20), 75);

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Submit("hello agent".into())),
            &ctx,
            &mut viewport,
            (100, 20),
            Some(&history),
        )
        .await;

        // The core recorded the accepted submit as a user line...
        assert!(has_user_line(&screen, "hello agent"));
        // ...its threaded PinBottom re-pinned the viewport...
        assert_eq!(viewport.top_offset(100, 20), 80);
        // ...and its threaded HistoryAppend wrote through to the store.
        assert_eq!(read_history(&history), vec!["hello agent".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_steer_while_idle_retries_as_a_submit() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("ok"))]));
        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Steer("redirect".into())),
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;

        // The Agent was Idle, so the steer came back Err(Idle) and the core
        // retried it as a submit: the text lands as a user line.
        assert!(has_user_line(&screen, "redirect"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_submit_while_busy_retries_as_steering() {
        let dir = TempDir::new().unwrap();
        let (entry, mut inflight) = Entry::barrier();
        let agent = start_agent(&dir, FakeLlm::script(vec![entry]));

        // Park a Turn mid-`complete`, so the Agent answers Busy.
        agent.submit("first").await.unwrap();
        let parked = tokio::time::timeout(Duration::from_secs(1), inflight.recv())
            .await
            .expect("the Turn parks")
            .expect("the barrier signals");

        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();
        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Submit("second".into())),
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;

        // Busy: no user line (steering is queued, not submitted); the core's
        // retry flipped it to a truthful Running status.
        assert_eq!(screen.status, Status::Running);
        assert!(!has_user_line(&screen, "second"));
        drop(parked); // release the barrier so the Turn can end
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_approve_and_cancel_reach_the_agent_without_hanging() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();

        let screen = Screen::new(ScreenOpts::default());
        let screen = tokio::time::timeout(
            Duration::from_secs(1),
            run_effect(
                screen,
                Effect::Agent(AgentCommand::Approve("id-1".into(), Decision::Approve)),
                &ctx,
                &mut viewport,
                (100, 20),
                None,
            ),
        )
        .await
        .expect("approve returns");

        tokio::time::timeout(
            Duration::from_secs(1),
            run_effect(
                screen,
                Effect::Agent(AgentCommand::Cancel),
                &ctx,
                &mut viewport,
                (100, 20),
                None,
            ),
        )
        .await
        .expect("cancel returns");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_scroll_effects_move_the_viewport_against_the_last_measure() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let geometry = (100, 20); // tail top offset is 80
        let mut viewport = Viewport::new();

        let mut screen = Screen::new(ScreenOpts::default());
        screen = run_effect(
            screen,
            Effect::ScrollUp(ScrollStep::Line),
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        assert_eq!(viewport.top_offset(100, 20), 80 - WHEEL_LINES);

        screen = run_effect(
            screen,
            Effect::ScrollUp(ScrollStep::Page),
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        assert_eq!(viewport.top_offset(100, 20), 58); // one page = height - 1

        screen = run_effect(
            screen,
            Effect::ScrollDown(ScrollStep::Line),
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        assert_eq!(viewport.top_offset(100, 20), 61);

        screen = run_effect(
            screen,
            Effect::ScrollDown(ScrollStep::Page),
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        assert_eq!(
            viewport.top_offset(100, 20),
            80,
            "a full page down re-pins at the tail"
        );

        screen = run_effect(
            screen,
            Effect::ScrollUp(ScrollStep::Line),
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        let _ = run_effect(
            screen,
            Effect::PinBottom,
            &ctx,
            &mut viewport,
            geometry,
            None,
        )
        .await;
        assert_eq!(viewport.top_offset(100, 20), 80, "PinBottom re-pins");
        // Pinned again: the tail is followed as content grows.
        assert_eq!(viewport.top_offset(200, 20), 180);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_focus_effects_are_noops_in_this_adapter() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::FocusModal,
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;
        let screen = run_effect(
            screen,
            Effect::FocusComposer,
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;

        assert_eq!(screen.status, Status::Idle);
        assert_eq!(viewport.top_offset(100, 20), 80, "the viewport never moved");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_history_append_writes_through_and_tolerates_no_store() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();
        let hist_dir = TempDir::new().unwrap();
        let history = store(&hist_dir);

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::HistoryAppend("saved".into()),
            &ctx,
            &mut viewport,
            (0, 0),
            Some(&history),
        )
        .await;
        assert_eq!(read_history(&history), vec!["saved".to_string()]);

        // No store opened (open_history failed at launch): the append is
        // dropped, never fatal - and the store on disk is untouched.
        let _ = run_effect(
            screen,
            Effect::HistoryAppend("dropped".into()),
            &ctx,
            &mut viewport,
            (0, 0),
            None,
        )
        .await;
        assert_eq!(read_history(&history), vec!["saved".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_routes_unhandled_commands_and_choices_to_visible_info_lines() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut viewport = Viewport::new();

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Command {
                name: "theme".into(),
                generation: 0,
            },
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;
        assert_eq!(last_info(&screen).as_deref(), Some("/theme: no handler"));

        let screen = run_effect(
            screen,
            Effect::SelectorChosen {
                command: "nope".into(),
                value: "dark".into(),
            },
            &ctx,
            &mut viewport,
            (100, 20),
            None,
        )
        .await;
        assert_eq!(last_info(&screen).as_deref(), Some("/nope: no handler"));
    }
}
