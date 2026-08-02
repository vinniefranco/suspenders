//! UI - the ratatui frontend, confined to this module (ADR-0001, ADR-0019).
//!
//! The submodules split by testability: [`screen`] is the PURE TEA fold root
//! (The Elm Architecture, ADR-0001), [`transcript`] the display-history store
//! it delegates to (which now also owns the Commit high-water mark, ADR-0046),
//! and [`composer`] the Composer it offers keys and events to first (ADR-0034),
//! each with its rules and its tests; [`components`] is the ONE semantic→terminal
//! color mapping (ADR-0008); and this file - the `run` adapter - is the
//! untested-by-design driver that owns the FULLSCREEN alt-screen terminal
//! (ADR-0046: the app renders the ENTIRE transcript itself each frame, so there
//! are no cursor-position queries and resize re-wraps everything from the model),
//! maps crossterm input to the core's pure [`screen::Key`], carries out the
//! [`screen::Effect`]s the core returns, and renders via [`components`]. Only this
//! module and [`components`] `use ratatui` / `use crossterm` (ADR-0019 invariant).

pub mod command;
pub mod completion;
pub mod components;
pub mod composer;
pub mod draft;
pub mod file_search;
pub mod history;
pub mod lull;
pub mod markdown;
pub mod mcp_command;
pub mod model_command;
pub mod picker;
pub mod screen;
pub mod selection;
pub mod slash;
pub mod theme;
pub mod theme_command;
pub mod transcript;

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
use screen::{AgentCommand, Busy, Decision, Effect, Idle, Key, Screen, ScreenOpts, Status};
use theme::ActiveTheme;

/// How often the status-bar spinner advances while a Run is running (~10 fps).
/// `pub(crate)` so `components::live_lull_lines` can run the lull's tick count
/// into elapsed seconds at the same cadence the adapter ticks (ADR-0029: one
/// place ticks become real time).
pub(crate) const TICK_MS: u64 = 100;

/// Runs the ratatui frontend against a live [`AgentHandle`], returning when the
/// user quits (Ctrl-C / Ctrl-Q). Uses the FULLSCREEN alt-screen model (ADR-0046):
/// [`ratatui::init`] enters the alternate screen + raw mode and installs a
/// restoring panic hook, and the app renders the ENTIRE transcript itself each
/// frame. This kills the async `EventStream` vs. inline `get_cursor_position`
/// crash (fullscreen makes no cursor-position reads) and makes resize robust
/// (everything is redrawn from the model at the current size). Mouse capture is
/// enabled like the picker; teardown releases it and calls [`ratatui::restore`]
/// on both the success and `?`-propagated paths.
///
/// The loop is a `tokio::select!` over crossterm's async [`EventStream`] and the
/// Agent's broadcast [`Receiver`](tokio::sync::broadcast::Receiver): key presses
/// fold through the Screen core, agent events fold through it too, and the
/// returned [`Effect`]s are executed here (Agent calls, focus, history).
///
/// `launch_notices` are info lines from before the terminal existed (a
/// context-file skip at load, the theme fallback); the Screen records them
/// right after the header. `themes` is the launch-resolved Theme state
/// (ADR-0038): the active Theme every frame draws with, swappable by
/// `/theme`.
pub async fn run(
    agent: AgentHandle,
    session: &Session,
    launch_notices: Vec<String>,
    themes: ActiveTheme,
) -> anyhow::Result<()> {
    // The FULLSCREEN terminal (ADR-0046): the app owns the whole alt-screen and
    // redraws the transcript from the model each frame. `ratatui::init` enters the
    // alt-screen + raw mode and installs a panic hook that restores both, so no
    // manual raw-mode/hook plumbing is needed. Mouse capture is enabled like the
    // picker (best-effort: no mouse still means a working UI).
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);

    let result = run_loop(&mut terminal, agent, session, launch_notices, themes).await;

    // Teardown: release mouse capture and restore the terminal (leave the
    // alt-screen + raw mode). The panic hook `ratatui::init` set covers the
    // abnormal path; this covers the normal and `?`-propagated ones.
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
pub async fn pick_session(
    entries: Vec<SessionEntry>,
    theme: &theme::Theme,
) -> anyhow::Result<PickerOutcome> {
    let mut terminal = ratatui::init();
    // Best-effort, like `run`: no mouse support still means a working picker.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = pick_loop(&mut terminal, EventStream::new(), entries, theme).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn pick_loop<B, S>(
    terminal: &mut Terminal<B>,
    mut input: S,
    entries: Vec<SessionEntry>,
    theme: &theme::Theme,
) -> anyhow::Result<PickerOutcome>
where
    B: Backend,
    S: Stream<Item = std::io::Result<CtEvent>> + Unpin,
{
    let mut picker = Picker::new(entries);

    // The picker draws in the launch-resolved Theme (ADR-0038): the caller
    // resolved the configured name (with the dark fallback) before this.
    terminal.draw(|frame| components::render_picker(frame, &picker, theme))?;

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
        terminal.draw(|frame| components::render_picker(frame, &picker, theme))?;
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
    /// event. The `@path` file search reuses the SAME channel, posting
    /// FileSearchReady.
    pub(crate) selector_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    /// The project root the `@path` file search walks (Phase C2). Read once at
    /// launch (the pure core stays IO-free, ADR-0019); handed to each spawned
    /// search task.
    pub(crate) root: std::path::PathBuf,
    /// A short-TTL cache of the walked project tree (Phase C2), shared across the
    /// spawned `@path` search tasks so a keystroke burst walks the tree once.
    pub(crate) walk_cache: file_search::WalkCache,
}

