
use super::*;
use crate::content::ContentBlock;
use crate::event::Stage;
use crate::view_model::Tone;
use crate::view_model::TranscriptItem;
use std::collections::HashMap;

// --- helpers mirroring transcript_test.exs -----------------------------

fn fresh() -> Screen {
    Screen::new(ScreenOpts::default())
}

fn fresh_opts(opts: ScreenOpts) -> Screen {
    Screen::new(opts)
}

// Identity pass-through (ADR-0046): the fullscreen renderer retired the
// inline commit seam, so folds no longer append a trailing `Effect::Commit`.
// Kept as a no-op so the many effect assertions that wrapped their effects in
// it stay readable, and so a future commit-like effect has one place to strip.
fn sans_commit(effects: Vec<Effect>) -> Vec<Effect> {
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
    // The only effect is the on-disk HistoryAppend (ADR-0046, fullscreen: no
    // commit seam - the appended User line just renders next frame).
    assert_eq!(effects, vec![Effect::HistoryAppend("fix the bug".into())]);
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
    // The info line just renders next frame (ADR-0046, fullscreen: no commit
    // seam), so no effect is due.
    assert!(effects.is_empty());
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

// --- scroll keys mint NO effect (ADR-0046, Stage 2) --------------------
//
// The app owns scrolling now: PageUp/PageDown and the mouse wheel move the
// Screen's scroll INTENT (tested above), but that is a pure view move - it
// emits no Effect for the adapter to carry out, idle or running. (They also
// still drive the pre-agent Session Picker's alt-screen list.)

#[test]
fn scroll_keys_mint_no_effect_idle_and_running() {
    for key in [Key::PageUp, Key::PageDown, Key::WheelUp, Key::WheelDown] {
        let (_t, effects) = fresh().handle_key(key.clone());
        assert_eq!(sans_commit(effects), vec![], "{key:?} idle mints nothing");

        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let (_t, effects) = t.handle_key(key.clone());
        assert_eq!(
            sans_commit(effects),
            vec![],
            "{key:?} running mints nothing"
        );
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

// Ctrl-O flips compact mode and mints NO effect (ADR-0046): the fullscreen
// renderer redraws the whole transcript at the new compact next frame, so
// there is no frozen scrollback to un-draw and nothing to emit.
#[test]
fn toggle_compact_flips_with_no_effect() {
    let (t, effects) = fresh().handle_key(Key::ToggleCompact);
    assert!(t.compact_mode);
    assert!(effects.is_empty(), "the flip needs no effect: {effects:?}");

    let (t, effects) = t.handle_key(Key::ToggleCompact);
    assert!(!t.compact_mode);
    assert!(effects.is_empty());
}

// The flip still takes hold with a settled Thinking item on screen - it just
// rides the free full-transcript redraw, no effect emitted.
#[test]
fn toggle_compact_flips_with_a_thinking_item_on_screen() {
    let (screen, _) = fresh().apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        crate::llm::Delta::Thinking("thinking".into()),
        vec![ContentBlock::Thinking {
            text: "a thought".into(),
        }],
    ));
    let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));

    let (screen, effects) = screen.handle_key(Key::ToggleCompact);
    assert!(screen.compact_mode);
    assert!(effects.is_empty(), "no scrollback redraw: {effects:?}");
}

#[test]
fn modal_swallows_toggle_compact() {
    assert_key_swallowed_while_modal_open(Key::ToggleCompact);
    // The flag must not have flipped; a fresh Screen starts non-compact.
    assert!(!fresh().compact_mode);
}

// --- app-owned transcript scrolling (ADR-0046, Stage 2) ------------------
//
// The pure core holds only the scroll INTENT (`scroll_lines` + `follow_tail`);
// the render clamps it to the live viewport. These pin the intent moves - the
// clamp itself is tested at [`components::anchor_clip`] in `ui::components`.

// A fresh Screen follows the tail with no scroll offset (the qwen/chat-UI
// default: newest content pinned to the bottom).
#[test]
fn scroll_starts_following_the_tail() {
    let t = fresh();
    assert!(t.follow_tail, "a fresh Screen follows the tail");
    assert_eq!(t.scroll_lines, 0);
}

// WheelUp DETACHES from the tail and lifts the view by one wheel step, minting
// no effect (scrolling is a pure view move).
#[test]
fn wheel_up_detaches_and_scrolls() {
    let (t, effects) = fresh().handle_key(Key::WheelUp);
    assert!(!t.follow_tail, "wheel-up detaches from the tail");
    assert_eq!(t.scroll_lines, WHEEL_STEP);
    assert!(
        effects.is_empty(),
        "a scroll move emits nothing: {effects:?}"
    );

    // A second tick accumulates another step.
    let (t, _) = t.handle_key(Key::WheelUp);
    assert_eq!(t.scroll_lines, 2 * WHEEL_STEP);
}

// WheelDown walks back toward the tail; reaching 0 RE-ATTACHES so new content
// follows again.
#[test]
fn wheel_down_reattaches_at_the_bottom() {
    let t = press(fresh(), vec![Key::WheelUp]); // scroll_lines == WHEEL_STEP
    assert!(!t.follow_tail);

    let (t, _) = t.handle_key(Key::WheelDown);
    assert_eq!(t.scroll_lines, 0);
    assert!(t.follow_tail, "reaching the bottom re-attaches to the tail");
}

