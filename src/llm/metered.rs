//! Session cost metering (ADR-0037): a decorator over the [`Llm`] boundary
//! that prices every Response against the captured Model it answered for.
//!
//! Every model call a Session makes - main Turn Passes, Scouts, Compaction
//! summaries, Handoff seeds - flows through the one injected `Arc<dyn Llm>`,
//! so wrapping that value at Agent start meters them all in one place. The
//! math is the existing [`Model::cost`] fold; this module only accumulates.
//!
//! Display-side only: the running total goes out through the injected sink
//! (the Agent wires it to a Session-cost event), never into the Conversation
//! or the Session Log. An unpriced Model (every local/custom model) prices to
//! `None` and a priced call with zero usage to zero - neither moves the total
//! nor fires the sink, so a local-only Session is metered silence.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::llm::model::Model;
use crate::llm::provider::Provider;
use crate::llm::response::Response;
use crate::llm::{Llm, LlmRequest, OnEvent};

/// The sink the new running total (in dollars) is pushed through after every
/// priced call.
pub type OnTotal = Box<dyn Fn(f64) + Send + Sync>;

/// The metering decorator: delegates both trait methods to `inner`, pricing
/// each `complete`'s usage against the call's own captured Model.
pub struct Metered {
    inner: Arc<dyn Llm>,
    total: Mutex<f64>,
    on_total: OnTotal,
}

impl Metered {
    pub fn new(inner: Arc<dyn Llm>, on_total: impl Fn(f64) + Send + Sync + 'static) -> Self {
        Metered {
            inner,
            total: Mutex::new(0.0),
            on_total: Box::new(on_total),
        }
    }
}

#[async_trait]
impl Llm for Metered {
    async fn complete(
        &self,
        request: &LlmRequest,
        model: &Model,
        on_event: &mut OnEvent<'_>,
    ) -> Response {
        let response = self.inner.complete(request, model, on_event).await;
        if let Some(cost) = model.cost(&response.usage)
            && cost.total > 0.0
        {
            let total = {
                let mut total = self.total.lock().unwrap();
                *total += cost.total;
                *total
            };
            (self.on_total)(total);
        }
        response
    }

    async fn list_models(&self, provider: &Provider) -> Result<Vec<String>, String> {
        self.inner.list_models(provider).await
    }
}

#[cfg(test)]
mod tests {
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
}
