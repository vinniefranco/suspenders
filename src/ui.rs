//! UI - the ratatui frontend, confined to this module (ADR-0001, ADR-0019).
//!
//! The submodules split by testability: [`screen`] is the PURE TEA fold root
//! (The Elm Architecture, ADR-0001), [`transcript`] the display-history store
//! it delegates to (which now also owns the Commit high-water mark, ADR-0046),
//! and [`composer`] the Composer it offers keys and events to first (ADR-0034),
//! each with its rules and its tests; [`components`] is the ONE semantic→terminal
//! color mapping (ADR-0008); and this file - the `run` adapter - is the
//! untested-by-design driver that owns the INLINE terminal (ADR-0046: committed
//! history scrolls into native scrollback via `insert_before`, the live pending
//! region redraws each frame), maps crossterm input to the core's pure
//! [`screen::Key`], carries out the [`screen::Effect`]s the core returns, and
//! renders via [`components`]. Only this module and [`components`] `use ratatui`
//! / `use crossterm` (ADR-0019 invariant).

pub mod command;
pub mod completion;
pub mod components;
pub mod composer;
pub mod draft;
pub mod file_search;
pub mod history;
pub mod lull;
pub mod markdown;
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
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Terminal, TerminalOptions, Viewport as TermViewport};
use tokio::sync::broadcast::error::RecvError;

/// The desired inline viewport height (rows) the pending region reserves
/// (ADR-0046): a fixed `Inline(cap)` where `cap = min(PENDING_CAP_DESIRED,
/// term_height - 1)`. Sized to hold the growing Composer (max ~min(term/3, 8))
/// plus room for an approval prompt or a short diff. On a real TTY these rows
/// stay reserved even when the live region is short (Ink does the same); older
/// pending rows scroll into native scrollback above.
const PENDING_CAP_DESIRED: u16 = 16;

/// Fallback terminal HEIGHT (rows) when `crossterm::terminal::size` cannot report
/// one (a detached/pipe context) - a conventional 80x24 terminal's height.
const FALLBACK_TERM_HEIGHT: u16 = 24;