/// The adapter-side MUTABLE state the [`Effect`] handlers act on - the
/// mutable twin of [`AdapterCtx`]: the active Theme state a `/theme` pick
/// swaps (ADR-0038), the pure scroll state, and the on-disk prompt-history
/// path. Owned by [`run_loop`] and passed as ONE `&mut` through the [`Effect`]
/// plumbing, so the recursion sites re-thread a single carrier instead of a
/// parameter per field.
pub(crate) struct AdapterState {
    pub(crate) themes: ActiveTheme,
    pub(crate) history: Option<String>,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent: AgentHandle,
    session: &Session,
    launch_notices: Vec<String>,
    themes: ActiveTheme,
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
        // The `@path` file search walks the Session's project root (Phase C2).
        root: std::path::PathBuf::from(&session.root),
        walk_cache: file_search::WalkCache::new(),
    };

    // Persistent prompt history (up/down recall ACROSS Sessions). The store
    // lives beside the Session Logs; the pure core keeps the in-memory ring -
    // the adapter loads it at mount and appends on each submit (HistoryAppend).
    let history_store = history_path(session).and_then(|p| open_history(&p));
    let history = history_store
        .as_deref()
        .map(read_history)
        .unwrap_or_default();

    // The startup Header facts (qwen `AppHeader`): the crate version, the launch
    // Model's scoped id, and the working directory. The cwd is read once here at
    // the edge (ADR-0019 keeps the pure core IO-free); the tip is picked
    // deterministically from the prompt-history length seed.
    let header = screen::HeaderFacts {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model: session.model.scoped_id(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        tip_seed: history.len(),
    };

    let mut screen = Some(Screen::new(ScreenOpts {
        context_budget: Some(session.context_budget_for(&session.model)),
        compaction_slack: session.compaction_slack,
        extensions: crate::extensions::configured(&session.extensions),
        history,
        notices: launch_notices,
        header,
    }));

    // The mutable adapter state the Effect handlers thread as one carrier:
    // the Theme state (ADR-0038) and the history path.
    let mut state = AdapterState {
        themes,
        history: history_store,
    };

    // The per-item render cache: settled items' lines and wrapped counts are
    // built once (per width / Ctrl-T state) instead of on every frame. Owned
    // here - it holds ratatui `Line`s, so it lives in the adapter/components
    // layer (ADR-0019), never in the pure core.
    let mut cache = components::RenderCache::new();

    // The Theme every frame draws with is `themes`'s (ADR-0038): the active
    // resolved Theme, or - while the `/theme` selector is open - the
    // highlighted row's preview. Swapped live by a `/theme` pick, which
    // reaches `themes` through the Effect plumbing below.

    // Drives the running-spinner animation: the event loop is otherwise idle
    // while the model thinks, so nothing would repaint. `spinner` is the frame
    // counter (only meaningful while running).
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut anim = components::Anim::default();
    // The monotonic host clock the pure core reads for the Approval radio's
    // digit-quick-select timeout (ADR-0049, `Screen::expire_approval`): elapsed
    // milliseconds since launch, advanced by the tick (no background timer).
    let started = std::time::Instant::now();

    // First frame (ADR-0046): the fullscreen renderer draws the WHOLE transcript
    // itself - the startup Header banner + any launch notices, the composer, and
    // the status - so a single draw shows a complete frame 1 with no keypress
    // needed and no commit seam to prime.
    draw_previewed(
        terminal,
        screen.as_ref().unwrap(),
        &conn,
        anim,
        &mut cache,
        &state,
    )?;

    loop {
        tokio::select! {
            // Animation tick: advance the spinner and repaint, but ONLY while a
            // Run is running - an idle UI does no work between events.
            _ = ticker.tick() => {
                // Drive the Approval radio's digit-quick-select timeout (ADR-0049):
                // the pure core reads the elapsed-millis clock and, on a fired
                // buffered digit, resolves the Approval. A no-op when no Approval
                // is open or nothing is buffered (the 3-row radio never buffers).
                {
                    let core = screen.take().unwrap();
                    let now = started.elapsed().as_millis() as u64;
                    let (core, effects) = core.expire_approval(now);
                    screen = Some(
                        dispatch(
                            terminal,
                            core,
                            effects,
                            &mut Adapter { ctx: &ctx, state: &mut state },
                        )
                        .await?,
                    );
                }
                let s = screen.as_ref().unwrap();
                if s.status == Status::Running {
                    anim.spinner = anim.spinner.wrapping_add(1);
                    if s.has_live_stream() {
                        // Output is streaming: no lull, reset the quiet clock.
                        anim.quiet_ticks = 0;
                    } else {
                        // A quiet tick. The 0 -> 1 edge begins a new lull, so
                        // bump the sequence that seeds the scene pick.
                        if anim.quiet_ticks == 0 {
                            anim.lull_seq = anim.lull_seq.wrapping_add(1);
                        }
                        anim.quiet_ticks = anim.quiet_ticks.saturating_add(1);
                    }
                    draw_previewed(terminal, s, &conn, anim, &mut cache, &state)?;
                } else {
                    // Idle between Runs: keep the lull clock at zero so the
                    // next Run's first quiet stretch is a fresh lull (fresh
                    // scene, full settle).
                    anim.quiet_ticks = 0;
                }
                continue;
            }

            // Terminal input. Bursts (paste, held keys) are coalesced: after
            // handling one event, any IMMEDIATELY-available events drain through
            // the exact same path - quit checks, key mapping, effects (Commit
            // included) - and the batch pays for ONE draw, not one per event.
            // `dirty` mirrors the old per-event behavior: only Release-kind keys
            // skipped the repaint.
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
                                // tested place. Record the drawn body height first so
                                // a PageUp/PageDown has its geometry-free page step
                                // (ADR-0046, Stage 2).
                                note_body_height(terminal, screen.as_mut().unwrap());
                                let core = screen.take().unwrap();
                                let (core, effects) = core.handle_key(map_key(&key_event));
                                screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state },
                                    )
                                    .await?,
                                );
                                dirty = true;
                            }
                        }
                        // Mouse wheel scrolls the transcript (ADR-0046, Stage 2):
                        // route it through `map_mouse` to the scroll keys, exactly
                        // as the Session Picker does. Non-wheel mouse kinds map to
                        // `None` and just repaint. Record the body height first for
                        // the PageUp/PageDown step parity.
                        Some(Ok(CtEvent::Mouse(mouse))) => {
                            if let Some(key) = map_mouse(&mouse) {
                                note_body_height(terminal, screen.as_mut().unwrap());
                                let core = screen.take().unwrap();
                                let (core, effects) = core.handle_key(key);
                                screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state },
                                    )
                                    .await?,
                                );
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
                        screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state },
                                    )
                                    .await?,
                                );
                    }
                    // The broadcast lagged; resync by continuing (the next
                    // events carry the accumulated snapshot).
                    Err(RecvError::Lagged(_)) => {}
                    // The Agent's sender is gone - it crashed/stopped. Reset to
                    // a truthful idle state (agent-down) and keep the UI up.
                    Err(RecvError::Closed) => {
                        let core = screen.take().unwrap();
                        let (core, effects) = core.agent_down();
                        screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state },
                                    )
                                    .await?,
                                );
                        draw_previewed(terminal, screen.as_ref().unwrap(), &conn, anim, &mut cache, &state)?;
                        // Nothing more will arrive; wait only on input now. The
                        // frozen frames draw in the ACTIVE Theme - any open
                        // /theme preview ends with the Agent.
                        let active = state.themes.active().clone();
                        return drain_input(
                            terminal,
                            input,
                            FrozenFrame {
                                screen: screen.take().unwrap(),
                                cache,
                                conn,
                                theme: active,
                            },
                        )
                        .await;
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
                screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state },
                                    )
                                    .await?,
                                );
                // The fetch result never changes the model, but a pick could
                // have raced in; refresh to stay truthful (still off the tick).
                conn.model = agent.active_model().await;
                conn.base_url = provider_base_url(session, &conn.model);
            }
        }

        draw_previewed(
            terminal,
            screen.as_ref().unwrap(),
            &conn,
            anim,
            &mut cache,
            &state,
        )?;
    }
}

