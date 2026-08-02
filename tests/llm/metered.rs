use super::*;
use crate::content::{ContentBlock, Message, Usage};
use crate::llm::cost::Pricing;
use crate::llm::model::Api;
use crate::llm::response::StopReason;
use crate::test_support::{Entry, FakeLlm};

fn priced_model() -> Model {
    let mut model = Model::new("p", "m", Api::OpenaiCompletions, 64_000, 8_000);
    model.pricing = Some(Pricing {
        input: 10.0,
        output: 50.0,
        cache_read: None,
        cache_write: None,
    });
    model
}

fn response_with_usage(input: u64, output: u64) -> Response {
    Response {
        content: vec![ContentBlock::text("ok")],
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..Usage::default()
        },
        error: None,
    }
}

fn request() -> LlmRequest {
    LlmRequest::new(
        "s",
        vec![Message::user(vec![ContentBlock::text("hi")])],
        vec![],
    )
}

fn metered_over(entries: Vec<Entry>) -> (Metered, Arc<Mutex<Vec<f64>>>) {
    let totals: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&totals);
    let metered = Metered::new(Arc::new(FakeLlm::script(entries)), move |total| {
        sink.lock().unwrap().push(total)
    });
    (metered, totals)
}

fn no_op() -> impl FnMut(&crate::llm::StreamEvent) + Send {
    |_| {}
}

#[tokio::test]
async fn priced_responses_accumulate_across_calls() {
    // 1M in at $10 + 100K out at $50 = $15; twice = $30 cumulative.
    let (metered, totals) = metered_over(vec![
        Entry::just(response_with_usage(1_000_000, 100_000)),
        Entry::just(response_with_usage(1_000_000, 100_000)),
    ]);
    metered
        .complete(&request(), &priced_model(), &mut no_op())
        .await;
    metered
        .complete(&request(), &priced_model(), &mut no_op())
        .await;
    assert_eq!(*totals.lock().unwrap(), vec![15.0, 30.0]);
}

#[tokio::test]
async fn an_unpriced_model_never_fires_the_sink() {
    let (metered, totals) =
        metered_over(vec![Entry::just(response_with_usage(1_000_000, 100_000))]);
    let unpriced = Model::new("local", "m", Api::OpenaiCompletions, 64_000, 8_000);
    let response = metered.complete(&request(), &unpriced, &mut no_op()).await;
    assert_eq!(response.stop_reason, StopReason::EndTurn, "delegates whole");
    assert!(totals.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_priced_call_with_zero_usage_is_silent() {
    let (metered, totals) = metered_over(vec![Entry::just(response_with_usage(0, 0))]);
    metered
        .complete(&request(), &priced_model(), &mut no_op())
        .await;
    assert!(totals.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_models_delegates_untouched() {
    let (metered, totals) = metered_over(vec![]);
    let provider = Provider {
        id: "local".into(),
        base_url: "http://localhost:1234/v1".into(),
        token: "".into(),
        api: Api::OpenaiCompletions,
        context_window: None,
        custom: true,
    };
    assert_eq!(metered.list_models(&provider).await, Ok(vec![]));
    assert!(totals.lock().unwrap().is_empty());
}
