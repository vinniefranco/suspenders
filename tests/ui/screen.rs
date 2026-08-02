use super::*;
use crate::content::ContentBlock;
use crate::event::Stage;
use crate::view_model::Tone;
use crate::view_model::TranscriptItem;
use std::collections::HashMap;

impl UngatedKey {
    /// Test-only mint, so Composer unit tests can fold keys without standing
    /// up a Screen. Production code has exactly one mint: the gate.
    pub(crate) fn for_test(key: Key) -> Self {
        UngatedKey(key)
    }
}

// --- helpers mirroring transcript_test.exs -----------------------------

fn fresh() -> Screen {
    Screen::new(ScreenOpts::default())
}

fn fresh_opts(opts: ScreenOpts) -> Screen {
    Screen::new(opts)
}

// Drops a trailing [`Effect::Commit`] (ADR-0046) so a fold's OWN effects
// can be asserted without threading the commit-seam count through every
// pre-existing effect test. A fresh Screen opens with an uncommitted
// header, so the first public fold exit legitimately appends a
// `Commit { count }`; the seam has its own dedicated tests below, and these
// orthogonal assertions strip it. Only ever drops from the END (the seam
// appends there) and only a Commit, so a mislaid effect still fails.
fn sans_commit(mut effects: Vec<Effect>) -> Vec<Effect> {
    if matches!(effects.last(), Some(Effect::Commit { .. })) {
        effects.pop();
    }
    effects
}

// Runs events through the fold, discarding effects.
fn fold(mut t: Screen, events: Vec<Event>) -> Screen {
    for event in events {
        let (next, _effects) = t.apply_event(event);
        t = next;
    }
    t
}

// Folds keys through handle_key, discarding effects.
fn press(mut t: Screen, keys: Vec<Key>) -> Screen {
    for key in keys {
        let (next, _effects) = t.handle_key(key);
        t = next;
    }
    t
}

// The key presses that type `text` into the Composer.
fn typed(text: &str) -> Vec<Key> {
    text.chars().map(Key::Char).collect()
}

// A PendingApproval as `apply_event(ApprovalRequest)` builds it with no live
// ToolCall in the transcript: ConfirmKind falls back to `Info` and the radio
// is a fresh 3-row SelectionList. Tests that compare `pending_approval`
// against this must open the modal the same way (`with_pending_approval`).
fn approval_with(command: &str) -> PendingApproval {
    PendingApproval {
        approval_id: format!("ref-{command}"),
        command: command.to_string(),
        kind: ConfirmKind::Info,
        selection: SelectionList::new(APPROVAL_OPTION_COUNT),
    }
}

fn approval() -> PendingApproval {
    approval_with("mix test")
}

fn with_pending_approval(t: Screen, a: &PendingApproval) -> Screen {
    let (t, _effects) = t.apply_event(Event::ApprovalRequest {
        approval_id: a.approval_id.clone(),
        command: a.command.clone(),
    });
    t
}

// items/1: everything after the header line.
fn items(t: &Screen) -> Vec<TranscriptItem> {
    t.transcript().items().iter().skip(1).cloned().collect()
}

// Asserts that pressing `key` while the approval modal is open produces no
// effects and leaves the pending approval untouched. Shared by the modal
// swallow tests so the loop shape is written once.
fn assert_key_swallowed_while_modal_open(key: Key) {
    let label = format!("{key:?}");
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let pending_before = t.pending_approval.clone();
    let (t, effects) = t.handle_key(key);
    assert_eq!(effects, vec![], "expected no effects for {label}");
    assert_eq!(
        t.pending_approval, pending_before,
        "pending approval changed for {label}"
    );
}

fn user(text: &str) -> TranscriptItem {
    TranscriptItem::User { text: text.into() }
}
fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::Assistant { text: text.into() }
}
fn thinking(text: &str) -> TranscriptItem {
    TranscriptItem::Thinking { text: text.into() }
}
fn info(text: &str) -> TranscriptItem {
    TranscriptItem::Info { text: text.into() }
}
fn marker(text: &str, tone: Tone) -> TranscriptItem {
    TranscriptItem::Marker {
        text: text.into(),
        tone,
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}
fn thinking_block(text: &str) -> ContentBlock {
    ContentBlock::Thinking { text: text.into() }
}

// --- new/1 -------------------------------------------------------------

#[test]
fn new_opens_with_the_startup_header_and_idle_status() {
    let t = fresh_opts(ScreenOpts {
        context_budget: Some(32_000),
        header: HeaderFacts {
            version: "1.2.3".into(),
            model: "openrouter/qwen3-coder".into(),
            cwd: "/home/dev/proj".into(),
            tip_seed: 0,
        },
        ..Default::default()
    });
    assert_eq!(t.transcript().items().len(), 1);
    match &t.transcript().items()[0] {
        TranscriptItem::Header {
            title,
            version,
            model,
            cwd,
            tip,
        } => {
            assert_eq!(title, "suspenders");
            assert_eq!(version, "1.2.3");
            assert_eq!(model, "openrouter/qwen3-coder");
            assert_eq!(cwd, "/home/dev/proj");
            // Seed 0 picks the first registry tip.
            assert_eq!(tip, STARTUP_TIPS[0]);
        }
        other => panic!("expected startup header, got {other:?}"),
    }
    assert_eq!(t.status, Status::Idle);
    assert_eq!(t.context_budget, Some(32_000));
    assert_eq!(t.pending_approval, None);
    assert!(
        t.transcript().streaming_text().is_empty()
            && t.transcript().streaming_thinking().is_empty()
    );
}

// The tip is picked deterministically from the injected seed (the pure core
// has no RNG/clock): the seed wraps the registry by modulo.
#[test]
fn startup_tip_is_seed_indexed_into_the_registry() {
    for seed in 0..(STARTUP_TIPS.len() * 2) {
        assert_eq!(
            pick_startup_tip(seed),
            STARTUP_TIPS[seed % STARTUP_TIPS.len()]
        );
    }
}

#[test]
fn new_records_launch_notices_after_the_header() {
    let t = fresh_opts(ScreenOpts {
        notices: vec![
            "context file .suspenders/SYSTEM.md exists but could not be read \
                 (permission denied); continuing without it"
                .to_string(),
        ],
        ..Default::default()
    });
    assert_eq!(
        items(&t),
        vec![info(
            "context file .suspenders/SYSTEM.md exists but could not be read \
                 (permission denied); continuing without it"
        )]
    );
}

// --- has_live_stream (the render gate's one predicate) ------------------

// The lull/tail gate: a fresh Screen streams nothing, a reasoning delta
// trips the `streaming_thinking` operand, and an answer-text delta trips the
// `streaming_text` operand - both `||` arms covered.
#[test]
fn has_live_stream_tracks_reasoning_and_answer_streams() {
    // Fresh: nothing on the wire.
    assert!(!fresh().has_live_stream(), "a fresh Screen streams nothing");

    // A reasoning delta => the thinking arm holds.
    let thinking_stream = fold(
        fresh(),
        vec![
            Event::run_started("r1"),
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Thinking("half a thought".into()),
                vec![thinking_block("half a thought")],
            ),
        ],
    );
    assert!(
        thinking_stream.has_live_stream(),
        "a streaming reasoning delta is a live stream"
    );

    // An answer-text delta => the text arm holds.
    let text_stream = fold(
        fresh(),
        vec![
            Event::run_started("r1"),
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Text("half an ans".into()),
                vec![text_block("half an ans")],
            ),
        ],
    );
    assert!(
        text_stream.has_live_stream(),
        "a streaming answer delta is a live stream"
    );
}

// --- streaming (the arms; the materialize rules live with the store) ----

#[test]
fn run_started_marks_running_and_clears_snapshot() {
    let t = fold(
        fresh(),
        vec![
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Text("stale".into()),
                vec![text_block("stale")],
            ),
        ],
    );
    let (t, effects) = t.apply_event(Event::run_started("r1"));
    assert_eq!(t.status, Status::Running);
    assert!(
        t.transcript().streaming_text().is_empty()
            && t.transcript().streaming_thinking().is_empty()
    );
    // No PinBottom (ADR-0046); the header may still commit here.
    assert_eq!(sans_commit(effects), vec![]);
}

