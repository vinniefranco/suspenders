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
        // An ask (Approval/Question) just opened: nudge the operator even when
        // their terminal is backgrounded (see [`emit_ask_notification`]).
        Effect::Notify(body) => {
            emit_ask_notification(&body);
            screen
        }
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

/// Makes an ask's text safe to carry inside an OSC 777 notification: strips the
/// control bytes that would terminate or corrupt the escape sequence (BEL, ESC,
/// the C1 String Terminator) and collapses newlines/tabs to spaces, then caps
/// the length so a long command does not fill the notification. A terminal that
/// ignores OSC 777 never sees this, but a well-behaved one gets a single clean
/// line.
fn sanitize_notification(body: &str) -> String {
    const MAX: usize = 120;
    let cleaned: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    match trimmed.char_indices().nth(MAX) {
        Some((byte_idx, _)) => format!("{}…", trimmed[..byte_idx].trim_end()),
        None => trimmed.to_string(),
    }
}

/// How the detected terminal wants a desktop notification. There is no single
/// escape sequence every POSIX terminal understands - the modern emulators split
/// across three incompatible OSC families and the rest support none - so we
/// detect the emulator ([`detect_notify_kind`]) and pick the one it speaks. The
/// universal BEL always rides along ([`notification_bytes`]); it is the ONLY
/// signal a bare xterm / Apple Terminal / VTE terminal can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyKind {
    /// iTerm2's `OSC 9 ; body`. iTerm2 (no separate title field, so we fold
    /// `title: body` into one line).
    Osc9,
    /// rxvt's `OSC 777 ; notify ; title ; body`. Ghostty, WezTerm, foot, urxvt.
    Osc777,
    /// kitty's `OSC 99 ; ; body`. kitty implements neither OSC 9 nor OSC 777, so
    /// it needs its own protocol (terminated by ST, not BEL).
    Osc99,
    /// No known desktop-notification OSC: rely on BEL alone. Covers xterm, st,
    /// Apple Terminal, VTE terminals (gnome-terminal, tilix, kgx), and anything
    /// under tmux/screen where a raw OSC would be stripped or mangled.
    BelOnly,
}

/// Picks the [`NotifyKind`] for the current terminal from its environment. Takes
/// the env reader as a closure so the (otherwise env-coupled) detection stays a
/// pure, table-testable function. Order matters: multiplexers are checked first
/// (they wrap whatever is inside), then emulators by their most specific marker.
fn detect_notify_kind(env: impl Fn(&str) -> Option<String>) -> NotifyKind {
    let term = env("TERM").unwrap_or_default();
    let term_program = env("TERM_PROGRAM").unwrap_or_default();

    // A multiplexer rewrites/strips OSC unless the user has enabled passthrough,
    // which we cannot assume; BEL is the reliable signal there (tmux turns it
    // into a bell/activity flag and forwards it to the outer terminal).
    if env("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux") {
        return NotifyKind::BelOnly;
    }
    // kitty implements OSC 99 ONLY (a deliberate choice against OSC 9 / 777).
    if env("KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
        return NotifyKind::Osc99;
    }
    // Ghostty (OSC 777, live-confirmed), WezTerm, foot, and urxvt all speak the
    // rxvt sequence.
    let is_ghostty = env("GHOSTTY_RESOURCES_DIR").is_some()
        || term_program == "ghostty"
        || term.contains("ghostty");
    let is_wezterm = env("WEZTERM_PANE").is_some() || env("WEZTERM_EXECUTABLE").is_some();
    if is_ghostty || is_wezterm || term.starts_with("foot") || term.starts_with("rxvt-unicode") {
        return NotifyKind::Osc777;
    }
    // iTerm2's own sequence.
    if term_program == "iTerm.app" {
        return NotifyKind::Osc9;
    }
    NotifyKind::BelOnly
}

/// Raises the ask notification on the current terminal: sanitize the body, pick
/// the emulator's sequence, and write it straight to stdout (crossterm has no
/// bell/notify command). Kept off [`run_effect`]'s dispatch so that stays pure
/// integration - this is the one operation seam that composes the notification
/// helpers and does the IO. It rides alongside the alt-screen rendering.
fn emit_ask_notification(body: &str) {
    use std::io::Write;
    let body = sanitize_notification(body);
    let kind = detect_notify_kind(|key| std::env::var(key).ok());
    let seq = notification_bytes(kind, "Suspenders", &body);
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Builds the notification byte string for a [`NotifyKind`]. Every variant trails
/// a BEL so a popup-capable terminal also flashes its urgency hint and a
/// `BelOnly` terminal still gets the one universal signal. Callers pass an
/// already-[`sanitize_notification`]d `body`; `title` is a trusted constant.
fn notification_bytes(kind: NotifyKind, title: &str, body: &str) -> String {
    match kind {
        NotifyKind::Osc9 => format!("\x1b]9;{title}: {body}\x07\x07"),
        NotifyKind::Osc777 => format!("\x1b]777;notify;{title};{body}\x07\x07"),
        // OSC 99 is ST-terminated (ESC \); the trailing BEL rings kitty's bell.
        NotifyKind::Osc99 => format!("\x1b]99;;{title}: {body}\x1b\\\x07"),
        NotifyKind::BelOnly => "\x07".to_string(),
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
#[path = "../tests/ui.rs"]
mod tests;