// WheelDown while already at the bottom is a harmless no-op: `scroll_lines`
// saturates at 0 and the view stays attached.
#[test]
fn wheel_down_at_the_bottom_stays_attached() {
    let (t, _) = fresh().handle_key(Key::WheelDown);
    assert_eq!(t.scroll_lines, 0);
    assert!(t.follow_tail);
}

// PageUp/Ctrl-S step by the last recorded body height (the adapter records it
// each frame); with none yet measured the page floors at one row so a
// pre-frame press still moves. Both detach.
#[test]
fn page_up_uses_the_recorded_body_height() {
    let mut t = fresh();
    t.note_body_height(20);
    let (t, effects) = t.handle_key(Key::PageUp);
    assert!(!t.follow_tail, "page-up detaches");
    assert_eq!(t.scroll_lines, 20);
    assert!(effects.is_empty());

    // Ctrl-S is the keyboard page-up: same step.
    let (t, _) = t.handle_key(Key::ShowMore);
    assert_eq!(t.scroll_lines, 40);
}

#[test]
fn page_up_before_any_frame_moves_one_row() {
    // `last_body_height` is 0 until the first frame; the page floors at 1.
    let (t, _) = fresh().handle_key(Key::PageUp);
    assert_eq!(t.scroll_lines, 1);
    assert!(!t.follow_tail);
}

// PageDown re-attaches at the bottom exactly like WheelDown.
#[test]
fn page_down_reattaches_at_the_bottom() {
    let mut t = fresh();
    t.note_body_height(20);
    let t = press(t, vec![Key::PageUp]); // scroll_lines == 20
    let (t, _) = t.handle_key(Key::PageDown);
    assert_eq!(t.scroll_lines, 0);
    assert!(t.follow_tail);
}

// Home (on an EMPTY draft) jumps to the TOP: detaches and asks for the max
// scroll (`usize::MAX`, the clamp saturates it to the oldest row).
#[test]
fn home_on_empty_draft_jumps_to_top() {
    let (t, effects) = fresh().handle_key(Key::Home);
    assert!(!t.follow_tail, "Home detaches from the tail");
    assert_eq!(t.scroll_lines, usize::MAX, "Home asks for the very top");
    assert!(effects.is_empty());
}

// End (on an EMPTY draft) RE-ATTACHES to the tail from any scroll position.
#[test]
fn end_on_empty_draft_reattaches() {
    let t = press(fresh(), vec![Key::Home]); // detached, at the top
    let (t, _) = t.handle_key(Key::End);
    assert!(t.follow_tail, "End re-attaches to the tail");
    assert_eq!(t.scroll_lines, 0);
}

// With a NON-empty draft, Home/End stay the Composer's readline line-nav (jump
// to line start/end) and do NOT scroll the transcript - the empty-draft guard.
#[test]
fn home_end_are_line_nav_while_typing() {
    let t = press(fresh(), typed("hello"));
    assert_eq!(t.composer().view().cursor, 5);

    let (t, _) = t.handle_key(Key::Home);
    assert_eq!(t.composer().view().cursor, 0, "Home moves the draft cursor");
    assert!(t.follow_tail, "Home did not scroll the transcript");

    let (t, _) = t.handle_key(Key::End);
    assert_eq!(t.composer().view().cursor, 5, "End moves the draft cursor");
    assert!(t.follow_tail, "End did not disturb the tail-follow");
}

// A detached view stays PUT when new content appends (streaming or new items):
// `follow_tail` is false, so the intent is untouched by the fold. This is the
// "no yank-down while detached" rule the render honors.
#[test]
fn appended_content_does_not_move_a_detached_view() {
    let t = press(fresh(), vec![Key::WheelUp, Key::WheelUp]); // detached, up 2 steps
    let scroll_before = t.scroll_lines;

    // A whole streamed answer appends at the bottom.
    let (t, _) = t.apply_event(Event::message_start(1));
    let (t, _) = t.apply_event(Event::message_update(
        crate::llm::Delta::Text("more output".into()),
        vec![ContentBlock::Text {
            text: "more output".into(),
        }],
    ));
    let (t, _) = t.apply_event(Event::message_end(vec![], StopReason::EndTurn));

    assert!(
        !t.follow_tail,
        "appending does not re-attach a detached view"
    );
    assert_eq!(
        t.scroll_lines, scroll_before,
        "appending does not move the detached scroll intent"
    );
}

// While FOLLOWING the tail, appended content keeps the view pinned to the
// bottom (the default): `follow_tail` stays true and the intent stays 0.
#[test]
fn appended_content_keeps_a_followed_view_pinned() {
    let (t, _) = fresh().apply_event(Event::message_start(1));
    let (t, _) = t.apply_event(Event::message_update(
        crate::llm::Delta::Text("output".into()),
        vec![ContentBlock::Text {
            text: "output".into(),
        }],
    ));
    assert!(t.follow_tail, "the default view stays attached");
    assert_eq!(t.scroll_lines, 0);
}

// Ctrl-S (page-up) is handled BEFORE the Approval gate, so it scrolls the body
// behind an open modal without disturbing the pending approval.
#[test]
fn show_more_scrolls_behind_an_open_modal() {
    let t = with_pending_approval(fresh(), &approval());
    let pending_before = t.pending_approval.clone();
    let (t, effects) = t.handle_key(Key::ShowMore);
    assert!(effects.is_empty());
    assert!(!t.follow_tail, "Ctrl-S detaches even behind a modal");
    assert_eq!(
        t.pending_approval, pending_before,
        "scrolling does not resolve or disturb the open approval"
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