// --- run_finished -----------------------------------------------------

#[test]
fn run_finished_flushes_snapshot_goes_idle_records_estimate_and_budget() {
    let t = fold(
        fresh_opts(ScreenOpts {
            context_budget: Some(100),
            ..Default::default()
        }),
        vec![
            Event::run_started("r1"),
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Text("Done.".into()),
                vec![text_block("Done.")],
            ),
            Event::RunFinished {
                stop_reason: StopReason::EndTurn,
                token_estimate: 42,
                context_budget: 32_000,
            },
        ],
    );
    assert_eq!(t.status, Status::Idle);
    assert_eq!(t.token_estimate, Some(42));
    assert_eq!(t.context_budget, Some(32_000));
    assert_eq!(items(&t), vec![assistant("Done.")]);
}

// baud keeps the previous budget when the event lacks one. In the Rust
// Event, RunFinished always carries a budget; the Agent forwards the live
// budget it holds. We reproduce baud's assertion by emitting the same
// budget the Screen was opened with (the Agent's live value).
#[test]
fn run_finished_keeps_previous_budget_when_event_carries_it() {
    let t = fold(
        fresh_opts(ScreenOpts {
            context_budget: Some(100),
            ..Default::default()
        }),
        vec![Event::RunFinished {
            stop_reason: StopReason::EndTurn,
            token_estimate: 0,
            context_budget: 100,
        }],
    );
    assert_eq!(t.context_budget, Some(100));
}

#[test]
fn normal_stop_reason_adds_no_info_abnormal_one_does() {
    let normal = fold(
        fresh(),
        vec![Event::RunFinished {
            stop_reason: StopReason::EndTurn,
            token_estimate: 0,
            context_budget: 0,
        }],
    );
    assert_eq!(items(&normal), vec![]);

    let abnormal = fold(
        fresh(),
        vec![Event::RunFinished {
            stop_reason: StopReason::MaxTokens,
            token_estimate: 0,
            context_budget: 0,
        }],
    );
    assert_eq!(items(&abnormal), vec![info("turn stopped: :max_tokens")]);
}

// --- context pressure --------------------------------------------------

fn pressurized(estimate: u64) -> Screen {
    fold(
        fresh_opts(ScreenOpts {
            context_budget: Some(1200),
            compaction_slack: 0.10,
            ..Default::default()
        }),
        vec![Event::context_pressure(estimate, 1200, 200)],
    )
}

#[test]
fn pressure_updates_estimate_and_budget_live() {
    let t = pressurized(500);
    assert_eq!(t.token_estimate, Some(500));
    assert_eq!(t.context_budget, Some(1200));
}

#[test]
fn pressure_ok_below_low_water() {
    assert_eq!(pressurized(0).pressure_level, PressureLevel::Ok);
    assert_eq!(pressurized(500).pressure_level, PressureLevel::Ok);
    assert_eq!(pressurized(880).pressure_level, PressureLevel::Ok);
}

#[test]
fn pressure_elevated_between_low_water_and_target() {
    assert_eq!(pressurized(881).pressure_level, PressureLevel::Elevated);
    assert_eq!(pressurized(950).pressure_level, PressureLevel::Elevated);
    assert_eq!(pressurized(1000).pressure_level, PressureLevel::Elevated);
}

#[test]
fn pressure_critical_above_target() {
    assert_eq!(pressurized(1001).pressure_level, PressureLevel::Critical);
    assert_eq!(pressurized(5000).pressure_level, PressureLevel::Critical);
}

#[test]
fn pressure_comes_from_events_live_window_not_new_budget() {
    let t = fold(
        fresh_opts(ScreenOpts {
            context_budget: Some(100),
            compaction_slack: 0.0,
            ..Default::default()
        }),
        vec![Event::context_pressure(1500, 2000, 200)],
    );
    assert_eq!(t.context_budget, Some(2000));
    assert_eq!(t.pressure_level, PressureLevel::Ok);
}

// --- Approval lifecycle ------------------------------------------------

#[test]
fn approval_request_stores_pending_and_focuses_modal() {
    let a = approval_with("rm -rf ./tmp");
    let (t, effects) = fresh().apply_event(Event::ApprovalRequest {
        approval_id: a.approval_id.clone(),
        command: a.command.clone(),
    });
    assert_eq!(t.pending_approval, Some(a));
    assert_eq!(sans_commit(effects), vec![Effect::FocusModal]);
}

// P2: a `run_command` approval derives `ConfirmKind::Exec` (not the Info
// fallback the bare `approval_with` helper hard-codes). The kind comes from
// the newest live ToolCall's name (ADR-0049), so we must emit that call
// first, then the ApprovalRequest. This proves the exec question path.
#[test]
fn a_run_command_approval_derives_confirm_kind_exec() {
    let t = fold(
        fresh(),
        vec![
            Event::run_started("r1"),
            Event::tool_call(
                "t1",
                "run_shell_command",
                serde_json::json!({"command": "cargo test"}),
            ),
            Event::approval_request("approval-0", "cargo test"),
        ],
    );
    let pending = t.pending_approval.as_ref().expect("an open approval");
    // Exec (not the Info fallback): this is the kind the render reads to draw
    // the `Allow execution of: '{command}'?` question (ADR-0049), so the exec
    // question path is exercised - not only the Info fallback the other
    // Screen tests hard-code.
    assert_eq!(pending.kind, ConfirmKind::Exec);
    assert_eq!(pending.command, "cargo test");
}

#[test]
fn y_approves_clears_and_refocuses() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Char('y'));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Approve)),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn n_denies() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Char('n'));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn a_approves_always_clears_and_refocuses() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Char('a'));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(
                a.approval_id,
                Decision::ApproveAlways
            )),
            Effect::FocusComposer,
        ]
    );
}

// Escape while an Approval is open DENIES this tool and the Run continues
// (P1, qwen `ToolConfirmationMessage.tsx:106-114`; matches the
// `No, suggest changes (esc)` label) - it does NOT cancel the Run.
#[test]
fn escape_while_modal_open_denies_the_tool_not_the_run() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Escape);
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
            Effect::FocusComposer,
        ]
    );
}

// The counterpart (P1): with NO approval open and a Run streaming, Escape
// STILL cancels the whole Run (qwen's `esc to cancel` spinner + suspenders'
// global cancel). This behavior is unchanged.
#[test]
fn escape_while_streaming_without_an_approval_cancels_the_run() {
    let mut t = fresh();
    t.status = Status::Running;
    assert!(t.pending_approval.is_none());
    let (_t, effects) = t.handle_key(Key::Escape);
    assert_eq!(
        sans_commit(effects),
        vec![Effect::Agent(AgentCommand::Cancel)]
    );
}

// Keys the radio does not act on (non-digit chars, page keys) are swallowed
// with no effect and no change to the pending Approval. Enter and the arrows
// are NOT here - they now drive the radio (asserted below).
#[test]
fn every_other_key_swallowed_while_modal_open() {
    for key in [Key::Char('x'), Key::PageUp, Key::PageDown, Key::Char('q')] {
        assert_key_swallowed_while_modal_open(key);
    }
}

// The inline radio (ADR-0049): Enter selects the active row (option 0,
// Approve, by default), so it resolves the Approval and refocuses.
#[test]
fn enter_selects_the_active_radio_row_which_is_approve_once() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Approve)),
            Effect::FocusComposer,
        ]
    );
}

// ArrowDown moves the radio to row 1 (Always allow); Enter there resolves as
// ApproveAlways. The move itself emits no effect but changes the selection.
#[test]
fn arrow_down_then_enter_selects_approve_always() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, moved) = t.handle_key(Key::ArrowDown);
    assert_eq!(sans_commit(moved), vec![], "a move emits no effect");
    assert_eq!(
        t.pending_approval.as_ref().unwrap().selection.active(),
        1,
        "the radio moved to row 1"
    );
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(
                a.approval_id,
                Decision::ApproveAlways
            )),
            Effect::FocusComposer,
        ]
    );
}

