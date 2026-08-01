//! Session cost metering (ADR-0037): a decorator over the [`Llm`] boundary
//! that prices every Response against the captured Model it answered for.
//!
//! Every model call a Session makes - main Run Passes and Compaction
//! summaries - flows through the one injected `Arc<dyn Llm>`,
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
use crate::llm::{DiscoveredModel, Llm, LlmRequest, OnEvent};

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

    async fn list_models(&self, provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
        self.inner.list_models(provider).await
    }
}

#[cfg(test)]
#[path = "../../tests/llm/metered.rs"]
mod tests;
