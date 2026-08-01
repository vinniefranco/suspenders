
use super::*;
use crate::content::ContentBlock;

fn model() -> Model {
    Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
}

fn request() -> LlmRequest {
    LlmRequest::new("s", vec![], vec![])
}

#[tokio::test]
async fn response_entry_fires_deltas_and_returns() {
    let fake = FakeLlm::script(vec![Entry::response(
        vec![Delta::Text("hi".into())],
        Response {
            content: vec![ContentBlock::text("hi")],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            error: None,
        },
    )]);

    let mut deltas = Vec::new();
    let mut on_event = |ev: &StreamEvent| deltas.push(ev.delta.clone());
    let r = fake.complete(&request(), &model(), &mut on_event).await;

    assert_eq!(deltas, vec![Delta::Text("hi".into())]);
    assert_eq!(r.content, vec![ContentBlock::text("hi")]);
}

#[tokio::test]
async fn error_entry_becomes_error_response() {
    let fake = FakeLlm::script(vec![Entry::error("nope")]);
    let mut on_event = |_ev: &StreamEvent| {};
    let r = fake.complete(&request(), &model(), &mut on_event).await;
    assert_eq!(r.stop_reason, StopReason::Error);
    assert_eq!(r.error.as_deref(), Some("nope"));
}

#[tokio::test]
async fn dynamic_entry_inspects_the_typed_request_and_model() {
    let fake = FakeLlm::script(vec![Entry::dynamic(
        vec![],
        |req: &LlmRequest, model: &Model| {
            let line = format!("{}@{}", req.system, model.scoped_id());
            Response {
                content: vec![ContentBlock::text(line)],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                error: None,
            }
        },
    )]);
    let mut on_event = |_ev: &StreamEvent| {};
    let r = fake.complete(&request(), &model(), &mut on_event).await;
    assert_eq!(r.content, vec![ContentBlock::text("s@local/m")]);
}
