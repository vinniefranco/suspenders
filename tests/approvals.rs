
use super::*;
use serde_json::json;

fn id() -> ApprovalId {
    ApprovalId::new()
}

// ---- gate_text: the one Approval-gate query ----

#[test]
fn gate_text_gates_run_command_and_web_fetch_and_nothing_else() {
    assert!(gate_text("run_shell_command", &json!({"command": "mix test"})).is_some());
    assert!(gate_text("web_fetch", &json!({"url": "https://docs.rs/tokio"})).is_some());

    for name in [
        "plan",
        "read_file",
        "list_directory",
        "grep_search",
        "edit",
        "write_file",
    ] {
        assert_eq!(gate_text(name, &json!({})), None, "{name} must not gate");
    }
    assert_eq!(gate_text("no_such_tool", &json!({})), None);
}

#[test]
fn gate_text_is_the_command_for_run_command() {
    assert_eq!(
        gate_text("run_shell_command", &json!({"command": "mix test"})),
        Some("mix test".to_string())
    );
}

#[test]
fn gate_text_is_the_domain_for_web_fetch() {
    // Domain-scoped (ADR-0024, revised, faithful to qwen): the gate text is
    // the URL's hostname, so a Standing Approval covers every path under it.
    assert_eq!(
        gate_text("web_fetch", &json!({"url": "https://docs.rs/tokio/latest"})),
        Some("docs.rs".to_string())
    );
    // A second fetch to the same host reads the same gate text (would
    // auto-approve under a Standing Approval).
    assert_eq!(
        gate_text("web_fetch", &json!({"url": "https://docs.rs/serde"})),
        Some("docs.rs".to_string())
    );
}

#[test]
fn gate_text_web_fetch_falls_back_to_the_raw_string_when_unparseable() {
    // qwen's `catch { domain = url }`: an unparseable URL (or one with no
    // host) scopes to the raw string itself.
    assert_eq!(
        gate_text("web_fetch", &json!({"url": "not a url"})),
        Some("not a url".to_string())
    );
}

#[test]
fn gate_text_is_none_for_tools_that_need_no_approval() {
    assert_eq!(gate_text("read_file", &json!({"path": "a.ex"})), None);
    assert_eq!(gate_text("no_such_tool", &json!({})), None);
}

// Memory writes auto-approve (P5, ADR-0062): qwen flips the default
// permission to 'allow' for a memory write so autonomous memory does not
// prompt. In Suspenders, write_file/edit_file are UNGATED by design
// (ADR-0050 gates only code-execution and outbound-fetch), so a write into
// the memory dir - like any write - carries no gate at all, while
// run_command still gates. This test pins that: the auto-approval of memory
// writes is inherent in the ungated write path, not a per-path branch.
#[test]
fn memory_dir_writes_do_not_gate_while_code_execution_still_does() {
    // A write/edit targeting a memory path is ungated (auto-approved).
    assert_eq!(
        gate_text(
            "write_file",
            &json!({"path": "/data/projects/slug/memory/MEMORY.md", "content": "x"})
        ),
        None
    );
    assert_eq!(
        gate_text(
            "edit",
            &json!({"path": "/data/projects/slug/memory/user.md", "old_str": "a", "new_str": "b"})
        ),
        None
    );
    // A non-memory code-execution call still gates, unchanged.
    assert!(gate_text("run_shell_command", &json!({"command": "rm -rf /"})).is_some());
}

#[test]
fn gate_text_falls_back_to_empty_when_the_field_is_missing_or_non_string() {
    assert_eq!(
        gate_text("run_shell_command", &json!({})),
        Some(String::new())
    );
    assert_eq!(
        gate_text("web_fetch", &json!({"url": 42})),
        Some(String::new())
    );
}

// An Approvals with a Standing Approval for `command`, nothing pending.
fn granted(command: &str) -> Approvals {
    let Request::Pending(approvals) = Approvals::new().request(id(), command) else {
        panic!("expected pending");
    };
    // Reuse the pending id to record it as standing.
    let pending_id = approvals.pending.as_ref().unwrap().id.clone();
    let Decide::Forward(true, approvals) = approvals.decide(pending_id, Decision::ApproveAlways)
    else {
        panic!("expected forward true");
    };
    approvals
}

// ---- request/3 ----

#[test]
fn with_no_standing_approval_the_request_becomes_pending() {
    let approvals = Approvals::new();
    let r = id();

    let Request::Pending(approvals) = approvals.request(r.clone(), "mix test") else {
        panic!("expected pending");
    };
    assert_eq!(
        approvals.pending,
        Some(Pending {
            id: r,
            command: "mix test".into()
        })
    );
}

#[test]
fn a_standing_approval_covering_the_exact_command_answers_auto_with_nothing_pending() {
    let Request::Auto(approvals) = granted("mix test").request(id(), "mix test") else {
        panic!("expected auto");
    };
    assert_eq!(approvals.pending, None);
}

#[test]
fn matching_is_string_equality_only_whitespace_variants_are_different_commands() {
    let approvals = granted("mix test");

    assert!(matches!(
        approvals.clone().request(id(), "mix test"),
        Request::Auto(_)
    ));
    assert!(matches!(
        approvals.clone().request(id(), "mix  test"),
        Request::Pending(_)
    ));
    assert!(matches!(
        approvals.clone().request(id(), "mix test "),
        Request::Pending(_)
    ));
    assert!(matches!(
        approvals.clone().request(id(), "MIX TEST"),
        Request::Pending(_)
    ));
}