// The numbered digits quick-select (3 rows, so a digit always resolves
// immediately): `2` → Always allow (row 1, ApproveAlways), `3` → No/Deny.
#[test]
fn digit_two_quick_selects_approve_always() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Char('2'));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(
                a.approval_id,
                Decision::ApproveAlways
            )),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn digit_three_quick_selects_deny() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    let (t, effects) = t.handle_key(Key::Char('3'));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
            Effect::FocusComposer,
        ]
    );
}

// Shift+Tab (Key::CycleApprovalMode) fires the cycle command through the
// Agent - even with an Approval open it does NOT disturb the pending block,
// and with no Approval open it still fires.
#[test]
fn cycle_approval_mode_key_emits_the_cycle_command() {
    let (t, effects) = fresh().handle_key(Key::CycleApprovalMode);
    assert_eq!(
        sans_commit(effects),
        vec![Effect::Agent(AgentCommand::CycleApprovalMode)]
    );
    assert_eq!(t.pending_approval, None);
}

// The host-driven expire seam (ADR-0049): with the 3-row approval no digit
// ever buffers, so expire_approval is a no-op - the block stays open and no
// command fires however far the clock advances.
#[test]
fn expire_approval_is_a_no_op_for_the_three_row_radio() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    // A digit press resolves immediately (never buffers), so before any press
    // the buffer is empty and a far-future tick fires nothing.
    let (t, effects) = t.expire_approval(10_000);
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(t.pending_approval, Some(a));
}

// With no Approval open, expire is inert.
#[test]
fn expire_approval_with_no_pending_is_inert() {
    let (t, effects) = fresh().expire_approval(10_000);
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(t.pending_approval, None);
}

// The mirror event (ADR-0050): ApprovalModeChanged updates the Screen's
// display-only copy and touches nothing else.
#[test]
fn approval_mode_changed_mirrors_the_mode_silently() {
    let (t, effects) = fresh().apply_event(Event::approval_mode_changed(ApprovalMode::Yolo));
    assert_eq!(t.approval_mode, ApprovalMode::Yolo);
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(items(&t), vec![], "the mirror is never a Transcript item");
}

// Cycling the mode while an Approval is open (a Shift+Tab press) fires the
// command and leaves the pending Approval whole - the block keeps holding the
// keyboard.
#[test]
fn cycling_the_mode_while_the_approval_is_open_leaves_it_pending() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);
    // Shift+Tab is swallowed by the Approval gate (only the radio keys +
    // y/n/a + Escape act), so the block stays open and no command fires.
    let (t, effects) = t.handle_key(Key::CycleApprovalMode);
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(t.pending_approval, Some(a));
}

#[test]
fn approval_auto_appends_standing_info_without_touching_modal() {
    let (t, effects) = fresh().apply_event(Event::approval_auto("mix test"));
    assert_eq!(
        t.transcript().items().last(),
        Some(&info("auto-approved (standing): mix test"))
    );
    assert_eq!(t.pending_approval, None);
    assert_eq!(sans_commit(effects), vec![]);
}

// A bounded re-draw (ADR-0030) is silent to the Conversation but never to
// the operator: one info line names the attempt against the budget.
#[test]
fn a_retry_recedes_one_bounded_redraw_info_line() {
    let (t, effects) = fresh().apply_event(Event::retry("unknown tool", 1, 3));
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(
        items(&t),
        vec![info("malformed tool call - re-drawing (1/3)")]
    );
}

#[test]
fn approval_resolved_clears_only_matching_pending() {
    let a = approval();
    let t = with_pending_approval(fresh(), &a);

    // Stale id: nothing happens (the header's Commit is orthogonal).
    let (t, effects) = t.apply_event(Event::approval_resolved("some-other-ref", true));
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(t.pending_approval, Some(a.clone()));

    // Matching id: cleared, composer refocused.
    let (t, effects) = t.apply_event(Event::approval_resolved(a.approval_id.clone(), true));
    assert_eq!(t.pending_approval, None);
    assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
}

// --- Question modal (ADR-0057, ask_user_question) -----------------------

fn question(header: &str, options: &[&str]) -> Question {
    Question {
        question: format!("Pick for {header}?"),
        header: header.to_string(),
        options: options
            .iter()
            .map(|label| crate::tool::caps::QuestionOption {
                label: label.to_string(),
                description: "desc".to_string(),
            })
            .collect(),
        multi_select: false,
    }
}

fn with_question(t: Screen, id: &str, questions: Vec<Question>) -> Screen {
    let (t, _effects) = t.apply_event(Event::question_request(id, questions));
    t
}

#[test]
fn question_request_stores_pending_and_focuses_modal() {
    let (t, effects) = fresh().apply_event(Event::question_request(
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    ));
    let pending = t.pending_question.as_ref().expect("an open question");
    assert_eq!(pending.question_id, "q-1");
    assert_eq!(pending.cursor, 0);
    // One radio per question, each options + 1 for the auto-"Other" row.
    assert_eq!(pending.per_question.len(), 1);
    assert_eq!(pending.per_question[0].len(), 3);
    assert_eq!(sans_commit(effects), vec![Effect::FocusModal]);
}

#[test]
fn selecting_a_real_option_records_it_and_resolves_a_single_question() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    // Enter selects the active row (row 0 = "serde"); the single question
    // resolves, emitting the answer and refocusing the composer.
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_question, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::AnswerQuestion(
                "q-1".to_string(),
                Ok(vec![(0, "serde".to_string())])
            )),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn a_digit_quick_selects_an_option() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    // Digit '2' selects the second option ("miniserde"); it resolves. The
    // tuple's first element is the QUESTION index (0), not the option index.
    let (t, effects) = t.handle_key(Key::Char('2'));
    assert_eq!(t.pending_question, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::AnswerQuestion(
                "q-1".to_string(),
                Ok(vec![(0, "miniserde".to_string())])
            )),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn two_questions_advance_the_cursor_before_resolving() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![
            question("Library", &["serde", "miniserde"]),
            question("Runtime", &["tokio", "smol"]),
        ],
    );
    // Answer the first question (row 0 = "serde"): the cursor advances, no
    // resolve yet.
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(sans_commit(effects), vec![]);
    let pending = t.pending_question.as_ref().expect("still open");
    assert_eq!(pending.cursor, 1);
    assert_eq!(pending.answers[0], Some("serde".to_string()));
    // Answer the second (row 0 = "tokio"): now it resolves with both answers.
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_question, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::AnswerQuestion(
                "q-1".to_string(),
                Ok(vec![(0, "serde".to_string()), (1, "tokio".to_string())])
            )),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn selecting_other_routes_to_the_composer_and_the_next_submit_fills_it() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    // The auto-"Other" row is the last one (index 2); digit '3' picks it.
    let (mut t, effects) = t.handle_key(Key::Char('3'));
    // It focuses the composer and arms free-form capture, without resolving.
    assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
    assert_eq!(
        t.pending_question.as_ref().unwrap().collecting_other,
        Some(0)
    );
    // The user types a free-form answer into the composer.
    for key in typed("something else") {
        let (next, _e) = t.handle_key(key);
        t = next;
    }
    // Enter (a submit) fills the answer instead of prompting; the single
    // question resolves with the typed text.
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_question, None);
    assert!(
        effects.contains(&Effect::Agent(AgentCommand::AnswerQuestion(
            "q-1".to_string(),
            Ok(vec![(0, "something else".to_string())])
        )))
    );
}

