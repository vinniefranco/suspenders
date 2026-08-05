use super::*;
use std::sync::{Arc, Mutex};

use crate::content::ContentBlock;
use crate::llm::LlmRequest;
use crate::llm::model::Api;
use crate::llm::response::{Response, StopReason as RespStop};
use crate::run::settlement::Reason;
use crate::session::{SessionConfig, SessionOpts};
use crate::subagents::{ToolSelector, subagent_tools};
use crate::test_support::{Entry, FakeLlm};
// `super::*` already brings the canonical `crate::stop_reason::StopReason` the
// outcome mapping uses.

fn session() -> Session {
    let opts = SessionOpts {
        root: Some(std::env::temp_dir().to_string_lossy().to_string()),
        ..SessionOpts::default()
    };
    Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
}

fn model() -> Model {
    Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
}

fn text_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: RespStop::EndTurn,
        usage: Default::default(),
        error: None,
    }
}

fn tool_use_response(id: &str, name: &str, input: serde_json::Value) -> Response {
    Response {
        content: vec![ContentBlock::tool_use(id, name, input)],
        stop_reason: RespStop::ToolUse,
        usage: Default::default(),
        error: None,
    }
}

fn request(llm: Arc<dyn Llm>, sink: Option<ChildSink>) -> ChildRunRequest {
    ChildRunRequest {
        model: model(),
        llm,
        system_prompt: "You are a subagent.".to_string(),
        tools: subagent_tools(&ToolSelector::All),
        prompt: "do the task".to_string(),
        max_turns: 5,
        temperature: None,
        thinking_budget: None,
        tool_call_style: ToolCallStyle::default(),
        session: session(),
        sink,
        depth: 1,
    }
}

#[tokio::test]
async fn a_final_text_pass_settles_as_goal_with_that_text() {
    let fake = FakeLlm::script(vec![Entry::just(text_response("the answer"))]);
    let out = run_child(request(Arc::new(fake), None)).await;
    assert_eq!(out.terminate_reason, "GOAL");
    assert_eq!(out.result, "the answer");
}

#[tokio::test]
async fn a_tool_use_pass_then_final_text_settles_as_goal() {
    // One tool_use (todo_write) then a final text: the child loops once,
    // answers the call, then finishes on the text.
    let fake = FakeLlm::script(vec![
        Entry::just(tool_use_response(
            "call-1",
            "todo_write",
            serde_json::json!({"todos": [{"content": "step", "status": "pending"}]}),
        )),
        Entry::just(text_response("done exploring")),
    ]);
    let out = run_child(request(Arc::new(fake), None)).await;
    assert_eq!(out.terminate_reason, "GOAL");
    assert_eq!(out.result, "done exploring");
}

#[tokio::test]
async fn a_child_never_sees_the_agent_tool_the_recursion_guard() {
    // Guard 1 (the tool subset): `subagent_tools(All)` drops `agent`, so the
    // child's wire tool list never carries the delegation tool - the model
    // cannot even name it. Capture the first Pass's wire request and assert.
    let saw_agent = Arc::new(Mutex::new(None::<bool>));
    let saw = Arc::clone(&saw_agent);
    let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &LlmRequest, _m| {
        if saw.lock().unwrap().is_none() {
            let present = req.tools.iter().any(|t| t.name == "agent");
            *saw.lock().unwrap() = Some(present);
        }
        text_response("done")
    })]);
    let out = run_child(request(Arc::new(fake), None)).await;
    assert_eq!(out.terminate_reason, "GOAL");
    assert!(
        !saw_agent.lock().unwrap().expect("the child made a request"),
        "the child's wire list must not carry the `agent` tool"
    );
}

