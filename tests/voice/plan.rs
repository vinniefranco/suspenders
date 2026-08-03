use super::*;

// The plan-mode reminder is VERBATIM qwen (getPlanModeSystemReminder, planOnly
// false). Pin the load-bearing lines rather than the whole block, so a wording
// drift in the read-only invariant or the exit_plan_mode convergence line is a
// test failure.
#[test]
fn plan_mode_reminder_carries_qwens_read_only_invariant() {
    let r = plan_mode_reminder();
    assert!(r.starts_with("<system-reminder>\nPlan mode is active."));
    assert!(r.contains(
        "you MUST NOT make any edits, run tools classified as state-modifying"
    ));
    // The planOnly=false convergence line names the exit_plan_mode tool.
    assert!(r.contains(
        "Present your plan by calling the exit_plan_mode tool, which will prompt the user to confirm the plan."
    ));
    // The interpolated tool names match suspenders' verbatim.
    assert!(r.contains("read-only tools (read_file, grep_search, glob)"));
    assert!(r.contains("use ask_user_question"));
    assert!(r.ends_with("</system-reminder>"));
}

// The manual-exit reminder interpolates the current mode exactly as qwen's
// ${currentMode}.
#[test]
fn manual_plan_exit_reminder_interpolates_the_current_mode() {
    let r = manual_plan_exit_reminder("auto-edit");
    assert!(r.contains("The approval mode changed outside the approved exit_plan_mode flow."));
    assert!(r.contains("The current approval mode is: auto-edit."));
    assert!(r.contains("Plan mode is no longer active."));
    assert!(r.ends_with("</system-reminder>"));
}