#[test]
fn a_slash_command_during_other_capture_does_not_route_to_the_slash_menu() {
    // Arm "Other" capture, then type a full `/model` slash command and press
    // Enter. Outside capture the composer would fire `Effect::Command` and
    // open the model selector; during capture the question modal MUST swallow
    // that machinery so it never leaks out, and the modal stays open collecting.
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    let (mut t, _e) = t.handle_key(Key::Char('3')); // pick "Other"
    assert_eq!(
        t.pending_question.as_ref().unwrap().collecting_other,
        Some(0)
    );
    // Type the leading `/model` (each keystroke should stay text/swallowed and
    // never route out).
    for key in typed("/model") {
        let (next, effects) = t.handle_key(key);
        t = next;
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Command { .. } | Effect::SelectorChosen { .. })),
            "a slash keystroke during Other capture must not route to a command"
        );
    }
    // Enter (which would commit the slash command outside capture): swallowed.
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(effects, vec![], "the slash-command Enter is swallowed");
    // The modal is still open and still collecting - the command did not fire.
    let pending = t.pending_question.as_ref().expect("modal stays open");
    assert_eq!(pending.collecting_other, Some(0));
    assert_eq!(pending.answers[0], None, "no answer recorded");
}

#[test]
fn arming_other_capture_clears_a_pre_existing_draft() {
    // The user had typed a message before the modal opened; when they pick
    // "Other", that stale draft must NOT leak into the answer (M2). Seed the
    // draft BEFORE the modal opens, while the composer still owns the keyboard.
    let mut t = fresh();
    for key in typed("stale in-progress text") {
        let (next, _e) = t.handle_key(key);
        t = next;
    }
    assert!(!t.composer().view().draft.is_empty(), "draft is seeded");
    let t = with_question(t, "q-1", vec![question("Library", &["serde", "miniserde"])]);
    assert!(
        !t.composer().view().draft.is_empty(),
        "the stale draft survives the modal opening"
    );
    // Pick "Other": the draft is cleared as capture arms.
    let (mut t, _e) = t.handle_key(Key::Char('3'));
    assert_eq!(
        t.pending_question.as_ref().unwrap().collecting_other,
        Some(0)
    );
    assert!(
        t.composer().view().draft.is_empty(),
        "arming Other capture clears the stale draft"
    );
    // Now type + submit the real answer; the stale text does not appear.
    for key in typed("real answer") {
        let (next, _e) = t.handle_key(key);
        t = next;
    }
    let (t, effects) = t.handle_key(Key::Enter);
    assert_eq!(t.pending_question, None);
    assert!(
        effects.contains(&Effect::Agent(AgentCommand::AnswerQuestion(
            "q-1".to_string(),
            Ok(vec![(0, "real answer".to_string())])
        )))
    );
}

#[test]
fn escape_during_other_capture_backs_out_to_the_radio() {
    // Escape while collecting an "Other" answer drops back to the radio: the
    // modal stays open, `collecting_other` resets to None, no answer recorded.
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    let (t, _e) = t.handle_key(Key::Char('3')); // pick "Other"
    assert_eq!(
        t.pending_question.as_ref().unwrap().collecting_other,
        Some(0)
    );
    let (t, effects) = t.handle_key(Key::Escape);
    let pending = t.pending_question.as_ref().expect("modal stays open");
    assert_eq!(pending.collecting_other, None, "back to the radio");
    assert_eq!(pending.answers[0], None, "no answer recorded");
    assert_eq!(sans_commit(effects), vec![], "backing out emits nothing");
}

#[test]
fn an_empty_other_submit_is_a_no_op_that_keeps_collecting() {
    // Submitting an empty "Other" draft records nothing and keeps collecting -
    // it must not resolve the question with an empty answer.
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    let (t, _e) = t.handle_key(Key::Char('3')); // pick "Other"
    let (t, effects) = t.handle_key(Key::Enter); // Enter on an empty draft
    // No answer is emitted (a redraw Commit may fire, but never an
    // AnswerQuestion): the empty submit records nothing and keeps collecting.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Agent(AgentCommand::AnswerQuestion(..)))),
        "an empty submit records no answer"
    );
    let pending = t.pending_question.as_ref().expect("modal stays open");
    assert_eq!(pending.collecting_other, Some(0), "still collecting");
    assert_eq!(pending.answers[0], None, "no empty answer recorded");
}

#[test]
fn escape_declines_the_question_round_trip() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    let (t, effects) = t.handle_key(Key::Escape);
    assert_eq!(t.pending_question, None);
    assert_eq!(
        sans_commit(effects),
        vec![
            Effect::Agent(AgentCommand::AnswerQuestion(
                "q-1".to_string(),
                Err("User declined to answer the questions.".to_string())
            )),
            Effect::FocusComposer,
        ]
    );
}

#[test]
fn arrows_move_the_question_radio_without_resolving() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde", "time"])],
    );
    let (t, effects) = t.handle_key(Key::ArrowDown);
    assert_eq!(sans_commit(effects), vec![], "a move emits nothing");
    assert!(t.pending_question.is_some());
    assert_eq!(
        t.pending_question.as_ref().unwrap().per_question[0].active(),
        1
    );
}

#[test]
fn a_stray_char_is_swallowed_while_the_question_modal_holds_the_keyboard() {
    let t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    let before = t.pending_question.clone();
    let (t, effects) = t.handle_key(Key::Char('x'));
    assert_eq!(effects, vec![], "no effect");
    assert_eq!(
        t.pending_question, before,
        "the draft was not edited; the modal is unchanged"
    );
    // The composer never saw the key (it did not open a slash menu etc.).
    assert!(t.composer().view().draft.is_empty());
}

#[test]
fn a_cancel_clears_the_question_modal() {
    let mut t = with_question(
        fresh(),
        "q-1",
        vec![question("Library", &["serde", "miniserde"])],
    );
    t.status = Status::Running;
    let (t, _e) = t.apply_event(Event::RunCancelled);
    assert_eq!(t.pending_question, None, "a cancel clears the modal");
}

// --- submit / steer outcomes --------------------------------------------
//
// Enter's submit-vs-steer decision lives in the Composer (`ui::composer`,
// ADR-0034); these pin the SEAM - the submitted/steered outcome hooks and
// the retry pair, which stay here because they touch the Transcript and
// the Agent status, and because the draft must survive a retry.

#[test]
fn successful_submit_appends_user_clears_and_records_history() {
    let t = press(fresh(), typed("fix the bug"));
    let (t, effects) = t.submitted("fix the bug", Ok(()));
    assert_eq!(items(&t), vec![user("fix the bug")]);
    assert_eq!(t.composer().view().draft, "");
    // The on-disk HistoryAppend, then the commit seam (ADR-0046): submitted
    // now routes through `with_commit`, so the terminal header + the new
    // User line freeze on THIS exit (count 2), not the next event.
    assert_eq!(
        sans_commit(effects.clone()),
        vec![Effect::HistoryAppend("fix the bug".into())]
    );
    assert_eq!(commit_count(&effects), Some(2));
    // Recorded into the ring through the Composer's hook: Up recalls it.
    let (t, _) = t.handle_key(Key::ArrowUp);
    assert_eq!(t.composer().view().draft, "fix the bug");
}

#[test]
fn busy_submit_retries_as_steering_and_the_draft_survives() {
    let t = press(fresh(), typed("another task"));
    let (t, effects) = t.submitted("another task", Err(Busy));
    assert_eq!(t.status, Status::Running);
    assert_eq!(
        effects,
        vec![Effect::Agent(AgentCommand::Steer("another task".into()))]
    );
    // Only a successful send clears the Composer - the retry must not.
    assert_eq!(t.composer().view().draft, "another task");
}

// --- steering ------------------------------------------------------------
//
// The marker text and the queued→delivered promotion are the store's rule
// (`ui::transcript`); this pins the ARM - queued and delivered are both
// silent of scroll effects now (native scrollback follows the tail), and
// both land in the store.

#[test]
fn steering_events_delegate_and_land_in_the_store() {
    let (t, effects) = fresh().apply_event(Event::steering_queued("check the README"));
    // The header commits on this first fold; the Steering marker itself
    // is non-terminal, so it stays pending (ADR-0046). No PinBottom.
    assert_eq!(sans_commit(effects), vec![]);

    let (t, effects) = t.apply_event(Event::steering_delivered("check the README"));
    // Delivery promotes the marker to a terminal User line, which now
    // commits.
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(items(&t), vec![user("check the README")]);
}

