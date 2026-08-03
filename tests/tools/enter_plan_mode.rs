use super::*;
use crate::tool::caps::{Capabilities, EnterPlanOutcome, PlanExitOutcome, PlanMode};
use crate::tool_registry::ToolRegistry;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// PlanExitOutcome is referenced only in the ScriptedPlanMode's unreachable
// request_exit arm below (a compile-time obligation of the trait).

// A scripted PlanMode that returns a fixed enter-outcome and records the
// `user_requested` flag it received, standing in for the tx-backed AgentPlanMode.
struct ScriptedPlanMode {
    outcome: EnterPlanOutcome,
    saw_user_requested: AtomicBool,
    was_called: AtomicBool,
}

impl ScriptedPlanMode {
    fn new(outcome: EnterPlanOutcome) -> Arc<Self> {
        Arc::new(ScriptedPlanMode {
            outcome,
            saw_user_requested: AtomicBool::new(false),
            was_called: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl PlanMode for ScriptedPlanMode {
    async fn enter(&self, user_requested: bool) -> EnterPlanOutcome {
        self.was_called.store(true, Ordering::SeqCst);
        self.saw_user_requested.store(user_requested, Ordering::SeqCst);
        self.outcome.clone()
    }

    async fn request_exit(&self, _plan: String) -> PlanExitOutcome {
        unreachable!("enter_plan_mode never calls request_exit")
    }
}

fn ctx_with(plan_mode: Arc<ScriptedPlanMode>) -> ToolCtx {
    let caps =
        Capabilities::for_test_with_plan_mode(plan_mode as Arc<dyn PlanMode>);
    let mut ctx = ToolCtx::for_test("/nowhere".into(), 100_000);
    ctx.caps = caps;
    ctx
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    EnterPlanMode.run(&input, ctx).await
}

#[test]
fn spec_is_enter_plan_mode_kind_think_and_always_visible() {
    let spec = EnterPlanMode.spec();
    assert_eq!(spec.name, "enter_plan_mode");
    // Kind::Think (qwen enterPlanMode.ts `Kind.Think`).
    assert_eq!(EnterPlanMode.kind(), crate::approvals::Kind::Think);
    // Always-visible (qwen shouldDefer:false): an explicit plan-mode request must
    // be able to reach it on the wire list.
    assert!(!EnterPlanMode.should_defer());
    assert!(!EnterPlanMode.always_load());
    // The single optional `userRequested` boolean, additionalProperties false.
    let props = &spec.input_schema["properties"];
    assert!(props["userRequested"].is_object());
    assert_eq!(spec.input_schema["additionalProperties"], serde_json::json!(false));
}

// The happy path: a user-requested entry returns the plan-mode reminder VERBATIM
// (the Agent hands it back).
#[tokio::test]
async fn entered_returns_the_reminder_verbatim() {
    let reminder = crate::voice::plan_mode_reminder().to_string();
    let pm = ScriptedPlanMode::new(EnterPlanOutcome::Entered {
        reminder: reminder.clone(),
    });
    let ctx = ctx_with(pm.clone());

    let out = run(json!({ "userRequested": true }), &ctx).await.unwrap();
    assert_eq!(out, reminder, "the tool returns the plan-mode reminder verbatim");
    assert!(pm.saw_user_requested.load(Ordering::SeqCst));
}

// exit_plan_mode is always-declared (qwen's always_load, #5210), so its schema is
// already on the base wire list - no reveal is needed on plan entry. This pins
// that faithful behavior (the reveal qwen calls is a no-op under always_load).
#[test]
fn exit_plan_mode_is_always_declared_so_no_reveal_is_needed() {
    let registry = ToolRegistry::new(crate::tools::tools());
    // Not "loadable" (deferred AND not always_load) - it is always-declared, so a
    // reveal would be a no-op. It rides the base wire list already.
    assert!(!registry.is_loadable("exit_plan_mode"));
    assert!(crate::tools::specs().iter().any(|s| s.name == "exit_plan_mode"));
}

// A model-initiated entry (userRequested absent -> false) is passed through as
// false, and the StayedYolo no-op returns qwen's VERBATIM guidance.
#[tokio::test]
async fn stayed_yolo_returns_the_verbatim_guidance() {
    let pm = ScriptedPlanMode::new(EnterPlanOutcome::StayedYolo);
    let ctx = ctx_with(pm.clone());

    let out = run(json!({}), &ctx).await.unwrap();
    assert!(!pm.saw_user_requested.load(Ordering::SeqCst), "defaults to false");
    assert_eq!(
        out,
        "Plan mode was not entered: the session is in YOLO mode, which the user explicitly chose for low-friction execution. Continue investigating and presenting your plan in the current mode without switching. If the user explicitly asked for plan mode in this turn, retry this tool call with userRequested: true."
    );
}

// Inside a subagent the degraded capability returns SubagentBlocked, which the
// tool words with qwen's VERBATIM block message.
#[tokio::test]
async fn subagent_blocked_returns_the_verbatim_block_message() {
    let pm = ScriptedPlanMode::new(EnterPlanOutcome::SubagentBlocked);
    let ctx = ctx_with(pm);

    let out = run(json!({ "userRequested": true }), &ctx).await.unwrap();
    assert_eq!(
        out,
        "enter_plan_mode is not available inside subagents or team agents. Plan mode is owned by the caller/main session; return your plan, findings, or constraints to the caller in your normal response instead of entering or exiting plan mode."
    );
}
