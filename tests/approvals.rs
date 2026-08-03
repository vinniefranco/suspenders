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

// ---- ApprovalMode cycle + mode-aware request (ADR-0050) ----

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

// `request` is the Agent-side pending-modal fold (reached only on a `classify`
// Ask that round-trips): it is mode-agnostic except for Yolo/Standing, so
// Plan/AutoEdit/Auto all become Pending here exactly like Default. Plan's
// read-only enforcement lives in `classify` (tested above), NOT in `request`.
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

// ---- classify: the one mode-aware verdict fold (ADR-0067) ----

// A mutating edit/write in DEFAULT mode is not gated and not blocked: it Allows
// (edits are ungated in suspenders - ADR-0050), so classify runs it straight
// through outside plan mode.
#[test]
fn classify_allows_a_mutating_tool_outside_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Default);
    assert_eq!(
        approvals.classify("edit", Kind::Edit, &json!({"path": "a.rs"})),
        Verdict::Allow
    );
    assert_eq!(
        approvals.classify("write_file", Kind::Edit, &json!({"path": "a.rs"})),
        Verdict::Allow
    );
}

// A read-only tool always Allows outside plan mode (it never gates).
#[test]
fn classify_allows_read_only_tools_outside_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Default);
    for (name, kind) in [
        ("read_file", Kind::Read),
        ("grep_search", Kind::Search),
        ("list_directory", Kind::Search),
    ] {
        assert_eq!(
            approvals.classify(name, kind, &json!({})),
            Verdict::Allow,
            "{name} must Allow"
        );
    }
}

// A gated tool with no cover, outside plan mode, Asks over the gate text.
#[test]
fn classify_asks_a_gated_tool_with_no_cover() {
    let approvals = Approvals::with_mode(ApprovalMode::Default);
    assert_eq!(
        approvals.classify(
            "run_shell_command",
            Kind::Execute,
            &json!({"command": "mix test"})
        ),
        Verdict::Ask("mix test".to_string())
    );
    // web_fetch asks over its DOMAIN.
    assert_eq!(
        approvals.classify("web_fetch", Kind::Fetch, &json!({"url": "https://docs.rs/x"})),
        Verdict::Ask("docs.rs".to_string())
    );
}

// Yolo mode auto-approves a gated Call: classify Allows it (no modal).
#[test]
fn classify_allows_a_gated_tool_under_yolo() {
    let approvals = Approvals::with_mode(ApprovalMode::Yolo);
    assert_eq!(
        approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "rm -rf /"})),
        Verdict::Allow
    );
}

// A covering Standing Approval Allows a gated Call outside plan mode.
#[test]
fn classify_allows_a_gated_tool_with_a_covering_standing_approval() {
    let mut approvals = granted("mix test");
    approvals.mode = ApprovalMode::Default;
    assert_eq!(
        approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "mix test"})),
        Verdict::Allow
    );
    // A different command still Asks.
    assert_eq!(
        approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "mix other"})),
        Verdict::Ask("mix other".to_string())
    );
}

// PLAN MODE blocks every mutating / non-read-only Kind with qwen's verbatim
// message and no modal. `run_shell_command` (also a mutator Kind) is the ONE
// exception: it routes to the plan-mode shell classifier instead of the wholesale
// block (see the classify_*_shell_in_plan_mode tests), so it is excluded here.
#[test]
fn classify_blocks_mutating_and_other_kinds_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    for (name, kind) in [
        ("edit", Kind::Edit),
        ("write_file", Kind::Edit),
        ("notebook_edit", Kind::Edit),
        ("agent", Kind::Agent),
        ("task_stop", Kind::Other),
        ("some_mcp_tool", Kind::Other),
    ] {
        let verdict = approvals.classify(name, kind, &json!({}));
        let Verdict::Block(reason) = verdict else {
            panic!("{name} ({kind:?}) must Block in plan mode, got {verdict:?}");
        };
        // qwen's verbatim message, with the tool name interpolated.
        assert!(
            reason.starts_with(&format!(
                "Tool blocked by plan mode: \"{name}\" is not a read-only tool."
            )),
            "block reason must be qwen-verbatim for {name}: {reason}"
        );
        assert!(reason.contains("Only read-only tools (read_file, grep_search, glob, list_directory, web_fetch, etc.) are allowed in plan mode."));
        assert!(reason.contains("Do NOT retry this tool."));
        assert!(reason.contains("call exit_plan_mode with a plan that covers this tool's purpose."));
    }
}