// --- context visibility (Bundle A) -------------------------------------

// A Session cost update refreshes the bar figure and nothing else: no
// Transcript item, no effects - and later totals replace, never add.
#[test]
fn session_cost_refreshes_the_bar_figure_silently() {
    let t = fresh();
    assert_eq!(t.session_cost, 0.0);
    let (t, effects) = t.apply_event(Event::session_cost(0.007));
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(t.session_cost, 0.007);
    assert_eq!(items(&t), vec![], "never a Transcript item");

    // The event carries the cumulative total; the fold stores, not sums.
    let (t, _) = t.apply_event(Event::session_cost(0.42));
    assert_eq!(t.session_cost, 0.42);
}

// Compaction progress recedes one Housekeeping marker.
#[test]
fn compaction_progress_recedes_one_marker() {
    let t = fresh();
    let (t, effects) = t.apply_event(Event::compaction_progress("working"));
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(
        items(&t),
        vec![marker(
            "⟨ compaction: working → summary ⟩",
            Tone::Housekeeping
        )]
    );
}

#[test]
fn successful_steer_clears_composer() {
    let (t, _) = fresh().apply_event(Event::run_started("r1"));
    let t = press(t, typed("check the README"));
    let (t, effects) = t.steered("check the README", Ok(()));
    assert_eq!(t.composer().view().draft, "");
    // steered_ok adds no terminal item of its own, but routes through the
    // commit seam (ADR-0046) for uniformity: it freezes the still-pending
    // header, so the exit carries a Commit. No effect of its own beyond it.
    assert_eq!(sans_commit(effects), vec![]);
}

#[test]
fn steer_that_lost_race_retries_as_submit_and_the_draft_survives() {
    let (t, _) = fresh().apply_event(Event::run_started("r1"));
    let t = press(t, typed("check the README"));
    let (t, effects) = t.steered("check the README", Err(Idle));
    assert_eq!(t.status, Status::Idle);
    assert_eq!(
        effects,
        vec![Effect::Agent(AgentCommand::Submit(
            "check the README".into()
        ))]
    );
    // Same retry rule as the busy submit: nothing clears.
    assert_eq!(t.composer().view().draft, "check the README");
}

#[test]
fn session_log_error_becomes_info_line() {
    let t = fold(fresh(), vec![Event::session_log_error("disk full")]);
    let items = items(&t);
    assert_eq!(items.len(), 1);
    match &items[0] {
        TranscriptItem::Info { text } => assert!(text.contains("disk full")),
        other => panic!("expected info, got {other:?}"),
    }
}

#[test]
fn extension_error_events_become_info_lines() {
    let t = fold(
        fresh(),
        vec![Event::extension_error("diff", Stage::PreRun, "boom")],
    );
    let items = items(&t);
    assert_eq!(items.len(), 1);
    match &items[0] {
        TranscriptItem::Info { text } => {
            assert!(text.contains("diff"));
            assert!(text.contains("pre_run"));
            assert!(text.contains("boom"));
        }
        other => panic!("expected info, got {other:?}"),
    }
}

// --- tool calls (the arms; the summary and pairing rules live with the
// store, tested at `ui::transcript`) ---------------------------------------

#[test]
fn a_tool_call_recedes_one_pending_call_line() {
    let (t, effects) = fresh().apply_event(Event::tool_call(
        "t1",
        "read_file",
        serde_json::json!({"path": "src/main.rs"}),
    ));
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(
        items(&t),
        vec![TranscriptItem::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "src/main.rs".into(),
        }]
    );
}

// --- the Commit seam at the fold exits (ADR-0046) ------------------------

// Returns the count a trailing Commit carries, or None when the fold
// emitted no commit.
fn commit_count(effects: &[Effect]) -> Option<usize> {
    effects.iter().find_map(|e| match e {
        Effect::Commit { count } => Some(*count),
        _ => None,
    })
}

// A fold that leaves a live ToolCall at the pending front commits only the
// terminal items BEFORE it - here just the header - and never the call.
#[test]
fn a_pending_tool_call_blocks_the_commit_after_it() {
    let (_t, effects) = fresh().apply_event(Event::tool_call(
        "t1",
        "read_file",
        serde_json::json!({"path": "src/main.rs"}),
    ));
    // Header commits (1); the ToolCall stays pending.
    assert_eq!(commit_count(&effects), Some(1));
}

// Folds one event and returns the count a trailing Commit carried (or
// None), ADVANCING the store's high-water mark by that count to mimic the
// adapter's post-blit `mark_committed` - the pure fold no longer moves the
// mark (ADR-0046, transactional commit), so a test that chains folds must
// stand in for the adapter here or every subsequent Commit re-counts the
// same leading items.
fn fold_and_commit(mut t: Screen, event: Event) -> (Screen, Option<usize>) {
    let (mut next, effects) = t.apply_event(event);
    let count = commit_count(&effects);
    if let Some(n) = count {
        next.transcript.mark_committed(n);
    }
    t = next;
    (t, count)
}

// committed==pending identity for the inline approval (ADR-0049): the
// confirming ToolCall carries no result while the Approval is open, so it is
// non-terminal and BLOCKS the commit after it - the approval rows (which
// render off `pending_approval`, not the item) can therefore never freeze
// into scrollback. Once the decision resolves and the ToolResult supersedes
// the call, the tail becomes terminal and commits - as a plain ToolResult,
// with the approval gone.
#[test]
fn a_confirming_tool_call_blocks_commit_until_the_approval_resolves() {
    // Header commits; the gated ToolCall stays pending.
    let (t, header) = fold_and_commit(
        fresh(),
        Event::tool_call(
            "t1",
            "run_shell_command",
            serde_json::json!({"command": "ls"}),
        ),
    );
    assert_eq!(header, Some(1));

    // The Approval opens on the live call: still non-terminal, nothing new
    // commits, and the approval lives on `pending_approval` (never an item).
    let (t, opened) = fold_and_commit(t, Event::approval_request("approval-0", "ls"));
    assert_eq!(opened, None, "the confirming call blocks the commit");
    assert!(t.pending_approval.is_some());

    // Resolve: the pending Approval clears. The call is still an unresolved
    // ToolCall item (no result yet), so it STILL blocks the commit - the
    // approval rows are already gone (pending_approval is None).
    let (t, resolved) = fold_and_commit(t, Event::approval_resolved("approval-0", true));
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        resolved, None,
        "the bare call still blocks until its result"
    );

    // The result supersedes the call → a terminal ToolResult, which commits.
    let (t, committed) = fold_and_commit(
        t,
        Event::tool_result("t1", "run_shell_command", "ok", false, HashMap::new()),
    );
    assert_eq!(committed, Some(1), "the resolved call commits as a result");
    // The committed item is a plain ToolResult - no approval trace.
    assert_eq!(
        items(&t),
        vec![TranscriptItem::ToolResult {
            name: "run_shell_command".into(),
            summary: "ok".into(),
            is_error: false,
            key_arg: Some("ls".into()),
        }]
    );
}

// Once the result merges the call away, the whole run tail becomes terminal
// and the next fold exit commits it.
#[test]
fn a_tool_result_merge_lets_the_run_commit() {
    // First fold commits the header; the call stays pending.
    let (t, first) = fold_and_commit(
        fresh(),
        Event::tool_call(
            "t1",
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
        ),
    );
    assert_eq!(first, Some(1));
    // The result supersedes the call: the merged ToolResult is terminal, so
    // it now commits (count 1 - the header was already committed).
    let (_t, second) = fold_and_commit(
        t,
        Event::tool_result("t1", "run_shell_command", "ok", false, HashMap::new()),
    );
    assert_eq!(second, Some(1));
}

