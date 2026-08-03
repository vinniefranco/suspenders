use super::*;
use crate::tool::caps::{Capabilities, EnterPlanOutcome, PlanExitOutcome, PlanMode};
use std::sync::Arc;
use std::sync::Mutex;

// A scripted PlanMode that returns a fixed exit-outcome and records the plan text
// it received, standing in for the tx-backed AgentPlanMode.
struct ScriptedPlanMode {
    outcome: PlanExitOutcome,
    saw_plan: Mutex<Option<String>>,
}

impl ScriptedPlanMode {
    fn new(outcome: PlanExitOutcome) -> Arc<Self> {
        Arc::new(ScriptedPlanMode {
            outcome,
            saw_plan: Mutex::new(None),
        })
    }
}

#[async_trait::async_trait]
impl PlanMode for ScriptedPlanMode {
    async fn enter(&self, _user_requested: bool) -> EnterPlanOutcome {
        unreachable!("exit_plan_mode never calls enter")
    }

    async fn request_exit(&self, plan: String) -> PlanExitOutcome {
        *self.saw_plan.lock().unwrap() = Some(plan);
        self.outcome.clone()
    }
}

fn ctx_with(plan_mode: Arc<ScriptedPlanMode>) -> ToolCtx {
    let caps = Capabilities::for_test_with_plan_mode(plan_mode as Arc<dyn PlanMode>);
    let mut ctx = ToolCtx::for_test("/nowhere".into(), 100_000);
    ctx.caps = caps;
    ctx
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    ExitPlanMode.run(&input, ctx).await
}

#[test]
fn spec_is_exit_plan_mode_kind_think_deferred_and_always_declared() {
    let spec = ExitPlanMode.spec();
    assert_eq!(spec.name, "exit_plan_mode");
    assert_eq!(ExitPlanMode.kind(), crate::approvals::Kind::Think);
    // Deferred + always-declared (qwen shouldDefer:true + always-declared, #5210):
    // discoverable and revealed by enter_plan_mode.
    assert!(ExitPlanMode.should_defer());
    assert!(ExitPlanMode.always_load());
    // required plan; optional originalRequest/researchSummary kept for the OMITTED
    // team path; resolutionSummary OMITTED.
    let props = &spec.input_schema["properties"];
    assert!(props["plan"].is_object());
    assert!(props["originalRequest"].is_object());
    assert!(props["researchSummary"].is_object());
    assert!(props.get("resolutionSummary").is_none());
    assert_eq!(spec.input_schema["required"], serde_json::json!(["plan"]));
}

// Empty plan is rejected with qwen's VERBATIM message, BEFORE reaching the
// capability (an Err / is_error result).
#[tokio::test]
async fn empty_plan_is_rejected_verbatim() {
    // A capability we can prove is never reached: it would panic in request_exit
    // only if called; the validation short-circuits first.
    let pm = ScriptedPlanMode::new(PlanExitOutcome::Approved);
    let ctx = ctx_with(pm.clone());

    let err = run(json!({ "plan": "   " }), &ctx).await.unwrap_err();
    assert_eq!(err, "Parameter \"plan\" must be a non-empty string.");
    // The capability was never consulted (the plan was rejected first).
    assert!(pm.saw_plan.lock().unwrap().is_none());

    // Missing plan too.
    let err = run(json!({}), &ctx).await.unwrap_err();
    assert_eq!(err, "Parameter \"plan\" must be a non-empty string.");
}

// Approved: the VERBATIM "User approved..." content, and the plan text reached
// the capability.
#[tokio::test]
async fn approved_returns_the_verbatim_content() {
    let pm = ScriptedPlanMode::new(PlanExitOutcome::Approved);
    let ctx = ctx_with(pm.clone());

    let out = run(json!({ "plan": "1. do the thing" }), &ctx).await.unwrap();
    assert_eq!(
        out,
        "User approved. You can now start coding. Start with updating your todo list if applicable."
    );
    assert_eq!(
        pm.saw_plan.lock().unwrap().as_deref(),
        Some("1. do the thing")
    );
}

#[tokio::test]
async fn cancel_returns_the_verbatim_remaining_in_plan_mode() {
    let pm = ScriptedPlanMode::new(PlanExitOutcome::Cancel);
    let ctx = ctx_with(pm);
    let out = run(json!({ "plan": "the plan" }), &ctx).await.unwrap();
    assert_eq!(out, "Plan execution was not approved. Remaining in plan mode.");
}

#[tokio::test]
async fn stale_returns_the_verbatim_stale_message() {
    let pm = ScriptedPlanMode::new(PlanExitOutcome::Stale);
    let ctx = ctx_with(pm);
    let out = run(json!({ "plan": "the plan" }), &ctx).await.unwrap();
    assert_eq!(
        out,
        "Plan approval is stale because the approval mode changed. No action was taken."
    );
}

// Outside plan mode: qwen's VERBATIM guidance error (an Err, not a deny), with
// the current mode interpolated.
#[tokio::test]
async fn not_in_plan_mode_returns_the_verbatim_guidance_error() {
    let pm = ScriptedPlanMode::new(PlanExitOutcome::NotInPlanMode {
        current_mode: "default".into(),
    });
    let ctx = ctx_with(pm);
    let err = run(json!({ "plan": "the plan" }), &ctx).await.unwrap_err();
    assert_eq!(
        err,
        "You are not in plan mode (current mode: default). The user may have manually switched modes via Shift+Tab or /approval-mode. Do not call exit_plan_mode again. Continue working in the current mode."
    );
}

#[tokio::test]
async fn subagent_blocked_returns_the_verbatim_block_message() {
    let pm = ScriptedPlanMode::new(PlanExitOutcome::SubagentBlocked);
    let ctx = ctx_with(pm);
    let out = run(json!({ "plan": "the plan" }), &ctx).await.unwrap();
    assert_eq!(
        out,
        "exit_plan_mode is not available inside subagents or team agents. Plan mode is owned by the caller/main session; return your plan, findings, or constraints to the caller in your normal response instead of entering or exiting plan mode."
    );
}