/// Fallback terminal WIDTH (cols) when the backend cannot report its size while
/// sizing a committed-slice blit - a conventional 80-column terminal.
const FALLBACK_TERM_WIDTH: u16 = 80;

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
/// user quits (Ctrl-C / Ctrl-Q). Uses the INLINE viewport model (ADR-0046), NOT
/// the alternate screen: it enters raw mode and builds a fixed-height
/// `Viewport::Inline(cap)` terminal, so committed history scrolls into the
/// terminal's NATIVE scrollback (via `insert_before`) above a live pending
/// region that redraws each frame. Teardown leaves raw mode and drops a trailing
/// newline (no alt-screen to restore), on both the success and error paths.
///
/// The loop is a `tokio::select!` over crossterm's async [`EventStream`] and the
/// Agent's broadcast [`Receiver`](tokio::sync::broadcast::Receiver): key presses
/// fold through the Screen core, agent events fold through it too, and the
/// returned [`Effect`]s are executed here (Agent calls, the `Commit` freeze,
/// focus, history).
///
/// NO mouse capture (ADR-0046): the wheel no longer scrolls the viewport - native
/// scrollback owns history - so the terminal's own scroll and text selection work
/// unimpeded.
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
    // The INLINE terminal (ADR-0046): committed history scrolls into native
    // scrollback above a fixed-height live viewport, NOT the alternate screen.
    // `ratatui::init()` would enter the alt-screen; instead build the backend
    // directly with an `Inline(cap)` viewport and enter raw mode explicitly.
    // `cap = min(PENDING_CAP_DESIRED, term_height - 1)` so the reserved region
    // never eats the whole terminal.
    let term_height = crossterm::terminal::size()
        .map(|(_, h)| h)
        .unwrap_or(FALLBACK_TERM_HEIGHT);
    // FLOOR the inline cap at 1 (ADR-0046): on a degenerate 1-row terminal
    // `term_height - 1` is 0, and `Viewport::Inline(0)` would leave no live
    // region at all. `Inline(1)` still renders (a single bottom-anchored row -
    // the composer/status get clipped by the layout's `Min(1)`), so the UI stays
    // usable rather than blank. `PENDING_CAP_DESIRED` caps the reserved rows on a
    // normal terminal so the live region never eats the whole screen.
    let cap = PENDING_CAP_DESIRED
        .min(term_height.saturating_sub(1))
        .max(1);
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: TermViewport::Inline(cap),
        },
    )?;
    let _ = crossterm::terminal::enable_raw_mode();

    // Restore the terminal on a PANIC too (ADR-0046): the inline path has no
    // alt-screen to unwind, but raw mode must still be left or the shell is
    // wedged (no echo, no line editing). Chain the previous hook so the default
    // panic message still prints. A best-effort `disable_raw_mode` in the hook is
    // safe to run even on the normal exit path's double-call below.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        previous_hook(info);
    }));

    let result = run_loop(&mut terminal, agent, session, launch_notices, themes).await;

    // Teardown: leave raw mode and drop a trailing newline so the shell prompt
    // lands below the last live frame. No alt-screen to restore. (The panic hook
    // covers the abnormal path; this covers the normal and `?`-propagated ones.)
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::panic::take_hook();
    println!();
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

    // First frame, in three beats (ADR-0046). qwen commits the startup header to
    // Static scrollback UP FRONT; we mirror that so frame 1 shows header-in-
    // scrollback + composer + status, not a bare header waiting on a keypress.
    //
    // The launch notices (the startup Header banner plus any pre-terminal info
    // lines) are terminal, so `committable_upto()` already covers them while the
    // high-water mark sits at 0 - the exact prefix a fold would freeze. But the
    // fold-driven commit only runs on the FIRST event (key/agent/tick), so
    // without this the initial pending draw would paint the header still
    // UNCOMMITTED in the pending region, and the composer/status would not
    // settle until that first keypress triggered the deferred commit.
    //
    // Ordering matters (a known ratatui inline gotcha): `insert_before` before
    // the viewport is established misplaces it. So we (1) draw once to establish
    // the inline viewport area, (2) run the startup commit through `dispatch` -
    // whose trailing-freeze block blits precisely the committable notices into
    // native scrollback and advances the high-water mark - then (3) redraw a
    // clean pending region (composer + status) with the header now frozen
    // above it. Passing an EMPTY effect vector routes solely through dispatch's
    // `committable_upto - committed_high_water` trailing commit; no `Commit`
    // effect is minted outside `with_commit`.
    draw_previewed(
        terminal,
        screen.as_ref().unwrap(),
        &conn,
        anim,
        &mut cache,
        &state,
    )?;
    screen = Some(
        dispatch(
            terminal,
            screen.take().unwrap(),
            Vec::new(),
            &mut Adapter {
                ctx: &ctx,
                state: &mut state,
                cache: &mut cache,
            },
        )
        .await?,
    );
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
                            &mut Adapter { ctx: &ctx, state: &mut state, cache: &mut cache },
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
                                // tested place.
                                let core = screen.take().unwrap();
                                let (core, effects) = core.handle_key(map_key(&key_event));
                                screen = Some(
                                    dispatch(
                                        terminal,
                                        core,
                                        effects,
                                        &mut Adapter { ctx: &ctx, state: &mut state, cache: &mut cache },
                                    )
                                    .await?,
                                );
                                dirty = true;
                            }
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
                                        &mut Adapter { ctx: &ctx, state: &mut state, cache: &mut cache },
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
                                        &mut Adapter { ctx: &ctx, state: &mut state, cache: &mut cache },
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
                                        &mut Adapter { ctx: &ctx, state: &mut state, cache: &mut cache },
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

/// The loop-level effect dispatcher for the inline model (ADR-0046): freezes any
/// [`Effect::Commit`] slice into native scrollback via `insert_before` (the ONE
/// place with the terminal + cache + theme to draw the committed items), then
/// runs the remaining effects through [`run_effects`]. Commit effects originate
/// only at the two public fold exits (`apply_event`/`handle_key`), never from
/// the submit/steer recursion inside `run_effects`, so handling them here - once
/// per fold, before the rest - is complete.
/// The adapter environment [`dispatch`] runs an effect fold in: the immutable
/// [`AdapterCtx`] plumbing, the mutable [`AdapterState`], and the render `cache`
/// the commit seam blits through. These three always travel together through the
/// loop's effect dispatch, so they ride as ONE borrow-scoped carrier instead of
/// three positional params (SRP_PARAMS fix) - constructed per call so it does not
/// hold the loop's `state`/`cache` borrowed across the wider `select!`.
struct Adapter<'a> {
    ctx: &'a AdapterCtx<'a>,
    state: &'a mut AdapterState,
    cache: &'a mut components::RenderCache,
}