// message_end settles the streamed answer into a terminal Assistant item,
// which the fold exit commits.
#[test]
fn message_end_commits_the_settled_answer() {
    let (t, _) = fold_and_commit(fresh(), Event::run_started("r1"));
    let (t, _) = fold_and_commit(t, Event::message_start(1));
    let (t, _) = fold_and_commit(
        t,
        Event::message_update(
            crate::llm::Delta::Text("Done.".into()),
            vec![text_block("Done.")],
        ),
    );
    // The header committed on the first fold; the streaming snapshot is
    // not an item, so message_end is what settles the terminal answer.
    let (_t, count) = fold_and_commit(
        t,
        Event::message_end(vec![text_block("Done.")], StopReason::EndTurn),
    );
    assert_eq!(count, Some(1));
}

// Steering delivery promotes the pending marker to a terminal User line;
// the delivering fold exit commits it. The queuing fold does not commit the
// marker (it is non-terminal).
#[test]
fn steering_delivery_commits_the_promoted_user_line() {
    // Only the header commits on queue; the marker stays pending.
    let (t, queued) = fold_and_commit(fresh(), Event::steering_queued("check the README"));
    assert_eq!(queued, Some(1));
    // The promoted User line commits (count 1 - the header was already
    // committed).
    let (_t, delivered) = fold_and_commit(t, Event::steering_delivered("check the README"));
    assert_eq!(delivered, Some(1));
}

// TRANSACTIONAL commit (ADR-0046): a fold that EMITS `Commit { count }` must
// NOT advance the high-water mark itself - the mark moves only when the
// adapter's `insert_before` succeeds (`ui::commit_items` -> `mark_committed`).
// So folding the same event twice through the pure core (without the adapter
// running in between) re-emits the SAME commit: the mark never budged.
#[test]
fn the_pure_fold_does_not_advance_the_high_water_mark() {
    let t = fresh();
    assert_eq!(t.transcript().committed_high_water(), 0);
    // The header is committable; the fold emits Commit { 1 } but must leave
    // the mark at 0 (the adapter has not blitted yet).
    let (t, first) = t.apply_event(Event::run_started("r1"));
    assert_eq!(commit_count(&first), Some(1));
    assert_eq!(
        t.transcript().committed_high_water(),
        0,
        "the pure fold must not move the mark - the adapter does, post-blit"
    );
    // A second fold, still no adapter: the same header is STILL uncommitted,
    // so it re-emits Commit { 1 } rather than dropping the count to zero.
    let (t, second) = t.apply_event(Event::message_start(1));
    assert_eq!(commit_count(&second), Some(1));
    assert_eq!(t.transcript().committed_high_water(), 0);
}

// A single fold can turn MORE than one leading item terminal at once: here a
// pending ToolCall is superseded by its result while a second call had
// already settled behind it, so the fold that merges the first result frees a
// batch. The emitted count covers all newly-committable leading items.
#[test]
fn one_fold_can_commit_a_batch_of_newly_terminal_items() {
    // Header + two tool calls in flight; the header commits, both calls
    // stay pending (the first blocks the second).
    let (t, _) = fold_and_commit(
        fresh(),
        Event::tool_call("t1", "read_file", serde_json::json!({"path": "a.rs"})),
    );
    let (t, blocked) = fold_and_commit(
        t,
        Event::tool_call("t2", "read_file", serde_json::json!({"path": "b.rs"})),
    );
    // The leading ToolCall (t1) is non-terminal, so nothing new commits.
    assert_eq!(blocked, None);
    // Resolve t2 first (behind the still-pending t1): still blocked by t1.
    let (t, still_blocked) = fold_and_commit(
        t,
        Event::tool_result("t2", "read_file", "ok", false, HashMap::new()),
    );
    assert_eq!(still_blocked, None);
    // Now resolve t1: t1's result AND t2's already-settled result both become
    // leading terminal items - ONE fold commits the batch of two.
    let (_t, batch) = fold_and_commit(
        t,
        Event::tool_result("t1", "read_file", "ok", false, HashMap::new()),
    );
    assert_eq!(batch, Some(2), "one fold committed both freed results");
}

// `newest_live_tool_name` (ADR-0049) is what the inline approval attaches
// to. With two live ToolCalls it picks the NEWEST by position; and a
// resolved (superseded → ToolResult) call is skipped so the block never
// binds to a call that already has a result.
#[test]
fn newest_live_tool_name_picks_the_newest_live_call_and_skips_resolved_ones() {
    // Two live calls, neither resolved: the newest (t2) is the live one.
    let t = fold(
        fresh(),
        vec![
            Event::tool_call(
                "t1",
                "run_shell_command",
                serde_json::json!({"command": "echo one"}),
            ),
            Event::tool_call("t2", "web_fetch", serde_json::json!({"url": "https://x"})),
        ],
    );
    assert_eq!(t.newest_live_tool_name(), Some("web_fetch"));

    // Resolve the newer call (t2 supersedes to a ToolResult): the only
    // surviving live ToolCall is the older t1, so the attach falls to it -
    // a resolved call is never chosen.
    let t = fold(
        t,
        vec![Event::tool_result(
            "t2",
            "web_fetch",
            "ok",
            false,
            HashMap::new(),
        )],
    );
    assert_eq!(t.newest_live_tool_name(), Some("run_shell_command"));

    // With no live ToolCall at all (t1 also resolved), it is None.
    let t = fold(
        t,
        vec![Event::tool_result(
            "t1",
            "run_shell_command",
            "ok",
            false,
            HashMap::new(),
        )],
    );
    assert_eq!(t.newest_live_tool_name(), None);
}

// Negative case: a fold that only ADDS a still-pending leading ToolCall (with
// nothing terminal ahead of it) emits no Commit at all.
#[test]
fn a_fold_that_only_adds_a_pending_tool_call_commits_nothing_new() {
    // Commit the header first (via the adapter stand-in).
    let (t, header) = fold_and_commit(fresh(), Event::run_started("r1"));
    assert_eq!(header, Some(1));
    // Now the only uncommitted item added is a live ToolCall: nothing new is
    // terminal, so no Commit is emitted.
    let (_t, none) = fold_and_commit(
        t,
        Event::tool_call("t1", "read_file", serde_json::json!({"path": "a.rs"})),
    );
    assert_eq!(none, None);
}

#[test]
fn a_tool_result_merges_with_its_call_into_one_line() {
    let t = fold(
        fresh(),
        vec![
            Event::tool_call(
                "t1",
                "run_shell_command",
                serde_json::json!({"command": "cargo test"}),
            ),
            Event::tool_result("t1", "run_shell_command", "ok", false, HashMap::new()),
        ],
    );
    let items = items(&t);
    assert_eq!(items.len(), 1, "the call line was superseded");
    match &items[0] {
        TranscriptItem::ToolResult {
            name,
            is_error,
            key_arg,
            ..
        } => {
            assert_eq!(name, "run_shell_command");
            assert!(!is_error);
            assert_eq!(key_arg.as_deref(), Some("cargo test"));
        }
        other => panic!("expected a merged result line, got {other:?}"),
    }
}

// --- Composer first refusal (ADR-0034) ----------------------------------
//
// The Composer's own rules - menu, selector, editing, history recall -
// are tested at its interface in `ui::composer`; these pin the ROUTING
// this fold owns: the fixed gate → Composer → own-arms order, the notice
// wiring, and the refused key coming back by value.

// The Composer's first refusal covers EVENTS too: a selector fill
// delivered through this fold is consumed by the Composer - the overlay
// flips to Ready, no Transcript item, no effects - so a stale or
// overlay-less fill can never leak into the arms below.
#[test]
fn a_selector_fill_is_consumed_by_the_composer_never_this_folds_arms() {
    use crate::ui::composer::{OverlayStatus, OverlayView};
    use crate::view_model::SelectorRow;

    // Commit `/model` through the Screen: a Loading overlay opens and one
    // Command effect carries the activation generation to echo back.
    let t = press(fresh(), typed("/model"));
    let (t, effects) = t.handle_key(Key::Enter);
    let effects = sans_commit(effects);
    let generation = match effects.as_slice() {
        [Effect::Command { name, generation }] if name == "model" => *generation,
        other => panic!("expected one Command effect, got {other:?}"),
    };

    let rows = vec![SelectorRow::new("qwen", "qwen", None)];
    let (t, effects) = t.apply_event(Event::selector_ready(generation, rows.clone()));
    assert_eq!(effects, vec![]);
    assert_eq!(items(&t), vec![], "never a Transcript item");
    match t.composer().view().overlay {
        Some(OverlayView::Dialog {
            status: OverlayStatus::Ready,
            rows: got,
            ..
        }) => assert_eq!(got, rows),
        other => panic!("expected a Ready selector overlay, got {other:?}"),
    }
}