// PLAN MODE allows read-only Kinds (Read/Search/Fetch/Think): they fold exactly
// like the non-plan path.
#[test]
fn classify_allows_read_only_kinds_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    for (name, kind) in [
        ("read_file", Kind::Read),
        ("grep_search", Kind::Search),
        ("list_directory", Kind::Search),
        ("todo_write", Kind::Think),
        ("ask_user_question", Kind::Think),
        ("skill", Kind::Read),
    ] {
        assert_eq!(
            approvals.classify(name, kind, &json!({})),
            Verdict::Allow,
            "{name} ({kind:?}) must Allow in plan mode"
        );
    }
}

// PLAN MODE + web_fetch (Kind::Fetch, a gated read-only tool): NOT blocked, still
// Asks - qwen keeps web_fetch's confirmation in plan mode (its confirmation is
// `type: 'info'`, which `isPlanModeBlocked` does not block).
#[test]
fn classify_web_fetch_in_plan_mode_still_asks_not_blocked() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    assert_eq!(
        approvals.classify(
            "web_fetch",
            Kind::Fetch,
            &json!({"url": "https://docs.rs/tokio"})
        ),
        Verdict::Ask("docs.rs".to_string())
    );
}

// PLAN MODE + run_shell_command runs the plan-mode shell classifier (ADR-0067
// Phase 4b, qwen plan-mode-shell-policy.ts), NOT a wholesale block: a read-only
// command is Allowed, a state-modifying one is Blocked, an indeterminate one Asks.
// The classifier itself is pinned in tests/shell_safety.rs; these pin the seam's
// three-valued -> Verdict mapping through `classify` in Plan mode.

#[test]
fn classify_allows_read_only_shell_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    for command in ["ls -la", "cat f", "git status", "grep -r x ."] {
        assert_eq!(
            approvals.classify("run_shell_command", Kind::Execute, &json!({ "command": command })),
            Verdict::Allow,
            "read-only shell command should be allowed in plan mode: {command}"
        );
    }
}

#[test]
fn classify_blocks_write_shell_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    for command in [
        "rm f",
        "git commit -m x",
        "sed -i s/a/b/ f",
        "echo x > f",
        "mv a b",
        "ls && rm f",
        "git status; git push",
        "echo $(rm f)",
    ] {
        let verdict = approvals.classify(
            "run_shell_command",
            Kind::Execute,
            &json!({ "command": command }),
        );
        assert!(
            matches!(verdict, Verdict::Block(_)),
            "state-modifying shell command should be blocked in plan mode: {command} -> {verdict:?}"
        );
    }
}

#[test]
fn classify_write_shell_block_message_is_qwen_verbatim() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    let verdict = approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "rm f"}));
    // plan-mode-shell-policy.ts:25 WRITE_BLOCK_MESSAGE, byte-verbatim.
    assert_eq!(
        verdict,
        Verdict::Block(
            "Plan mode blocked this shell command because it was classified as state-modifying. \
Do not retry it through wrappers or obfuscation; continue read-only investigation and \
include the action in the plan."
                .to_string()
        )
    );
}

#[test]
fn classify_asks_unknown_shell_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    // an unrecognized binary classifies unknown -> Ask over the raw command (the
    // approval surface qwen decorates for the unknown path).
    assert_eq!(
        approvals.classify(
            "run_shell_command",
            Kind::Execute,
            &json!({"command": "frobnicate --do-thing"})
        ),
        Verdict::Ask("frobnicate --do-thing".to_string())
    );
}

#[test]
fn classify_read_only_pipeline_allowed_but_write_pipeline_blocked_in_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    assert_eq!(
        approvals.classify(
            "run_shell_command",
            Kind::Execute,
            &json!({"command": "cat a | grep b"})
        ),
        Verdict::Allow
    );
    assert!(matches!(
        approvals.classify(
            "run_shell_command",
            Kind::Execute,
            &json!({"command": "cat a | tee b"})
        ),
        Verdict::Block(_)
    ));
}

// Outside plan mode the shell classifier does NOT run: a shell Call gates
// normally (Ask over the command) regardless of read-only/write classification.
#[test]
fn classify_does_not_run_shell_classifier_outside_plan_mode() {
    let approvals = Approvals::with_mode(ApprovalMode::Default);
    assert_eq!(
        approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "ls -la"})),
        Verdict::Ask("ls -la".to_string())
    );
    assert_eq!(
        approvals.classify("run_shell_command", Kind::Execute, &json!({"command": "rm f"})),
        Verdict::Ask("rm f".to_string())
    );
}