async fn dispatch<B: Backend>(
    terminal: &mut Terminal<B>,
    mut core: Screen,
    effects: Vec<Effect>,
    adapter: &mut Adapter<'_>,
) -> anyhow::Result<Screen> {
    let Adapter { ctx, state, cache } = adapter;
    let ctx: &AdapterCtx = ctx;
    let state: &mut AdapterState = state;
    let cache: &mut components::RenderCache = cache;
    // Freeze committed slices first, while `core` (and its cache) still describe
    // the state the commit was computed against. Committed items are terminal
    // and immutable, so freezing before running the other effects is safe. The
    // theme is the ACTIVE one - frozen scrollback must never bake a live
    // /theme preview. The commit is TRANSACTIONAL (ADR-0046): `commit_items`
    // advances the pure core's high-water mark ONLY after `insert_before`
    // succeeds, and a blit error propagates as FATAL (`?`) so the run loop can
    // tear the terminal down rather than leave the seam half-applied.
    let active = state.themes.active().clone();
    let mut rest = Vec::with_capacity(effects.len());
    for effect in effects {
        match effect {
            Effect::Commit { count } => commit_items(terminal, &mut core, cache, count, &active)?,
            // A compact toggle over frozen scrollback (ADR-0052): the pure fold
            // already flipped `compact_mode`, so the pending region redraws at the
            // new compact for free next frame; this only repaints the live
            // viewport so the toggle takes hold with no transient artifact. See
            // [`redraw_scrollback`] for the SPIKE result and the degraded-fallback
            // scope (the frozen prefix above the fold stays at the old compact -
            // ratatui exposes no portable scrollback purge).
            Effect::RedrawScrollback => redraw_scrollback(terminal, &mut core, cache, &active)?,
            // Ctrl-S peek (ADR-0046): blit the FULL, unclamped pending body into
            // scrollback so the user reads the top-clipped rows. Non-committing -
            // it reads `core` but does NOT advance the high-water mark, so the same
            // body redraws (clipped) in the live viewport next frame.
            Effect::PeekPending => peek_pending(terminal, &core, cache, &active)?,
            other => rest.push(other),
        }
    }
    let mut core = run_effects(core, rest, ctx, state).await;

    // A TRAILING freeze (ADR-0046): the submit/steer/command effects run through
    // the pure core's outcome hooks (`submitted`/`steered`/`info`), which route
    // through `with_commit` and so can make a new leading prefix terminal (the
    // just-appended User or info line). Those hooks run inside `run_effects`,
    // which has no terminal, so their `Commit` is dropped there and re-derived
    // HERE - keeping ALL freezing in `dispatch` (the one place with the terminal)
    // and the "every public transcript-mutating exit advances the seam" rule
    // uniform. `committable_upto - committed_high_water` is the exact same
    // computation `with_commit` runs, so this freezes precisely the prefix those
    // hooks marked committable and nothing more.
    let trailing = core
        .transcript()
        .committable_upto()
        .saturating_sub(core.transcript().committed_high_water());
    if trailing > 0 {
        commit_items(terminal, &mut core, cache, trailing, &active)?;
    }
    Ok(core)
}

/// Freezes the just-committed slice `[hw, hw + count)` into native scrollback
/// (ADR-0046): sizes a temp [`Buffer`](ratatui::buffer::Buffer) to the slice's
/// wrapped height and hands it to `terminal.insert_before`, which scrolls it
/// into the region above the inline viewport (overflow past the top goes to the
/// terminal's own scrollback). The cache is synced at the same full content
/// width the pending region measures at, so the committed wrap matches the
/// pending wrap exactly (ADR-0029). A no-op on a non-inline viewport (e.g. a
/// `TestBackend` without `Inline`), so headless tests stay valid.
///
/// TRANSACTIONAL (ADR-0046): the pure fold left the high-water mark UNMOVED - it
/// only emitted the count. This adapter is the sole mover: it advances the mark
/// via [`Screen::mark_committed`] ONLY after `insert_before` returns `Ok`. A
/// blit error is FATAL and propagates (`?`): the slice stays uncommitted, so it
/// redraws in the pending region rather than vanishing, and the caller tears the
/// terminal down. The `height == 0` case (a fully-blank slice) still advances the
/// mark - zero rows commit, nothing is lost.
fn commit_items<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Screen,
    cache: &mut components::RenderCache,
    count: usize,
    theme: &theme::Theme,
) -> anyhow::Result<()> {
    if count == 0 {
        return Ok(());
    }
    // Capture the mark ONCE: the slice to freeze is `[hw, hw + count)` (the pure
    // fold did not advance it).
    let hw = screen.transcript().committed_high_water();

    // Sync the cache at the content width the commit draws at (full frame width
    // minus the two `CONTENT_MARGIN` columns, no scrollbar) so measure == draw.
    // `insert_before` renders into a full-terminal-width buffer, so the content
    // width is the terminal width minus those margins.
    let width = terminal
        .size()
        .map(|s| s.width)
        .unwrap_or(FALLBACK_TERM_WIDTH);
    let content_width = width.saturating_sub(2 * components::CONTENT_MARGIN);
    components::sync_commit_cache(cache, screen, content_width, theme);

    // Bound the item list to the committed slice `[0, hw + count)` so the
    // render-time tool-group fold stops a group at the slice edge; the fold emits
    // only `[hw..]`.
    let items: Vec<_> = screen
        .transcript()
        .items()
        .iter()
        .take(hw + count)
        .cloned()
        .collect();
    let committed = components::CommittedSlice {
        cache,
        items: &items,
        hw,
        count,
        theme,
    };
    let height = components::commit_slice_height(&committed, content_width);
    if height > 0 {
        // FATAL on error (`?`): a failed blit must not leave the mark advanced,
        // so we advance it only on the `Ok` path below.
        terminal.insert_before(height, |buf| {
            components::render_committed_slice(buf, &committed);
        })?;
    }
    // Only now - after a successful blit (or a zero-height no-op) - advance the
    // pure core's high-water mark so a later fold never re-freezes this slice.
    screen.mark_committed(count);
    Ok(())
}