// Escape with an open overlay closes the overlay - it must NOT cancel the
// running Run (Escape is only Cancellation when the Composer refuses it).
#[test]
fn escape_with_an_open_overlay_closes_it_instead_of_cancelling_the_run() {
    let (t, _) = fresh().apply_event(Event::run_started("r1"));
    let t = press(t, vec![Key::Char('/')]);
    assert!(
        t.composer().view().overlay.is_some(),
        "menu opens while running"
    );
    let (t, effects) = t.handle_key(Key::Escape);
    assert_eq!(
        sans_commit(effects),
        vec![],
        "no Cancel - the Composer consumed Escape"
    );
    assert!(t.composer().view().overlay.is_none());
    assert_eq!(t.status, Status::Running, "the Turn is untouched");
    // With the Composer emptied, Escape is refused and Cancellation fires.
    let (_t, effects) = t.handle_key(Key::Escape);
    assert_eq!(
        sans_commit(effects),
        vec![Effect::Agent(AgentCommand::Cancel)]
    );
}

// A refused key comes back BY VALUE and still reaches the arms below,
// mid-draft included: refusal returns the key, it does not drop it. PageUp
// no longer scrolls (ADR-0046), so it falls through with no effect - but the
// draft stays untouched, proving the key was refused (not consumed as text).
#[test]
fn a_refused_key_reaches_the_arms_below_mid_draft() {
    let t = press(fresh(), typed("half a thought"));
    let (t, effects) = t.handle_key(Key::PageUp);
    assert_eq!(sans_commit(effects), vec![]);
    assert_eq!(
        t.composer().view().draft,
        "half a thought",
        "the draft is untouched"
    );
}

// The Composer's notice (the unknown-command line) lands as a normal info
// line through the store - never an Effect the adapter must interpret.
#[test]
fn a_composer_notice_becomes_an_info_line() {
    let t = press(fresh(), typed("/nope"));
    let (t, effects) = t.handle_key(Key::Enter);
    // The unknown-command info line commits on this exit (ADR-0046).
    assert_eq!(sans_commit(effects), vec![], "no Turn, no command effect");
    assert_eq!(items(&t), vec![info("unknown command: /nope")]);
    assert_eq!(t.composer().view().draft, "", "draft cleared");
}

// --- Cancellation and errors -------------------------------------------

#[test]
fn escape_while_running_no_modal_cancels() {
    let (t, _) = fresh().apply_event(Event::run_started("r1"));
    let (_t, effects) = t.handle_key(Key::Escape);
    assert_eq!(
        sans_commit(effects),
        vec![Effect::Agent(AgentCommand::Cancel)]
    );
}

#[test]
fn escape_while_idle_does_nothing() {
    let (_t, effects) = fresh().handle_key(Key::Escape);
    assert_eq!(sans_commit(effects), vec![]);
}

#[test]
fn run_cancelled_flushes_snapshot_goes_idle_notes_cancellation() {
    let t = fold(
        fresh(),
        vec![
            Event::run_started("r1"),
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Thinking("half a thought".into()),
                vec![thinking_block("half a thought")],
            ),
            Event::RunCancelled,
        ],
    );
    assert_eq!(t.status, Status::Idle);
    assert_eq!(
        items(&t),
        vec![thinking("half a thought"), info("turn cancelled")]
    );
}

#[test]
fn run_cancelled_clears_pending_approval_and_refocuses() {
    let t = fold(fresh(), vec![Event::run_started("r1")]);
    let t = with_pending_approval(t, &approval());
    let (t, effects) = t.apply_event(Event::RunCancelled);
    assert_eq!(t.pending_approval, None);
    assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
}

#[test]
fn run_error_notes_reason_and_goes_idle() {
    let (t, _) = fresh().apply_event(Event::run_started("r1"));
    let (t, _) = t.apply_event(Event::RunError {
        reason: ":boom".into(),
    });
    assert_eq!(t.status, Status::Idle);
    assert_eq!(items(&t), vec![info("turn error: :boom")]);
}

// --- agent_down --------------------------------------------------------

#[test]
fn agent_down_resets_to_truthful_idle_and_reports_restart() {
    let t = fold(
        fresh(),
        vec![
            Event::run_started("r1"),
            Event::message_start(1),
            Event::message_update(
                crate::llm::Delta::Text("half an ans".into()),
                vec![text_block("half an ans")],
            ),
        ],
    );
    let t = with_pending_approval(t, &approval());
    let (t, effects) = t.agent_down();
    assert_eq!(t.status, Status::Idle);
    assert_eq!(t.pending_approval, None);
    assert_eq!(
        t.transcript().items().last(),
        Some(&info("agent restarted; session history was reset"))
    );
    assert!(t.transcript().items().contains(&assistant("half an ans")));
    assert_eq!(effects, vec![Effect::FocusComposer]);
}

// --- info (adapter-side news) --------------------------------------------

// The adapter's direct line in: Resume drift notes and other
// adapter-authored news append as one info line through the store.
#[test]
fn info_appends_one_adapter_authored_line() {
    let (t, effects) = fresh().info("resume: 2 turns replayed with drift");
    assert_eq!(items(&t), vec![info("resume: 2 turns replayed with drift")]);
    // The info line routes through the commit seam (ADR-0046): the header
    // and the new info line are both terminal, so the exit emits a Commit.
    assert_eq!(commit_count(&effects), Some(2));
}

// --- unknown input -----------------------------------------------------

#[test]
fn a_stale_selector_fill_and_an_unknown_key_are_ignored() {
    let t = fresh();
    // A selector fill with no overlay open is the Composer's own event
    // (ADR-0034): it is consumed there, changes nothing, and never reaches
    // a Transcript item.
    let (t, effects) = t.apply_event(Event::selector_ready(0, vec![]));
    assert_eq!(effects, vec![]);
    assert_eq!(items(&t), vec![]);

    let (_t, effects) = t.handle_key(Key::Other);
    // The header was still uncommitted (the selector fill returned via
    // the Composer without a fold exit), so this no-op key commits it.
    assert_eq!(sans_commit(effects), vec![]);
}

// --- scroll keys are inert in the Screen (ADR-0046) --------------------
//
// Native scrollback owns history now: PageUp/PageDown and the mouse wheel no
// longer emit a scroll effect from the Screen. They stay in [`Key`] for the
// pre-agent Session Picker (its alt-screen list still navigates by them),
// but the transcript fold produces nothing for them.

#[test]
fn page_and_wheel_keys_are_inert_idle_and_running() {
    for key in [Key::PageUp, Key::PageDown, Key::WheelUp, Key::WheelDown] {
        let (_t, effects) = fresh().handle_key(key.clone());
        assert_eq!(sans_commit(effects), vec![], "{key:?} idle is inert");

        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let (_t, effects) = t.handle_key(key.clone());
        assert_eq!(sans_commit(effects), vec![], "{key:?} running is inert");
    }
}

#[test]
fn wheel_keys_swallowed_while_modal_open() {
    for key in [Key::WheelUp, Key::WheelDown] {
        assert_key_swallowed_while_modal_open(key);
    }
}

// --- Ctrl-O compact mode (ADR-0052) --------------------------------------

#[test]
fn compact_mode_starts_off() {
    assert!(!fresh().compact_mode);
}