/// The adapter environment [`dispatch`] runs an effect fold in: the immutable
/// [`AdapterCtx`] plumbing and the mutable [`AdapterState`]. These travel together
/// through the loop's effect dispatch, so they ride as ONE borrow-scoped carrier
/// instead of two positional params (SRP_PARAMS fix) - constructed per call so it
/// does not hold the loop's `state` borrowed across the wider `select!`.
struct Adapter<'a> {
    ctx: &'a AdapterCtx<'a>,
    state: &'a mut AdapterState,
}

/// The loop-level effect dispatcher for the fullscreen model (ADR-0046): runs the
/// [`Effect`]s the fold returned through [`run_effects`]. The inline commit seam
/// is gone - the fullscreen renderer redraws the whole transcript each frame - so
/// this no longer freezes anything into scrollback; it is a thin async wrapper
/// that keeps the loop's call sites uniform. `_terminal` is retained so a future
/// terminal-owning effect has a home and the signature stays stable.
async fn dispatch<B: Backend>(
    _terminal: &mut Terminal<B>,
    core: Screen,
    effects: Vec<Effect>,
    adapter: &mut Adapter<'_>,
) -> anyhow::Result<Screen> {
    let Adapter { ctx, state } = adapter;
    let ctx: &AdapterCtx = ctx;
    let state: &mut AdapterState = state;
    Ok(run_effects(core, effects, ctx, state).await)
}

/// The static render state a post-Agent [`drain_input`] repaints from: the frozen
/// [`Screen`], its render `cache`, and the connection facts + theme that build the
/// still [`FrameCtx`]. After the Agent is gone none of these changes, so they ride
/// as ONE owned carrier instead of four positional params (SRP_PARAMS fix).
struct FrozenFrame {
    screen: Screen,
    cache: components::RenderCache,
    conn: components::ConnectionFacts,
    theme: theme::Theme,
}