/// Re-applies compact mode over already-frozen scrollback (ADR-0052, the sibling
/// of [`commit_items`]): syncs the render cache to the new compact and repaints
/// the live inline viewport so the toggle takes hold cleanly, WITHOUT resetting
/// the high-water mark (the frozen prefix stays committed).
///
/// SPIKE RESULT (Risk #1, the HIGH one). qwen's faithful `refreshStatic` =
/// `clearTerminal` (emit `\x1b[2J\x1b[3J\x1b[H`, wiping screen AND scrollback)
/// then replay every committed item at the new compact. Two findings killed the
/// faithful port here. First, `Terminal::clear()` on an `Inline` viewport clears
/// only from the viewport top downwards (`ClearType::AfterCursor`) - it does NOT
/// touch the frozen rows already in native scrollback, so `clear()` + a full
/// re-`insert_before` DOUBLES the committed rows (old ones stay above, fresh ones
/// push the viewport down). Second, ratatui's `backend::ClearType` has no `Purge`
/// (`\x1b[3J`) variant, so there is NO PORTABLE way through the `Backend` trait to
/// wipe native scrollback - and `TestBackend` cannot model a scrollback purge at
/// all.
///
/// Per the Phase-6 design's directive ("if it doubles/orphans rows, use the
/// degraded viewport-only fallback rather than shipping broken scrollback"), this
/// is the DEGRADED fallback: the pending region (and every FUTURE commit) renders
/// at the new compact, but the frozen prefix above the fold keeps the compact it
/// was blitted at. A `terminal.clear()` repaints the live viewport so the flip is
/// immediate and artifact-free; the next `draw` fills it at the new compact.
///
/// A no-op-safe path on a non-inline backend (`TestBackend` without `Inline`):
/// `clear()` there just resets the buffer, and the cache re-sync is harmless.
fn redraw_scrollback<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Screen,
    cache: &mut components::RenderCache,
    theme: &theme::Theme,
) -> anyhow::Result<()> {
    // Rebuild the cache at the new compact so the very next `draw` measures and
    // paints the pending region correctly (the cache keys on the compact toggle,
    // so this is the wholesale rebuild `needs_rebuild` triggers).
    let width = terminal
        .size()
        .map(|s| s.width)
        .unwrap_or(FALLBACK_TERM_WIDTH);
    let content_width = width.saturating_sub(2 * components::CONTENT_MARGIN);
    components::sync_commit_cache(cache, screen, content_width, theme);
    // Repaint the live viewport (degraded fallback: the frozen scrollback above
    // cannot be un-drawn portably). The high-water mark is deliberately left
    // UNCHANGED - the committed prefix is still committed.
    terminal.clear()?;
    Ok(())
}