// A plain-chat transcript (only the startup Header committed, nothing
// compact-affected) flips compact with NO RedrawScrollback - the predicate is
// false, so no expensive scrollback redraw is minted.
#[test]
fn toggle_compact_flips_without_redraw_when_nothing_committed_is_affected() {
    let (t, effects) = fresh().handle_key(Key::ToggleCompact);
    assert!(t.compact_mode);
    // Only the startup Header exists; no committed Thinking/tool item, so no
    // RedrawScrollback. (The Header's own Commit may ride along.)
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RedrawScrollback)),
        "a plain chat toggles with no scrollback redraw"
    );

    let (t, effects) = t.handle_key(Key::ToggleCompact);
    assert!(!t.compact_mode);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RedrawScrollback))
    );
}

// With a committed Thinking item, flipping compact DOES mint a
// RedrawScrollback (the frozen thought must be un-drawn, ADR-0052).
#[test]
fn toggle_compact_emits_redraw_when_a_committed_item_is_affected() {
    // Stream + settle a thought, then commit it into scrollback.
    let (screen, _) = fresh().apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        crate::llm::Delta::Thinking("thinking".into()),
        vec![ContentBlock::Thinking {
            text: "a thought".into(),
        }],
    ));
    let (mut screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
    // Freeze everything terminal (the adapter's job) so the thought is
    // committed in the pure core's view.
    let hw = screen.transcript().committable_upto();
    screen.mark_committed(hw);
    assert!(
        screen.transcript().compact_toggle_has_visual_effect(),
        "the committed thought makes the toggle visually effective"
    );

    let (screen, effects) = screen.handle_key(Key::ToggleCompact);
    assert!(screen.compact_mode);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RedrawScrollback)),
        "a committed thought forces the scrollback redraw: {effects:?}"
    );
}

// Compact is DISPLAY-ONLY (ADR-0052): flipping `compact_mode` must not change
// the structural commit seam. `committable_upto` counts leading TERMINAL items
// (a structural property of the transcript), so it is invariant under a compact
// toggle - the invariant that lets committed==pending hold under compact.
#[test]
fn compact_toggle_does_not_change_committable_upto() {
    // Settle a thought so there IS something committable to measure.
    let (screen, _) = fresh().apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        crate::llm::Delta::Thinking("thinking".into()),
        vec![ContentBlock::Thinking {
            text: "a thought".into(),
        }],
    ));
    let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));

    let before = screen.transcript().committable_upto();
    let (screen, _) = screen.handle_key(Key::ToggleCompact);
    assert!(screen.compact_mode);
    assert_eq!(
        screen.transcript().committable_upto(),
        before,
        "compact is display-only; the commit seam is structural and unchanged"
    );
}

#[test]
fn modal_swallows_toggle_compact() {
    assert_key_swallowed_while_modal_open(Key::ToggleCompact);
    // The flag must not have flipped; a fresh Screen starts non-compact.
    assert!(!fresh().compact_mode);
}

// --- Ctrl-S peek (BUG 1, ADR-0046) ---------------------------------------

// Ctrl-S emits `PeekPending` and nothing else: the fixed inline viewport
// cannot grow, so the pure core fires a non-committing peek the adapter blits
// into scrollback. It changes NO state (no commit seam), so the effect list is
// exactly one `PeekPending`.
#[test]
fn show_more_emits_peek_pending_only() {
    let (_t, effects) = fresh().handle_key(Key::ShowMore);
    assert_eq!(effects, vec![Effect::PeekPending]);
}

// Ctrl-S is handled BEFORE the Approval gate: an overflowing approval body is
// exactly when the user reaches for "show more", so the peek must fire even
// while a modal holds the keyboard. The pending approval is left untouched.
#[test]
fn show_more_peeks_even_while_a_modal_is_open() {
    let t = with_pending_approval(fresh(), &approval());
    let pending_before = t.pending_approval.clone();
    let (t, effects) = t.handle_key(Key::ShowMore);
    assert_eq!(effects, vec![Effect::PeekPending]);
    assert_eq!(
        t.pending_approval, pending_before,
        "the peek does not resolve or disturb the open approval"
    );
}

// --- the Help overlay (qwen `Help`, the `?` affordance) ------------------

#[test]
fn help_starts_closed() {
    assert!(!fresh().help_open);
}

// `?` on an EMPTY draft opens the Help overlay and focuses it like a modal
// (`FocusModal`), consuming the key so no `?` lands in the draft.
#[test]
fn question_mark_on_empty_draft_opens_help() {
    let (t, effects) = fresh().handle_key(Key::Char('?'));
    assert!(t.help_open, "? opens Help on an empty draft");
    assert_eq!(t.composer().view().draft, "", "the ? was not typed");
    assert_eq!(sans_commit(effects), vec![Effect::FocusModal]);
}

// `?` on a NON-empty draft stays a typed char (the interception defers to the
// Composer's first refusal), so Help does NOT open and the draft gains a `?`.
#[test]
fn question_mark_on_non_empty_draft_types_normally() {
    let t = press(fresh(), typed("fix"));
    let (t, _effects) = t.handle_key(Key::Char('?'));
    assert!(
        !t.help_open,
        "? does not open Help while the draft is non-empty"
    );
    assert_eq!(
        t.composer().view().draft,
        "fix?",
        "the ? typed into the draft"
    );
}

// Esc closes the open Help overlay and hands focus back to the composer.
#[test]
fn escape_closes_help() {
    let t = press(fresh(), vec![Key::Char('?')]);
    assert!(t.help_open);
    let (t, effects) = t.handle_key(Key::Escape);
    assert!(!t.help_open, "Esc closes the Help overlay");
    assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
}

// `?` and `q` are convenience closers too (qwen `Help`), also refocusing.
#[test]
fn question_mark_or_q_also_close_help() {
    for closer in [Key::Char('?'), Key::Char('q')] {
        let t = press(fresh(), vec![Key::Char('?')]);
        let (t, effects) = t.handle_key(closer.clone());
        assert!(!t.help_open, "{closer:?} closes the Help overlay");
        assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
    }
}

// While Help is open it holds the keyboard like the Approval modal: every
// non-closer key is swallowed with NO effect and NO leak to the Composer, so
// the draft stays empty and nothing runs.
#[test]
fn help_swallows_every_non_closer_key() {
    for key in [Key::Char('x'), Key::Enter, Key::ArrowUp, Key::Tab] {
        let t = press(fresh(), vec![Key::Char('?')]);
        let (next, effects) = t.handle_key(key.clone());
        assert!(next.help_open, "{key:?} leaves Help open");
        assert!(
            effects.is_empty(),
            "{key:?} produces no effect while Help is open"
        );
        assert_eq!(
            next.composer().view().draft,
            "",
            "{key:?} did not leak to the draft"
        );
    }
}

// The Approval gate wins if both could apply: an open Approval routes to its
// own handler, so `?` never opens Help behind a pending Approval.
#[test]
fn approval_gate_wins_over_help() {
    let t = with_pending_approval(fresh(), &approval());
    let (t, _effects) = t.handle_key(Key::Char('?'));
    assert!(!t.help_open, "an open Approval keeps ? from opening Help");
    assert!(t.pending_approval.is_some());
}

// --- the Approval gate vs the Composer ----------------------------------
//
// Editing itself is tested at the Composer's interface (`ui::composer`);
// this pins the gate ORDER - the modal runs before the Composer's first
// refusal, so a typed char must NOT edit the draft while it is open.

#[test]
fn typed_chars_do_not_edit_the_composer_while_modal_open() {
    let t = press(fresh(), typed("draft"));
    let t = with_pending_approval(t, &approval());
    let pending_before = t.pending_approval.clone();
    let t = press(
        t,
        vec![
            Key::Char('x'),
            Key::Backspace,
            Key::InsertNewline,
            Key::Left,
        ],
    );
    assert_eq!(t.composer().view().draft, "draft");
    assert_eq!(t.composer().view().cursor, 5);
    assert_eq!(t.pending_approval, pending_before);
}