#[test]
fn no_prefix_widening_an_approved_stem_does_not_cover_a_longer_command() {
    let approvals = granted("mix test");

    assert!(matches!(
        approvals.request(id(), "mix test --seed 0 && rm -rf /"),
        Request::Pending(_)
    ));
}

// ---- decide/3 ----

#[test]
fn approve_forwards_approved_without_recording_a_standing_approval() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };

    let Decide::Forward(true, approvals) = approvals.decide(r, Decision::Approve) else {
        panic!("expected forward true");
    };
    assert_eq!(approvals.pending, None);
    assert!(matches!(
        approvals.request(id(), "mix test"),
        Request::Pending(_)
    ));
}

#[test]
fn deny_forwards_not_approved_and_records_nothing() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };

    let Decide::Forward(false, approvals) = approvals.decide(r, Decision::Deny) else {
        panic!("expected forward false");
    };
    assert_eq!(approvals.pending, None);
    assert!(matches!(
        approvals.request(id(), "mix test"),
        Request::Pending(_)
    ));
}

#[test]
fn approve_always_forwards_approved_and_records_the_exact_pending_command() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };

    let Decide::Forward(true, approvals) = approvals.decide(r, Decision::ApproveAlways) else {
        panic!("expected forward true");
    };
    assert!(matches!(
        approvals.request(id(), "mix test"),
        Request::Auto(_)
    ));
}

#[test]
fn a_stale_or_unknown_id_is_ignored_leaving_the_pending_approval_open() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };

    let Decide::Ignore(approvals) = approvals.decide(id(), Decision::Approve) else {
        panic!("expected ignore");
    };
    assert_eq!(
        approvals.pending,
        Some(Pending {
            id: r,
            command: "mix test".into()
        })
    );
}

#[test]
fn a_duplicate_decision_after_the_forward_is_ignored_not_forwarded_twice() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };
    let Decide::Forward(true, approvals) = approvals.decide(r.clone(), Decision::Approve) else {
        panic!();
    };

    assert!(matches!(
        approvals.decide(r, Decision::Approve),
        Decide::Ignore(_)
    ));
}

#[test]
fn with_nothing_pending_every_decision_is_ignored() {
    assert!(matches!(
        Approvals::new().decide(id(), Decision::ApproveAlways),
        Decide::Ignore(_)
    ));
}

// ---- reset/1 ----

#[test]
fn clears_the_pending_approval() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };
    let approvals = approvals.reset();

    assert_eq!(approvals.pending, None);
    assert!(matches!(
        approvals.decide(r, Decision::Approve),
        Decide::Ignore(_)
    ));
}

#[test]
fn standing_approvals_survive_they_are_session_scoped_not_run_scoped() {
    let approvals = granted("mix test").reset();

    assert!(matches!(
        approvals.request(id(), "mix test"),
        Request::Auto(_)
    ));
}

// ---- ApprovalMode + cycle_mode + mode-aware request (ADR-0050) ----

// The Shift+Tab cycle order IS qwen's APPROVAL_MODES order with a hard wrap:
// plan → default → auto-edit → auto → yolo → plan.
#[test]
fn cycle_walks_plan_default_auto_edit_auto_yolo_then_wraps() {
    let order = [
        ApprovalMode::Plan,
        ApprovalMode::Default,
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Yolo,
        ApprovalMode::Plan,
    ];
    for pair in order.windows(2) {
        assert_eq!(pair[0].cycle(), pair[1], "{:?} → {:?}", pair[0], pair[1]);
    }
}

#[test]
fn a_fresh_approvals_defaults_to_default_mode() {
    assert_eq!(Approvals::new().mode, ApprovalMode::Default);
}

// cycle_mode folds the state and reports the mode it landed on, from Default.
#[test]
fn cycle_mode_rotates_and_reports_the_new_mode() {
    let (approvals, mode) = Approvals::new().cycle_mode();
    assert_eq!(mode, ApprovalMode::AutoEdit);
    assert_eq!(approvals.mode, ApprovalMode::AutoEdit);
}

// Yolo mode auto-approves EVERY gated Call, no modal, no standing entry.
#[test]
fn yolo_mode_auto_approves_every_request_without_a_modal() {
    let mut approvals = Approvals::new();
    approvals.mode = ApprovalMode::Yolo;
    let Request::Auto(approvals) = approvals.request(id(), "rm -rf /") else {
        panic!("expected auto under yolo");
    };
    assert_eq!(approvals.pending, None);
    // Auto did NOT record a standing approval: dropping out of yolo re-gates.
    let mut approvals = approvals;
    approvals.mode = ApprovalMode::Default;
    assert!(matches!(
        approvals.request(id(), "rm -rf /"),
        Request::Pending(_)
    ));
}

// Plan/AutoEdit/Auto are behavior-stubbed: they gate exactly like Default.
#[test]
fn plan_auto_edit_and_auto_gate_exactly_like_default() {
    for mode in [
        ApprovalMode::Plan,
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Default,
    ] {
        let mut approvals = Approvals::new();
        approvals.mode = mode;
        assert!(
            matches!(approvals.request(id(), "mix test"), Request::Pending(_)),
            "{mode:?} must gate like Default"
        );
    }
}

// Cycling the mode while an Approval is pending leaves that Approval whole.
#[test]
fn cycling_the_mode_does_not_disturb_the_pending_approval() {
    let r = id();
    let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };
    let pending_before = approvals.pending.clone();
    let (approvals, _mode) = approvals.cycle_mode();
    assert_eq!(approvals.pending, pending_before);
    // The still-pending decision still forwards to the same waiting Run.
    assert!(matches!(
        approvals.decide(r, Decision::Approve),
        Decide::Forward(true, _)
    ));
}