// An unknown tool defaults to Kind::Other (the trait default), so plan mode
// blocks it - the fail-safe: a mis-declared tool is blocked, not slipped through.
#[test]
fn classify_blocks_an_other_kind_in_plan_mode_fail_safe() {
    let approvals = Approvals::with_mode(ApprovalMode::Plan);
    assert!(matches!(
        approvals.classify("mystery", Kind::Other, &json!({})),
        Verdict::Block(_)
    ));
}

// ---- AtomicApprovalMode: the shared live-mode mirror (ADR-0067) ----

#[test]
fn atomic_mode_defaults_to_the_fold_default() {
    assert_eq!(AtomicApprovalMode::default().load(), ApprovalMode::Default);
}

#[test]
fn atomic_mode_round_trips_every_mode() {
    let mirror = AtomicApprovalMode::default();
    for mode in [
        ApprovalMode::Plan,
        ApprovalMode::Default,
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Yolo,
    ] {
        mirror.store(mode);
        assert_eq!(mirror.load(), mode, "mirror must round-trip {mode:?}");
    }
}

// The mirror reflects a store immediately (the loop reads the latest write).
#[test]
fn atomic_mode_reflects_the_latest_store() {
    let mirror = AtomicApprovalMode::new(ApprovalMode::Default);
    mirror.store(ApprovalMode::Plan);
    assert_eq!(mirror.load(), ApprovalMode::Plan);
    mirror.store(ApprovalMode::Yolo);
    assert_eq!(mirror.load(), ApprovalMode::Yolo);
}

// Cycling the mode while an Approval is pending leaves that Approval whole. The
// production cycle (Agent's `set_approval_mode`) mutates `mode` in place via the
// enum `cycle()` and never touches `pending`/`standing`; this pins that
// invariant on the pure `Approvals` fold.
#[test]
fn cycling_the_mode_does_not_disturb_the_pending_approval() {
    let r = id();
    let Request::Pending(mut approvals) = Approvals::new().request(r.clone(), "mix test") else {
        panic!();
    };
    let pending_before = approvals.pending.clone();
    approvals.mode = approvals.mode.cycle();
    assert_eq!(approvals.pending, pending_before);
    // The still-pending decision still forwards to the same waiting Run.
    assert!(matches!(
        approvals.decide(r, Decision::Approve),
        Decide::Forward(true, _)
    ));
}

// ---- PendingManualPlanExit: the one-shot manual-exit notice carrier (ADR-0067)

// A fresh carrier holds no notice.
#[test]
fn pending_manual_exit_starts_empty() {
    assert_eq!(PendingManualPlanExit::default().take(), None);
    assert_eq!(PendingManualPlanExit::empty().take(), None);
}

// A queued notice is delivered on the first take and gone on the next: the
// one-shot semantics the loop relies on to inject the reminder into exactly one
// request.
#[test]
fn pending_manual_exit_is_one_shot() {
    let carrier = PendingManualPlanExit::empty();
    carrier.set(ApprovalMode::Default);
    assert_eq!(carrier.take(), Some(ApprovalMode::Default));
    assert_eq!(carrier.take(), None, "a taken notice must not repeat");
}

// The carrier round-trips the exact mode Plan was left FOR (the reminder
// interpolates it), for every mode.
#[test]
fn pending_manual_exit_round_trips_every_mode() {
    let carrier = PendingManualPlanExit::empty();
    for mode in [
        ApprovalMode::Plan,
        ApprovalMode::Default,
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Yolo,
    ] {
        carrier.set(mode);
        assert_eq!(carrier.take(), Some(mode), "must carry {mode:?}");
    }
}

// A second manual exit before the first is taken overwrites with the newer mode:
// the reminder always names the CURRENT mode, so the latest exit is correct.
#[test]
fn pending_manual_exit_keeps_the_latest_when_reset_before_take() {
    let carrier = PendingManualPlanExit::empty();
    carrier.set(ApprovalMode::Default);
    carrier.set(ApprovalMode::Yolo);
    assert_eq!(carrier.take(), Some(ApprovalMode::Yolo));
    assert_eq!(carrier.take(), None);
}
