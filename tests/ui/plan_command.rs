use super::*;
use crate::ui::slash;

// --- decide (the six qwen planCommand.ts branches) ----------------------

// `/plan exit` outside Plan mode: the error message, no mode change, no submit.
#[test]
fn exit_outside_plan_is_the_not_in_plan_error() {
    assert_eq!(
        decide(ApprovalMode::Default, Some("exit")),
        PlanAction::Message {
            text: NOT_IN_PLAN_MODE,
            is_error: true,
        }
    );
    // The exact bytes qwen emits (planCommand.ts:45).
    assert_eq!(
        NOT_IN_PLAN_MODE,
        "Not in plan mode. Use \"/plan\" to enter plan mode first."
    );
}

// `/plan exit` in Plan mode: the Exit action (restore + info line).
#[test]
fn exit_in_plan_restores_the_previous_mode() {
    assert_eq!(decide(ApprovalMode::Plan, Some("exit")), PlanAction::Exit);
    assert_eq!(
        EXITED_PLAN_MODE,
        "Exited plan mode. Previous approval mode restored."
    );
}

// `/plan` (no args) outside Plan: enter Plan, then the info line.
#[test]
fn no_args_outside_plan_enters_plan_mode() {
    assert_eq!(decide(ApprovalMode::Default, None), PlanAction::Enter);
    // A trailing-space-only remainder is still "no args" (qwen's args.trim()).
    assert_eq!(
        decide(ApprovalMode::Default, Some("   ")),
        PlanAction::Enter
    );
    assert_eq!(
        ENABLED_PLAN_MODE,
        "Enabled plan mode. The agent will analyze and plan without executing tools."
    );
}

// `/plan <prompt>` outside Plan: enter Plan AND submit the (trimmed) prompt.
#[test]
fn a_prompt_outside_plan_enters_then_submits() {
    assert_eq!(
        decide(ApprovalMode::Default, Some("  refactor the parser  ")),
        PlanAction::EnterThenSubmit("refactor the parser".to_string())
    );
}

// `/plan` (no args) already in Plan: the "already in plan" info line, no change.
#[test]
fn no_args_already_in_plan_is_the_already_in_plan_info() {
    assert_eq!(
        decide(ApprovalMode::Plan, None),
        PlanAction::Message {
            text: ALREADY_IN_PLAN_MODE,
            is_error: false,
        }
    );
    assert_eq!(
        ALREADY_IN_PLAN_MODE,
        "Already in plan mode. Use \"/plan exit\" to exit plan mode."
    );
}

// `/plan <prompt>` already in Plan: submit the prompt, no mode change.
#[test]
fn a_prompt_already_in_plan_just_submits() {
    assert_eq!(
        decide(ApprovalMode::Plan, Some("keep going")),
        PlanAction::Submit("keep going".to_string())
    );
}

// Entering from Yolo takes the same enter branch: `decide` is over the mode only,
// and Yolo is not Plan, so it enters (the user-request YOLO override is the
// Agent's concern, exercised in agent.rs).
#[test]
fn no_args_from_yolo_enters_plan() {
    assert_eq!(decide(ApprovalMode::Yolo, None), PlanAction::Enter);
}

// --- palette registration ----------------------------------------------

// `/plan` appears in the built-in registry with qwen's verbatim description and
// is fire-and-run (no selector).
#[test]
fn plan_is_registered_fire_and_run_with_the_qwen_description() {
    let cmd = slash::lookup("plan").expect("/plan is registered");
    assert_eq!(cmd.help, "Switch to plan mode or exit plan mode");
    assert!(!cmd.opens_selector, "/plan is fire-and-run, not a selector");
}
