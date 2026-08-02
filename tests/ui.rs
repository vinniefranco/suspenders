
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

async fn drain(terminal: &mut Terminal<TestBackend>, events: InputEvents) -> anyhow::Result<()> {
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