#[tokio::test]
async fn a_child_agent_call_folds_the_unavailable_err_the_recursion_guard() {
    // Guard 2 (defence in depth): even if the child's tool subset DID carry
    // an `agent` tool (bypass the exclusion by adding it explicitly), its
    // `spawn` reaches the child's `UnavailableSubagentSpawner`, so a nested
    // `agent` call fails rather than spawning a grandchild. The child emits an
    // `agent` tool_use on Pass 1; the (error) tool result rides into Pass 2's
    // wire messages, where we assert the verbatim Unavailable string.
    use crate::content::ContentBlock as CB;
    use crate::subagents::builtins;
    use crate::tools::agent::AgentTool;

    let seen_result = Arc::new(Mutex::new(None::<String>));
    let seen = Arc::clone(&seen_result);
    let fake = FakeLlm::script(vec![
        // Pass 1: the child model emits a nested `agent` tool_use.
        Entry::just(tool_use_response(
            "call-1",
            "agent",
            serde_json::json!({
                "description": "nested spawn",
                "prompt": "recurse",
                "subagent_type": "general-purpose",
            }),
        )),
        // Pass 2: scan the wire messages for the nested call's tool result.
        Entry::dynamic(vec![], move |req: &LlmRequest, _m| {
            for msg in &req.messages {
                for block in &msg.content {
                    if let CB::ToolResult { content, .. } = block {
                        let text = crate::content::result_blocks_text(content.as_slice());
                        if !text.is_empty() {
                            *seen.lock().unwrap() = Some(text);
                        }
                    }
                }
            }
            text_response("gave up")
        }),
    ]);

    // Build the child tools = the All subset PLUS an AgentTool bolted on, so
    // the exclusion is deliberately bypassed to exercise the second guard.
    let mut tools = subagent_tools(&ToolSelector::All);
    tools.push(Box::new(AgentTool::new(Arc::new(
        crate::subagents::SubagentRegistry::new(builtins()),
    ))));
    let mut req = request(Arc::new(fake), None);
    req.tools = tools;

    let out = run_child(req).await;
    assert_eq!(out.result, "gave up");
    let result_text = seen_result
        .lock()
        .unwrap()
        .clone()
        .expect("the child's `agent` call produced a tool result");
    assert!(
        result_text.contains("subagents are unavailable in this environment"),
        "a nested `agent` call must fold the UnavailableSubagentSpawner Err, got: {result_text}"
    );
}

#[tokio::test]
async fn a_child_emits_nothing_to_a_no_op_sink_and_can_feed_a_supplied_one() {
    // Foreground path (sink None): the child's whole run is invisible - no
    // parent channel is touched. A supplied sink DOES receive the child's
    // events (the background path's seam), proving the emitter routing.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let sink: ChildSink = Box::new(move |event| sink_seen.lock().unwrap().push(event));
    let fake = FakeLlm::script(vec![Entry::just(text_response("hi"))]);
    let out = run_child(request(Arc::new(fake), Some(sink))).await;
    assert_eq!(out.terminate_reason, "GOAL");
    // The child produced at least a message grammar into the supplied sink.
    assert!(!seen.lock().unwrap().is_empty());
}

#[test]
fn outcome_run_limit_maps_to_max_turns() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
    conv.add_assistant_response(
        vec![ContentBlock::text("real work so far")],
        model().provenance(),
    );
    // The Loop appends a marker-only assistant message on a Run-Limit close.
    conv.add_assistant_blocks(vec![ContentBlock::text(
        crate::voice::Marker::RunLimit.text(),
    )]);
    let out = outcome_to_result(Outcome::Ok(conv, StopReason::RunLimit));
    assert_eq!(out.terminate_reason, "MAX_TURNS");
    // The trailing pure-marker message is skipped; the real text is the answer.
    assert_eq!(out.result, "real work so far");
}

#[test]
fn outcome_failed_maps_to_error_with_partial_text() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
    conv.add_assistant_response(vec![ContentBlock::text("partial")], model().provenance());
    conv.add_assistant_blocks(vec![ContentBlock::text(
        crate::voice::Marker::RunFailed.text(),
    )]);
    let out = outcome_to_result(Outcome::Failed(Reason::verbatim("boom"), conv));
    assert_eq!(out.terminate_reason, "ERROR");
    assert_eq!(out.result, "partial");
}

#[test]
fn outcome_budget_error_maps_to_error_with_no_text() {
    let out = outcome_to_result(Outcome::Error(Reason::atom("context_budget_exhausted")));
    assert_eq!(out.terminate_reason, "ERROR");
    assert_eq!(out.result, "");
}

#[test]
fn outcome_stuck_loop_maps_to_error() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
    conv.add_assistant_blocks(vec![ContentBlock::text(
        crate::voice::Marker::LoopStall.text(),
    )]);
    let out = outcome_to_result(Outcome::Ok(conv, StopReason::RunLimitStuck));
    assert_eq!(out.terminate_reason, "ERROR");
}