/// The Ctrl-S peek (ADR-0046, [`Effect::PeekPending`]): blits the FULL, UNCLAMPED
/// pending body into native scrollback via `insert_before`, above the live inline
/// viewport, so the user can scroll up to read the rows the viewport top-clips
/// away ("… Ctrl-S to show more"). The fixed inline viewport cannot grow, so the
/// clipped rows are revealed ABOVE the live region rather than in place.
///
/// A PEEK, NOT a commit: unlike [`commit_items`], this does NOT advance the
/// high-water mark and freezes NOTHING - the same body (clipped) redraws in the
/// live viewport on the next `draw`. The blit lands in scrollback purely for the
/// user to scroll back to. It reads `screen` immutably; nothing changes state.
///
/// A no-op-safe path on a non-inline backend (`TestBackend` without `Inline`):
/// `insert_before` there is a documented no-op, so headless tests stay valid. A
/// zero-height body (nothing pending) also no-ops - there is nothing to peek.
///
/// [`Effect::PeekPending`]: crate::ui::screen::Effect::PeekPending
fn peek_pending<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &Screen,
    cache: &mut components::RenderCache,
    theme: &theme::Theme,
) -> anyhow::Result<()> {
    let width = terminal
        .size()
        .map(|s| s.width)
        .unwrap_or(FALLBACK_TERM_WIDTH);
    let content_width = width.saturating_sub(2 * components::CONTENT_MARGIN);
    // The peek reads the SAME line set the live body draws (via `pending_body_lines`
    // at the live high-water mark), so it syncs the cache at the same content width
    // (measure == draw, ADR-0029). `Anim::default()` is fine: the spinner FRAME is
    // irrelevant to a static scrollback snapshot the user reads at rest.
    let mut peek = components::PendingPeek {
        cache,
        screen,
        anim: components::Anim::default(),
        theme,
    };
    let height = components::pending_peek_height(&mut peek, content_width);
    if height > 0 {
        terminal.insert_before(height, |buf| {
            components::render_pending_peek(buf, &mut peek);
        })?;
    }
    Ok(())
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

/// After the Agent is gone we keep the TUI responsive to quit only (native
/// scrollback owns history now, so there is nothing left to scroll). The Active
/// Model can no longer change (no Agent to swap it), so the connection facts are
/// frozen - carried in [`FrozenFrame`]. Repaints the pending region on
/// resize/read-error so the frozen frame stays coherent.
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
        // Ctrl-S (qwen `ShowMoreLines`): peek the full, unclamped pending body into
        // scrollback (ADR-0046). BEFORE the generic Ctrl-chord/Char arms so it is
        // named intent, not a typed 's'.
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

/// Carries out the Effects the pure core returned (minus [`Effect::Commit`],
/// which the loop's [`dispatch`] freezes into scrollback first), threading the
/// Screen through the Agent retries (submit↔steer) the core asks for. `state`
/// is the ONE mutable adapter-state carrier ([`AdapterState`], the mutable twin
/// of [`AdapterCtx`]): the Theme state a `/theme` pick swaps and the history
/// path the appends write through.
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
// the handler below. [`Effect::Commit`] is handled a level up in [`dispatch`]
// (it needs the terminal), so it never reaches here.
async fn run_effect(
    screen: Screen,
    effect: Effect,
    ctx: &AdapterCtx<'_>,
    state: &mut AdapterState,
) -> Screen {
    match effect {
        Effect::Agent(command) => run_agent_command(command, screen, ctx, state).await,
        // The terminal-owning `dispatch` freezes commits: the pre-effect ones up
        // front, and a TRAILING one after `run_effects` for any prefix the
        // submit/steer/info outcome hooks marked committable. A `Commit` that
        // surfaces INSIDE this recursion (from those hooks) is a no-op here - it
        // is re-derived and frozen by that trailing pass - so the mark never
        // advances mid-recursion and the freeze stays atomic (ADR-0046).
        Effect::Commit { .. } => screen,
        // `RedrawScrollback` (ADR-0052) needs the terminal to wipe + re-blit the
        // committed slice, so like `Commit` it is handled a level up in
        // `dispatch`; a stray one inside this recursion is a no-op.
        Effect::RedrawScrollback => screen,
        // `PeekPending` (ADR-0046, Ctrl-S) needs the terminal to `insert_before`
        // the full body, so like `Commit`/`RedrawScrollback` it is handled a level
        // up in `dispatch`; a stray one inside this recursion is a no-op.
        Effect::PeekPending => screen,
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

/// Draws one inline PENDING frame (ADR-0046): the render path syncs the
/// [`components::RenderCache`] (settled items build once, per width) and draws
/// only the uncommitted tail (the settled items past the store's high-water
/// mark, plus the live stream), bottom-anchored and top-clipped, with the status
/// bar and Composer below - all inside [`components::render_pending`]. Committed
/// history was already frozen into native scrollback by [`commit_items`]. The
/// per-frame connection/anim/theme travel as one [`components::FrameCtx`].
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

    // END-TO-END inline freeze (ADR-0046): driving `dispatch` with a fold that
    // commits must (a) blit the committed slice into the TestBackend's native
    // SCROLLBACK via `insert_before`, and (b) leave that item OUT of the pending
    // region on the next draw. This is the whole point of the seam - the item
    // moves from the live frame to frozen scrollback - and it is headless-
    // testable because `Terminal::insert_before` works under `TestBackend`
    // (via `append_lines`) when the viewport is `Inline`.
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_commit_freezes_the_slice_into_scrollback_and_drops_it_from_pending() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();
        let mut cache = components::RenderCache::new();

        // An INLINE TestBackend, deliberately SHORT (a 3-row terminal with a
        // 1-row inline viewport) so a multi-row committed slice overflows the top
        // of the terminal buffer and lands in the backend's `scrollback()`.
        let mut terminal = Terminal::with_options(
            TestBackend::new(30, 3),
            TerminalOptions {
                viewport: TermViewport::Inline(1),
            },
        )
        .unwrap();

        // A fresh Screen opens with the startup Header banner, which spans
        // several rows at this narrow width - so the committed slice overflows
        // the 3-row terminal and its top rows reach the backend's scrollback.
        // run_started makes the whole leading prefix terminal, so the fold emits
        // Effect::Commit.
        let core = Screen::new(ScreenOpts::default());
        let (core, effects) = core.apply_event(Event::run_started("r1"));
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Commit { .. })),
            "the fold emitted a Commit to freeze"
        );

        let core = dispatch(
            &mut terminal,
            core,
            effects,
            &mut Adapter {
                ctx: &ctx,
                state: &mut state,
                cache: &mut cache,
            },
        )
        .await
        .expect("dispatch freezes without error");

        // (a) The committed header landed in NATIVE SCROLLBACK (above the inline
        // viewport), not in the live viewport buffer.
        let scrollback: String = {
            let sb = terminal.backend().scrollback();
            (0..sb.area.height)
                .map(|y| {
                    (0..sb.area.width)
                        .map(|x| sb.cell((x, y)).expect("cell").symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            scrollback.contains(">_ suspenders"),
            "the committed header froze into native scrollback:\n{scrollback}"
        );

        // The adapter advanced the high-water mark ONLY after the successful blit
        // (transactional commit): the header is now committed.
        assert!(
            core.transcript().committed_high_water() >= 1,
            "the mark advanced post-blit"
        );

        // (b) The committed slice has LEFT the pending region: drawing the live
        // frame now shows the pending body WITHOUT it (it is committed, drawn from
        // scrollback on a real TTY). Grow the inline viewport first so the body +
        // status + composer layout has room to render.
        terminal.backend_mut().resize(30, 8);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 30, 6))
            .unwrap();
        let conn = components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };
        draw(
            &mut terminal,
            &core,
            &mut cache,
            components::FrameCtx {
                conn: conn.view(),
                anim: components::Anim::default(),
                theme: theme::dark(),
            },
        )
        .unwrap();
        let pending: String = {
            let buf = terminal.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf.cell((x, y)).expect("cell").symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            !pending.contains(">_ suspenders"),
            "the committed header is gone from the pending region:\n{pending}"
        );
    }

    // FIRST-FRAME bug (ADR-0046): on startup the header must be committed to
    // native scrollback UP FRONT, so frame 1 already shows the composer +
    // status - WITHOUT any keypress. This mirrors `run_loop`'s three-beat first
    // frame (draw to establish the inline viewport, startup `dispatch` with an
    // EMPTY effect vector whose trailing-freeze flushes the committable notices,
    // then a clean pending redraw). Before the fix the initial draw painted the
    // header still uncommitted and the composer/status only appeared after the
    // first key triggered the deferred commit.
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_commits_header_and_frame_one_shows_composer_and_status() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();
        let mut cache = components::RenderCache::new();

        // An INLINE TestBackend that is SHORT (a small terminal with a 5-row
        // inline viewport) so the committed header overflows the top of the
        // terminal buffer and lands in the backend's scrollback, while the
        // viewport still fits the status bar + composer chrome.
        let mut terminal = Terminal::with_options(
            TestBackend::new(40, 6),
            TerminalOptions {
                viewport: TermViewport::Inline(5),
            },
        )
        .unwrap();

        let conn = components::ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };
        let anim = components::Anim::default();

        // A launch Screen: the startup Header banner (terminal, hence
        // committable) with the high-water mark at 0.
        let mut screen = Some(Screen::new(ScreenOpts::default()));
        assert_eq!(
            screen.as_ref().unwrap().transcript().committed_high_water(),
            0,
            "nothing committed before the startup pass"
        );
        let committable = screen.as_ref().unwrap().transcript().committable_upto();
        assert!(committable >= 1, "the header is committable at launch");

        // Beat 1: establish the inline viewport.
        draw_previewed(
            &mut terminal,
            screen.as_ref().unwrap(),
            &conn,
            anim,
            &mut cache,
            &state,
        )
        .unwrap();

        // Beat 2: the startup commit - an EMPTY effect vector, so ONLY dispatch's
        // trailing freeze runs, flushing exactly the committable notices.
        screen = Some(
            dispatch(
                &mut terminal,
                screen.take().unwrap(),
                Vec::new(),
                &mut Adapter {
                    ctx: &ctx,
                    state: &mut state,
                    cache: &mut cache,
                },
            )
            .await
            .expect("startup dispatch freezes without error"),
        );

        // Beat 3: the clean pending redraw.
        draw_previewed(
            &mut terminal,
            screen.as_ref().unwrap(),
            &conn,
            anim,
            &mut cache,
            &state,
        )
        .unwrap();

        let core = screen.as_ref().unwrap();

        // The startup pass advanced the high-water mark past the notices - the
        // header is now committed, with NO simulated keypress.
        assert_eq!(
            core.transcript().committed_high_water(),
            committable,
            "the startup commit advanced the mark past the committable notices"
        );

        // (a) The header froze into NATIVE SCROLLBACK.
        let scrollback: String = {
            let sb = terminal.backend().scrollback();
            (0..sb.area.height)
                .map(|y| {
                    (0..sb.area.width)
                        .map(|x| sb.cell((x, y)).expect("cell").symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            scrollback.contains(">_ suspenders"),
            "frame 1: the header is in scrollback:\n{scrollback}"
        );

        // (b) The live viewport already shows the composer placeholder AND footer
        // content on frame 1 - no keypress needed.
        let viewport = buffer_text(&terminal);
        assert!(
            viewport.contains("Type your message"),
            "frame 1: the composer placeholder is drawn:\n{viewport}"
        );
        assert!(
            viewport.contains("model m") && viewport.contains("? for shortcuts"),
            "frame 1: the flat footer (model fact + shortcuts hint) is drawn:\n{viewport}"
        );
        // The header is NOT re-drawn in the pending region (it is committed).
        assert!(
            !viewport.contains(">_ suspenders"),
            "frame 1: the committed header is gone from the pending region:\n{viewport}"
        );
    }

    // Ctrl-S peek (BUG 1, ADR-0046): driving `dispatch` with a `PeekPending`
    // effect must (a) blit the FULL pending body into the TestBackend's native
    // SCROLLBACK via `insert_before`, and (b) leave the high-water mark UNCHANGED -
    // it is a non-committing peek, so nothing freezes and the same body redraws
    // (clipped) in the live viewport next frame.
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_peek_pending_blits_the_full_body_and_keeps_the_mark() {
        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();
        let mut cache = components::RenderCache::new();

        // A SHORT inline terminal so the pending body overflows the live viewport
        // and the peek's blit reaches the backend's scrollback.
        let mut terminal = Terminal::with_options(
            TestBackend::new(60, 3),
            TerminalOptions {
                viewport: TermViewport::Inline(1),
            },
        )
        .unwrap();

        // First settle the transcript so the peek is the ONLY thing dispatch can
        // do: a live (un-resulted) `run_command` tool call is NON-terminal, so
        // `committable_upto` stops before it and dispatch's trailing commit is a
        // no-op - isolating the peek's effect on the mark. The header + call are
        // committed up front so the pending body is a stable, live tool group.
        let core = Screen::new(ScreenOpts::default());
        let (core, _) = core.apply_event(Event::run_started("r1"));
        let (core, _) = core.apply_event(Event::tool_call(
            "t1",
            "run_command",
            serde_json::json!({"command": "echo peek-me"}),
        ));
        // Freeze everything committable (the header), leaving the live call
        // pending. Do it through dispatch so the mark reflects real adapter state.
        let prime = core.transcript().committable_upto();
        let core = dispatch(
            &mut terminal,
            core,
            vec![Effect::Commit { count: prime }],
            &mut Adapter {
                ctx: &ctx,
                state: &mut state,
                cache: &mut cache,
            },
        )
        .await
        .expect("prime commit succeeds");

        let mark_before = core.transcript().committed_high_water();
        let (core, effects) = core.handle_key(crate::ui::screen::Key::ShowMore);
        assert_eq!(
            effects,
            vec![Effect::PeekPending],
            "Ctrl-S minted exactly the peek"
        );

        let core = dispatch(
            &mut terminal,
            core,
            effects,
            &mut Adapter {
                ctx: &ctx,
                state: &mut state,
                cache: &mut cache,
            },
        )
        .await
        .expect("dispatch peeks without error");

        // (a) The FULL pending body landed via `insert_before`: its rows scroll up
        // from the live viewport into native SCROLLBACK (rows past the top of the
        // short terminal). Capture the scrollback AND the live buffer so the check
        // is robust to exactly where the short terminal's boundary falls.
        let render = |area: ratatui::layout::Rect, cell: &dyn Fn(u16, u16) -> String| -> String {
            (0..area.height)
                .map(|y| (0..area.width).map(|x| cell(x, y)).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let scrollback = {
            let sb = terminal.backend().scrollback();
            render(sb.area, &|x, y| {
                sb.cell((x, y)).expect("cell").symbol().to_string()
            })
        };
        let live = {
            let buf = terminal.backend().buffer();
            render(buf.area, &|x, y| {
                buf.cell((x, y)).expect("cell").symbol().to_string()
            })
        };
        let combined = format!("{scrollback}\n{live}");
        assert!(
            combined.contains("echo peek-me"),
            "the full pending body was peeked into scrollback + viewport:\n{combined}"
        );

        // (b) The high-water mark is UNCHANGED - the peek committed NOTHING.
        assert_eq!(
            core.transcript().committed_high_water(),
            mark_before,
            "a peek does not advance the high-water mark (non-committing)"
        );
    }

    // RedrawScrollback (ADR-0052): a compact toggle over frozen scrollback must
    // (a) leave the high-water mark UNCHANGED (the committed prefix stays
    // committed), and (b) re-sync the render cache to the new compact so the very
    // next pending draw hides the committed-then-uncommitted thought. The SPIKE
    // (see `redraw_scrollback`) established that native scrollback above the fold
    // cannot be un-drawn portably (the degraded fallback); this pins the parts
    // that ARE correct - the mark and the fresh pending render.
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_redraw_scrollback_keeps_the_mark_and_resyncs_to_compact() {
        use crate::llm::Delta;
        use crate::view_model::TranscriptItem;

        let dir = TempDir::new().unwrap();
        let agent = start_agent(&dir, FakeLlm::script(vec![Entry::just(end_turn("hi"))]));
        let ctx = adapter_ctx(&agent);
        let mut state = test_state();
        let mut cache = components::RenderCache::new();
        let mut terminal = Terminal::with_options(
            TestBackend::new(60, 8),
            TerminalOptions {
                viewport: TermViewport::Inline(3),
            },
        )
        .unwrap();

        // Build a committed thought purely (the multi-dispatch stream is exercised
        // elsewhere): stream + settle a thought, then freeze the terminal prefix as
        // `commit_items` would. The pure `handle_key` then flips compact and mints
        // the RedrawScrollback whose ADAPTER handling this test pins.
        let core = Screen::new(ScreenOpts::default());
        let (core, _) = core.apply_event(Event::message_start(1));
        let (core, _) = core.apply_event(Event::message_update(
            Delta::Thinking("thinking".into()),
            vec![crate::content::ContentBlock::Thinking {
                text: "a secret thought".into(),
            }],
        ));
        let (mut core, _) = core.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        core.mark_committed(core.transcript().committable_upto());
        assert!(
            core.transcript()
                .items()
                .iter()
                .any(|i| matches!(i, TranscriptItem::Thinking { .. })),
            "a committed Thinking item exists"
        );
        let mark_before = core.transcript().committed_high_water();
        assert!(mark_before >= 1, "the thought committed");

        // Ctrl+O: the fold flips compact and mints RedrawScrollback.
        let (core, effects) = core.handle_key(crate::ui::screen::Key::ToggleCompact);
        assert!(core.compact_mode, "compact is now on");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::RedrawScrollback)),
            "the toggle minted a scrollback redraw: {effects:?}"
        );

        let core = dispatch(
            &mut terminal,
            core,
            effects,
            &mut Adapter {
                ctx: &ctx,
                state: &mut state,
                cache: &mut cache,
            },
        )
        .await
        .expect("redraw dispatches without error");

        // (a) The high-water mark is UNCHANGED - the prefix stays committed.
        assert_eq!(
            core.transcript().committed_high_water(),
            mark_before,
            "RedrawScrollback must not move the high-water mark"
        );
        // (b) The redraw actually re-synced the CACHE to the new compact: blitting
        // the committed prefix from the re-synced cache now renders the secret
        // thought as ZERO lines (compact hides a Thinking item entirely, like
        // `cache_sync_rebuilds_when_compact_hides_a_thought`). Rendering FROM THE
        // CACHE (not re-asserting the pure predicate) proves the adapter's
        // `sync_commit_cache` took effect.
        let hw = core.transcript().committed_high_water();
        let items: Vec<_> = core.transcript().items().iter().take(hw).cloned().collect();
        let theme = state.themes.active().clone();
        let slice = components::CommittedSlice {
            cache: &cache,
            items: &items,
            hw: 0,
            count: hw,
            theme: &theme,
        };
        let height = components::commit_slice_height(&slice, 60).max(1);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 60, height));
        components::render_committed_slice(&mut buf, &slice);
        let text: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            !text.contains("a secret thought"),
            "the re-synced cache blits the committed thought as 0 lines under compact:\n{text}"
        );
    }
}