/// After the Agent is gone we keep the TUI responsive to quit only. The Active
/// Model can no longer change (no Agent to swap it), so the connection facts are
/// frozen - carried in [`FrozenFrame`]. Repaints the frame on resize/read-error
/// so it stays coherent at the current size.
async fn drain_input<B, S>(
    terminal: &mut Terminal<B>,
    mut input: S,
    frame: FrozenFrame,
) -> anyhow::Result<()>
where
    B: Backend,
    S: Stream<Item = std::io::Result<CtEvent>> + Unpin,
{
    let FrozenFrame {
        screen,
        mut cache,
        conn,
        theme,
    } = frame;
    // The frozen frame context, built ONCE: after the Agent is gone the conn and
    // theme never change and the Anim is still, so every repaint below shares it.
    let ctx = components::FrameCtx {
        conn: conn.view(),
        anim: components::Anim::default(),
        theme: &theme,
    };
    loop {
        // Every non-quit event is inert after the Agent is gone; a key, a resize,
        // or a read-error all just repaint so the frozen frame stays coherent.
        match input.next().await {
            Some(Ok(CtEvent::Key(key_event))) if is_quit(&key_event) => return Ok(()),
            Some(Ok(_)) => draw(terminal, &screen, &mut cache, ctx)?,
            Some(Err(_)) => {}
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
        // Ctrl-O (qwen `TOGGLE_COMPACT_MODE`): toggle compact mode. Ctrl-T is
        // RETIRED (ADR-0046/0052 completed the retirement) - it falls through to
        // the generic Ctrl-chord arm below as `Key::Other`, never typing a 't'.
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::ToggleCompact,
        // Ctrl-S (qwen `ShowMoreLines`): a keyboard page-up through the app-owned
        // transcript scroll (ADR-0046, Stage 2). BEFORE the generic Ctrl-chord/Char
        // arms so it is named intent, not a typed 's'.
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::ShowMore,
        // Shift+Tab cycles the Approval mode (ADR-0050). Crossterm reports it as
        // `BackTab`, or as `Tab` + SHIFT on terminals that do not synthesize
        // BackTab - both map to the same intent (qwen's win32 fallback is plain
        // Tab, which suspenders does not adopt: bare Tab stays inert).
        KeyCode::BackTab => Key::CycleApprovalMode,
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Key::CycleApprovalMode,
        // Bare Tab accepts the highlighted `/` palette suggestion (ADR-0051
        // System B); inert everywhere else (the Composer refuses it).
        KeyCode::Tab => Key::Tab,
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

/// Records the transcript body zone's height into the pure `Screen` (ADR-0046,
/// Stage 2) so its geometry-free PageUp/PageDown have a page step. Runs the same
/// pure layout the render uses ([`components::body_height`]) at the terminal's
/// current size, so the page matches the drawn body (measure == draw). A size read
/// failure leaves the last-known height standing (a harmless stale page). Called
/// from the input loop before folding a scroll key - NOT from the renderer, which
/// takes `&Screen` and stays pure.
fn note_body_height<B: Backend>(terminal: &Terminal<B>, screen: &mut Screen) {
    if let Ok(size) = terminal.size() {
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        screen.note_body_height(components::body_height(area, screen));
    }
}

/// Carries out the Effects the pure core returned, threading the Screen through
/// the Agent retries (submit↔steer) the core asks for. `state` is the ONE mutable
/// adapter-state carrier ([`AdapterState`], the mutable twin of [`AdapterCtx`]):
/// the Theme state a `/theme` pick swaps and the history path the appends write
/// through.
async fn run_effects(
    mut screen: Screen,
    effects: Vec<Effect>,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
) -> Screen {
    for effect in effects {
        screen = run_effect(screen, effect, ctx, state).await;
    }
    screen
}

// The effect interpreter's dispatch seam: one arm per [`Effect`] family, each
// routing to a cohesive handler so this function only integrates and never
// operates. The families are the separable concerns - agent commands, history
// persistence, and the slash-command seam - each of which owns its own logic in
// the handler below.
async fn run_effect(
    screen: Screen,
    effect: Effect,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
) -> Screen {
    match effect {
        Effect::Agent(command) => run_agent_command(command, screen, ctx, state).await,
        // Focus effects are a no-op in the ratatui adapter: there is no separate
        // focusable widget tree; the modal captures keys via the pure core's
        // pending_approval, and the composer is always the input target.
        Effect::FocusModal | Effect::FocusComposer => screen,
        Effect::HistoryAppend(prompt) => persist_history(screen, state, prompt),
        // A committed Slash Command (ADR-0032/0033). The adapter routes it
        // through the single `command::run` seam - `is_handled` reflects exactly
        // what it routes, so an unwired registry entry is a visible info line,
        // never a silent drop.
        Effect::Command { name, generation } => {
            command::run(screen, ctx, state, &name, generation).await
        }
        // A row was chosen from a command's selector (ADR-0033): routed through
        // the same seam as the command itself.
        Effect::SelectorChosen {
            command: cmd,
            value,
        } => command::choose(screen, ctx, state, &cmd, value).await,
        // A `@path` AT pattern changed (Phase C2, qwen `useAtCompletion`): spawn
        // the file search off the loop (like `/model`'s fetch) and let it post
        // FileSearchReady back through `ctx.selector_tx`. Fire-and-forget - the
        // screen is untouched; the fill lands later via the selector_rx arm.
        Effect::FileSearch { query, generation } => {
            file_search::spawn(
                ctx.walk_cache.clone(),
                ctx.root.clone(),
                ctx.selector_tx.clone(),
                query,
                generation,
            );
            screen
        }
        // The `/mcp` management dialog opened (ADR-0065 Phase E): kick the async
        // `mcp_views()` fetch that fills it, routed through the command seam like
        // `/model`'s fetch.
        Effect::McpCommand { generation } => {
            command::run(screen, ctx, state, mcp_command::NAME, generation).await
        }
        // A picked `/mcp` dialog action (ADR-0065 Phase E): run it against the
        // Agent off-loop and re-fetch views so the dialog reflects the change.
        Effect::McpAction {
            action,
            server,
            generation,
        } => mcp_command::act(screen, ctx, action, server, generation).await,
        // The `/mcp` AUTHENTICATE `c` copy (ADR-0065 Phase E, qwen
        // `copyToClipboardViaOsc52`): write the OSC52 escape to the terminal and
        // post the TTY-reached result back through `ctx.selector_tx` so the open
        // dialog flips its copy-feedback hint.
        Effect::ClipboardOsc52(text) => {
            let copied = copy_via_osc52(&text);
            let _ = ctx.selector_tx.send(Event::mcp_copy_result(copied));
            screen
        }
    }
}

/// Copies `text` to the clipboard via the OSC52 terminal escape (qwen's
/// `copyToClipboardViaOsc52`): base64-encodes it and writes
/// `\x1b]52;c;<base64>\x07` to stderr if it is a TTY, else stdout if it is,
/// else nowhere. Returns whether the sequence reached a TTY (a `true` does NOT
/// guarantee the terminal honoured it - some disable OSC52 by default). Works
/// over SSH and web terminals without spawning a subprocess.
fn copy_via_osc52(text: &str) -> bool {
    use base64::Engine;
    use std::io::{IsTerminal, Write};

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");

    if std::io::stderr().is_terminal() {
        write!(std::io::stderr(), "{seq}").is_ok()
    } else if std::io::stdout().is_terminal() {
        write!(std::io::stdout(), "{seq}").is_ok()
    } else {
        false
    }
}

/// Runs one [`AgentCommand`] against the live [`AgentHandle`]: `submit`/`steer`
/// feed their outcome back through the pure core (which may emit MORE effects,
/// hence the recursion through [`run_effects`]), while `approve`/`cancel` are
/// fire-through calls that leave the screen untouched.
async fn run_agent_command(
    command: AgentCommand,
    mut screen: Screen,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
) -> Screen {
    let agent = ctx.agent;
    match command {
        AgentCommand::Submit(prompt) => {
            // The core records the outcome (ok appends the user line; busy
            // retries as steer) and may emit MORE effects.
            let outcome = agent.submit(prompt.clone()).await.map_err(|_| Busy);
            let (core, effects) = screen.submitted(prompt, outcome);
            Box::pin(run_effects(core, effects, ctx, state)).await
        }
        AgentCommand::Steer(text) => {
            let outcome = agent.steer(text.clone()).await.map_err(|_| Idle);
            let (core, effects) = screen.steered(text, outcome);
            Box::pin(run_effects(core, effects, ctx, state)).await
        }
        AgentCommand::Approve(id, decision) => {
            agent.approve(id, to_agent_decision(decision)).await;
            screen
        }
        AgentCommand::CycleApprovalMode => {
            // Set the Screen mirror DIRECTLY from the authoritative fold result
            // (P0): the `ApprovalModeChanged` broadcast is lossy (a `Lagged`
            // in the event loop could drop it and leave the footer indicator
            // permanently stale, a safety-signal lie). The broadcast still
            // fires for any other subscribers; this call site no longer depends
            // on it for the mirror the footer reads.
            screen.approval_mode = agent.cycle_approval_mode().await;
            screen
        }
        AgentCommand::AnswerQuestion(id, answers) => {
            // Forward the user's picks (or the decline) to the parked tool call's
            // reply oneshot (ADR-0057). Fire-and-forget like Approve; the Agent
            // emits `question_resolved` once the tool reads the reply.
            agent.answer_question(id, answers).await;
            screen
        }
        AgentCommand::Cancel => {
            agent.cancel().await;
            screen
        }
    }
}

/// Persists a submitted prompt to the on-disk history ring and returns the
/// screen unchanged. The pure core already added it to its in-memory ring; this
/// writes it through to the on-disk store (best-effort, never fatal) so up/down
/// recall survives across Sessions.
fn persist_history(screen: Screen, state: &AdapterState, prompt: String) -> Screen {
    if let Some(path) = state.history.as_deref() {
        append_history(path, &prompt);
    }
    screen
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

/// Draws one frame in the Theme this frame should render with (ADR-0038's
/// live preview): the `/theme` selector's highlighted row while it is open
/// and Ready, the active Theme otherwise. Derived fresh each draw from the
/// pure core's own selector state - so Escape's exact revert is nothing but
/// the next frame, and a Theme change makes [`components::RenderCache`]
/// rebuild (it keys on the Theme), repainting everything including code
/// fences. The non-allocating [`composer::Composer::selector_highlight`]
/// answers exactly what the preview needs, so this pre-draw read never
/// clones the selector rows the render builds again anyway.
fn draw_previewed<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &Screen,
    conn: &components::ConnectionFacts,
    anim: components::Anim,
    cache: &mut components::RenderCache,
    state: &AdapterState,
) -> anyhow::Result<()> {
    let preview = theme_command::preview_name(screen.composer().selector_highlight());
    let theme = state.themes.render_theme(preview);
    draw(
        terminal,
        screen,
        cache,
        components::FrameCtx {
            conn: conn.view(),
            anim,
            theme,
        },
    )
}

/// Draws one FULLSCREEN frame (ADR-0046): the render path syncs the
/// [`components::RenderCache`] (settled items build once, per width) and draws
/// the WHOLE transcript (every settled item plus the live stream), bottom-
/// anchored and top-clipped, with the status bar and Composer below - all inside
/// [`components::render_pending`]. The per-frame connection/anim/theme travel as
/// one [`components::FrameCtx`].
fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &Screen,
    cache: &mut components::RenderCache,
    ctx: components::FrameCtx<'_>,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        components::render_pending(frame, screen, cache, ctx);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::ApprovalMode;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // The composer-editing rules themselves (insert/backspace/modal gating)
    // live in the pure core and are tested there; these tests only guard the
    // crossterm→Key mapping the adapter owns.

    // Regression: Ctrl-T is RETIRED (ADR-0046/0052). It no longer maps to a
    // display toggle - it falls through to the generic Ctrl-chord arm as
    // `Key::Other`, so it never types a literal 't'.
    #[test]
    fn ctrl_t_is_retired_and_maps_to_other() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::Other);
    }

    // Regression: Ctrl-O must map to ToggleCompact, not be swallowed by the
    // generic Char arm as a plain 'o' - the modifier arms must come first.
    #[test]
    fn ctrl_o_maps_to_toggle_compact() {
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::ToggleCompact);
    }

    #[test]
    fn plain_t_is_still_a_typed_char() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(map_key(&key), Key::Char('t'));
    }

    // Regression (BUG 1, ADR-0046): Ctrl-S must map to ShowMore (the peek), not be
    // swallowed as a plain 's' - the modifier arm must come before the generic
    // Char arm.
    #[test]
    fn ctrl_s_maps_to_show_more() {
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&key), Key::ShowMore);
    }

    #[test]
    fn plain_s_is_still_a_typed_char() {
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(map_key(&key), Key::Char('s'));
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
    fn bare_tab_maps_to_the_palette_accept_key() {
        // Bare Tab accepts the `/` palette suggestion (ADR-0051 System B);
        // inert everywhere else because the Composer refuses it.
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Key::Tab
        );
    }

    #[test]
    fn keys_without_a_mapping_fall_through_to_other() {
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Key::Other
        );
    }

    // Shift+Tab cycles the Approval mode (ADR-0050): crossterm reports it as
    // BackTab, or Tab + SHIFT on terminals that do not synthesize BackTab.
    #[test]
    fn shift_tab_maps_to_the_approval_mode_cycle() {
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            Key::CycleApprovalMode
        );
        assert_eq!(
            map_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
            Key::CycleApprovalMode
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
        pick_loop(
            &mut terminal,
            stream::iter(events),
            session_entries(n),
            theme::dark(),
        )
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
            theme::dark(),
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
        let cache = components::RenderCache::new();
        let conn = facts();
        drain_input(
            terminal,
            stream::iter(events),
            FrozenFrame {
                screen,
                cache,
                conn,
                theme: theme::dark().clone(),
            },
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

    // After the Agent is gone the pending region still draws its tail (native
    // scrollback owns history, ADR-0046), and inert keys/resize/read-errors just
    // repaint until a quit. The transcript no longer scrolls - there is no scroll
    // state to move.
    #[tokio::test]
    async fn drain_input_repaints_the_tail_and_survives_noise_until_quit() {
        let mut terminal = test_terminal(40, 12);
        drain(
            &mut terminal,
            vec![
                press(KeyCode::Char('x')),          // inert key: redraw only
                Ok(CtEvent::Resize(40, 12)),        // resize: repaint
                Err(std::io::Error::other("read")), // read error: keep going
                ctrl_press('q'),
            ],
        )
        .await
        .unwrap();
        // The pending region bottom-anchors and top-clips, so the NEWEST notice
        // is on screen even after the Agent is gone.
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
    use crate::view_model::TranscriptItem;
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
            root: std::path::PathBuf::from("/nonexistent/root"),
            walk_cache: file_search::WalkCache::new(),
        }
    }

    /// A launch-shaped AdapterState over no themes dir: active = dark, no
    /// history store. Tests that watch a field mutate hold one across calls; the
    /// rest build one per call.
    fn test_state() -> AdapterState {
        AdapterState {
            themes: ActiveTheme::launch("dark", std::path::PathBuf::from("/nonexistent/themes")).0,
            history: None,
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
    async fn run_effect_submit_records_the_user_line_and_appends_history() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let hist_dir = TempDir::new().unwrap();
        let history = store(&hist_dir);

        let mut state = test_state();
        state.history = Some(history.clone());

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Submit("hello agent".into())),
            &ctx,
            &mut state,
        )
        .await;

        // The core recorded the accepted submit as a user line...
        assert!(has_user_line(&screen, "hello agent"));
        // ...and its threaded HistoryAppend wrote through to the store (native
        // scrollback follows the tail, ADR-0046 - no PinBottom).
        assert_eq!(read_history(&history), vec!["hello agent".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_steer_while_idle_retries_as_a_submit() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("ok"))]));
        let ctx = adapter_ctx(&agent);

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Steer("redirect".into())),
            &ctx,
            &mut test_state(),
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

        // Park a Run mid-`complete`, so the Agent answers Busy.
        agent.submit("first").await.unwrap();
        let parked = tokio::time::timeout(Duration::from_secs(1), inflight.recv())
            .await
            .expect("the Turn parks")
            .expect("the barrier signals");

        let ctx = adapter_ctx(&agent);
        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::Submit("second".into())),
            &ctx,
            &mut test_state(),
        )
        .await;

        // Busy: no user line (steering is queued, not submitted); the core's
        // retry flipped it to a truthful Running status.
        assert_eq!(screen.status, Status::Running);
        assert!(!has_user_line(&screen, "second"));
        drop(parked); // release the barrier so the Run can end
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_approve_and_cancel_reach_the_agent_without_hanging() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);

        let screen = Screen::new(ScreenOpts::default());
        let screen = tokio::time::timeout(
            Duration::from_secs(1),
            run_effect(
                screen,
                Effect::Agent(AgentCommand::Approve("id-1".into(), Decision::Approve)),
                &ctx,
                &mut test_state(),
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
                &mut test_state(),
            ),
        )
        .await
        .expect("cancel returns");
    }

    // P0 (mode-mirror desync): the footer AutoAcceptIndicator must derive from
    // the AUTHORITATIVE cycle result, never from the lossy `ApprovalModeChanged`
    // broadcast (a `RecvError::Lagged` in the event loop could drop that event
    // and leave the mirror permanently stale - a safety-signal lie). This test
    // NEVER subscribes to events, so the broadcast is, from the Screen's point
    // of view, dropped; the mirror must still advance because `run_agent_command`
    // sets it directly from `cycle_approval_mode`'s return value.
    #[tokio::test(flavor = "multi_thread")]
    async fn cycle_updates_the_mirror_even_when_the_broadcast_is_dropped() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();

        // Fresh Screen starts at Default; no event subscriber exists here.
        let mut screen = Screen::new(ScreenOpts::default());
        assert_eq!(screen.approval_mode, ApprovalMode::Default);

        // One cycle through the real dispatch path lands on AutoEdit (qwen order:
        // plan → default → auto-edit → …) purely from the returned mode.
        screen = tokio::time::timeout(
            Duration::from_secs(1),
            run_effect(
                screen,
                Effect::Agent(AgentCommand::CycleApprovalMode),
                &ctx,
                &mut state,
            ),
        )
        .await
        .expect("cycle returns");
        assert_eq!(screen.approval_mode, ApprovalMode::AutoEdit);

        // A second cycle advances to Auto - again with no broadcast consumed.
        screen = run_effect(
            screen,
            Effect::Agent(AgentCommand::CycleApprovalMode),
            &ctx,
            &mut state,
        )
        .await;
        assert_eq!(screen.approval_mode, ApprovalMode::Auto);
    }

    // (The old scroll-effect executor test is retired: native scrollback owns
    // history, so there is no `ScrollUp`/`ScrollDown`/`PinBottom` effect and no
    // adapter-side viewport to move - ADR-0046.)

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_focus_effects_are_noops_in_this_adapter() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(screen, Effect::FocusModal, &ctx, &mut state).await;
        let screen = run_effect(screen, Effect::FocusComposer, &ctx, &mut state).await;

        // Focus effects change nothing in this adapter (no separate focusable
        // widget tree).
        assert_eq!(screen.status, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_history_append_writes_through_and_tolerates_no_store() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let hist_dir = TempDir::new().unwrap();
        let history = store(&hist_dir);
        let mut state = test_state();
        state.history = Some(history.clone());

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::HistoryAppend("saved".into()),
            &ctx,
            &mut state,
        )
        .await;
        assert_eq!(read_history(&history), vec!["saved".to_string()]);

        // No store opened (open_history failed at launch): the append is
        // dropped, never fatal - and the store on disk is untouched.
        state.history = None;
        let _ = run_effect(
            screen,
            Effect::HistoryAppend("dropped".into()),
            &ctx,
            &mut state,
        )
        .await;
        assert_eq!(read_history(&history), vec!["saved".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_routes_unhandled_commands_and_choices_to_visible_info_lines() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();

        let screen = Screen::new(ScreenOpts::default());
        let screen = run_effect(
            screen,
            Effect::Command {
                name: "compact".into(),
                generation: 0,
            },
            &ctx,
            &mut state,
        )
        .await;
        assert_eq!(last_info(&screen).as_deref(), Some("/compact: no handler"));

        let screen = run_effect(
            screen,
            Effect::SelectorChosen {
                command: "nope".into(),
                value: "dark".into(),
            },
            &ctx,
            &mut state,
        )
        .await;
        assert_eq!(last_info(&screen).as_deref(), Some("/nope: no handler"));
    }

    // -----------------------------------------------------------------------
    // The /theme flow through the Effect executor (ADR-0038): the same seam
    // /model routes through, with the Theme domain's ActiveTheme threaded
    // inside the AdapterState carrier.
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_theme_command_posts_the_rows_through_the_selector_channel() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let (selector_tx, mut selector_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AdapterCtx {
            agent: &agent,
            config_path: "/nonexistent/config.json".into(),
            selector_tx,
            root: std::path::PathBuf::from("/nonexistent/root"),
            walk_cache: file_search::WalkCache::new(),
        };

        let _ = run_effect(
            Screen::new(ScreenOpts::default()),
            Effect::Command {
                name: "theme".into(),
                generation: 7,
            },
            &ctx,
            &mut test_state(),
        )
        .await;

        // The rows arrive as a SelectorReady echoing the activation counter,
        // exactly like /model's fetch - built-ins listed, dark current.
        let Event::SelectorReady { generation, rows } =
            selector_rx.try_recv().expect("the rows were posted")
        else {
            panic!("expected SelectorReady");
        };
        assert_eq!(generation, 7);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["dark", "light"]);
        assert_eq!(rows[0].hint.as_deref(), Some("(current)"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_theme_choice_swaps_the_active_theme_and_persists_it() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let cfg_dir = TempDir::new().unwrap();
        let config_path = cfg_dir
            .path()
            .join("config.json")
            .to_string_lossy()
            .into_owned();
        let (selector_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AdapterCtx {
            agent: &agent,
            config_path: config_path.clone(),
            selector_tx,
            root: std::path::PathBuf::from("/nonexistent/root"),
            walk_cache: file_search::WalkCache::new(),
        };
        let mut state = test_state();

        let screen = run_effect(
            Screen::new(ScreenOpts::default()),
            Effect::SelectorChosen {
                command: "theme".into(),
                value: "light".into(),
            },
            &ctx,
            &mut state,
        )
        .await;

        // The live swap: the run loop's next frame draws light.
        assert_eq!(state.themes.active(), theme::light());
        // The applied info line (its env/persist variants are pinned in
        // theme_command's pure tests; ambient env must not fail this one).
        let info = last_info(&screen).expect("an applied line lands");
        assert!(info.starts_with("theme → light"), "info was: {info}");
        // The sticky write: only the theme key, in the config file.
        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(written.contains("\"theme\": \"light\""), "wrote: {written}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_re_choosing_the_current_theme_is_a_silent_no_op() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let cfg_dir = TempDir::new().unwrap();
        let config_path = cfg_dir
            .path()
            .join("config.json")
            .to_string_lossy()
            .into_owned();
        let (selector_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AdapterCtx {
            agent: &agent,
            config_path: config_path.clone(),
            selector_tx,
            root: std::path::PathBuf::from("/nonexistent/root"),
            walk_cache: file_search::WalkCache::new(),
        };
        let mut state = test_state();

        let screen = run_effect(
            Screen::new(ScreenOpts::default()),
            Effect::SelectorChosen {
                command: "theme".into(),
                value: "dark".into(),
            },
            &ctx,
            &mut state,
        )
        .await;

        // No swap, no write, no info line (ADR-0038, matching /model): the
        // Transcript's last info is still the header, untouched.
        assert_eq!(state.themes.active(), theme::dark());
        assert_eq!(
            last_info(&screen),
            last_info(&Screen::new(ScreenOpts::default()))
        );
        assert!(!std::path::Path::new(&config_path).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_effect_theme_choice_of_a_file_broken_after_open_refuses_and_persists_nothing() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![]));
        let cfg_dir = TempDir::new().unwrap();
        let config_path = cfg_dir
            .path()
            .join("config.json")
            .to_string_lossy()
            .into_owned();
        let (selector_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AdapterCtx {
            agent: &agent,
            config_path: config_path.clone(),
            selector_tx,
            root: std::path::PathBuf::from("/nonexistent/root"),
            walk_cache: file_search::WalkCache::new(),
        };

        // A valid theme at open time, broken before the pick lands: Enter
        // must re-load from disk (never the open-time previews cache), refuse
        // with the reason, and persist nothing - a stale swap here would
        // write a now-dangling name and silently fall back next launch.
        let themes_dir = TempDir::new().unwrap();
        std::fs::write(
            themes_dir.path().join("mine.toml"),
            "[colors]\nadded = \"#101010\"\n",
        )
        .unwrap();
        let mut state = test_state();
        state.themes = ActiveTheme::launch("dark", themes_dir.path().to_path_buf()).0;
        let _ = run_effect(
            Screen::new(ScreenOpts::default()),
            Effect::Command {
                name: "theme".into(),
                generation: 1,
            },
            &ctx,
            &mut state,
        )
        .await;
        std::fs::write(
            themes_dir.path().join("mine.toml"),
            "[colors]\nadded = \"greenish\"\n",
        )
        .unwrap();

        let screen = run_effect(
            Screen::new(ScreenOpts::default()),
            Effect::SelectorChosen {
                command: "theme".into(),
                value: "mine".into(),
            },
            &ctx,
            &mut state,
        )
        .await;

        let info = last_info(&screen).expect("the refusal surfaces");
        assert!(
            info.starts_with("theme → mine (not applied: colors.added:"),
            "info was: {info}"
        );
        assert_eq!(state.themes.active(), theme::dark(), "nothing swapped");
        assert!(
            !std::path::Path::new(&config_path).exists(),
            "nothing persisted"
        );
    }

    // FIRST FRAME (ADR-0046, fullscreen): a single draw of a launch Screen shows a
    // COMPLETE frame - the startup Header banner, the composer placeholder, and the
    // flat footer - all in the viewport, with NO keypress and no commit seam. The
    // whole transcript renders each frame, so the header stays visible (it is not
    // frozen into scrollback).
    #[test]
    fn first_frame_renders_header_composer_and_footer() {
        let state = test_state();
        let mut cache = components::RenderCache::new();

        // A FULLSCREEN TestBackend (the default viewport): tall enough to hold the
        // header, the body, the footer, and the composer.
        let mut terminal = test_terminal(48, 20);
        let conn = components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };

        let screen = Screen::new(ScreenOpts::default());
        draw_previewed(
            &mut terminal,
            &screen,
            &conn,
            components::Anim::default(),
            &mut cache,
            &state,
        )
        .unwrap();

        let frame = buffer_text(&terminal);
        assert!(
            frame.contains(">_ suspenders"),
            "the header wordmark renders in the fullscreen frame:\n{frame}"
        );
        assert!(
            frame.contains("Type your message"),
            "the composer placeholder is drawn:\n{frame}"
        );
        assert!(
            frame.contains("model m") && frame.contains("? for shortcuts"),
            "the flat footer (model fact + shortcuts hint) is drawn:\n{frame}"
        );
    }

    // A settled transcript item renders in the fullscreen frame (ADR-0046): with
    // the whole transcript drawn each frame, a run's settled answer appears in the
    // viewport - there is no commit seam moving it out of view.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settled_item_renders_in_the_fullscreen_frame() {
        let state = test_state();
        let mut cache = components::RenderCache::new();
        let mut terminal = test_terminal(48, 20);
        let conn = components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };

        // Stream + settle an assistant answer.
        let core = Screen::new(ScreenOpts::default());
        let (core, _) = core.apply_event(Event::run_started("r1"));
        let (core, _) = core.apply_event(Event::message_start(1));
        let (core, _) = core.apply_event(Event::message_update(
            crate::llm::Delta::Text("all done here".into()),
            vec![crate::content::ContentBlock::Text {
                text: "all done here".into(),
            }],
        ));
        let (core, _) = core.apply_event(Event::message_end(
            vec![crate::content::ContentBlock::Text {
                text: "all done here".into(),
            }],
            StopReason::EndTurn,
        ));

        draw_previewed(
            &mut terminal,
            &core,
            &conn,
            components::Anim::default(),
            &mut cache,
            &state,
        )
        .unwrap();

        let frame = buffer_text(&terminal);
        assert!(
            frame.contains("all done here"),
            "the settled answer renders in the fullscreen frame:\n{frame}"
        );
    }

    // RESIZE regression (ADR-0046): the whole transcript is redrawn from the model
    // at the current size, so shrinking the terminal re-wraps the header cleanly -
    // no leftover wide cells from the previous width. This guards the corruption
    // the old inline model showed when committed scrollback could not re-wrap.
    #[test]
    fn resize_re_renders_the_header_cleanly_at_the_new_width() {
        let state = test_state();
        let mut cache = components::RenderCache::new();
        let conn = components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };
        let screen = Screen::new(ScreenOpts::default());

        // Draw WIDE first, so the header lays out across a wide row.
        let mut terminal = test_terminal(80, 20);
        draw_previewed(
            &mut terminal,
            &screen,
            &conn,
            components::Anim::default(),
            &mut cache,
            &state,
        )
        .unwrap();
        assert!(
            buffer_text(&terminal).contains(">_ suspenders"),
            "the header renders at the wide width"
        );

        // Shrink to a NARROW width and redraw from the model.
        terminal.backend_mut().resize(30, 20);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 30, 20))
            .unwrap();
        draw_previewed(
            &mut terminal,
            &screen,
            &conn,
            components::Anim::default(),
            &mut cache,
            &state,
        )
        .unwrap();

        let narrow = buffer_text(&terminal);
        // Every drawn row fits the new width - no row is wider than 30 cells, so no
        // leftover wide cells survive the shrink.
        for line in narrow.lines() {
            assert!(
                line.chars().count() <= 30,
                "a row overflows the narrow width (leftover wide cells):\n{narrow}"
            );
        }
        // The header still renders (re-wrapped for the narrow width).
        assert!(
            narrow.contains("suspenders"),
            "the header wordmark re-renders at the narrow width:\n{narrow}"
        );
    }
}
