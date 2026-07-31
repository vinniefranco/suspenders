//! Model - the facts of one model at one Provider (ADR-0037, CONTEXT.md:
//! Model): scoped identifier, Api, context window, output cap, pricing, and
//! the reasoning capability flag.
//!
//! The scoped identifier is `provider/model-id`, split on the FIRST `/` only -
//! model ids themselves contain slashes (`local/qwen/Qwen3.6-27B-MTP-GGUF`
//! names the `qwen/Qwen3.6-27B-MTP-GGUF` model at the `local` Provider).

use serde::{Deserialize, Serialize};

use crate::content::{Modalities, Provenance, Usage};
use crate::llm::catalog;
use crate::llm::cost::{self, Cost, Pricing};
use crate::llm::provider::Provider;

/// A wire protocol an adapter speaks (CONTEXT.md: Api). The seam of the LLM
/// boundary: one hand-written adapter per Api, and every Provider is data that
/// selects one. The serde forms are the config strings (`anthropic-messages`,
/// `openai-completions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenaiCompletions,
}

/// The facts of one model at one Provider (CONTEXT.md: Model). Read from the
/// Catalog for built-in Providers; synthesized from config for custom ones.
/// Each Run captures a Model when it begins (ADR-0033 amendment), and the
/// Context Budget, the Eviction reserve, and the Result Cap derive from that
/// capture at Run start (ADR-0037).
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The Provider's identifier (the scope of the scoped id).
    pub provider: String,
    /// The model's own identifier at that Provider - may contain slashes.
    pub id: String,
    /// The wire protocol the Provider serves this model over.
    pub api: Api,
    /// The model's context window in tokens.
    pub context_window: u64,
    /// The wire `max_tokens` (the model's output cap): the reported ceiling
    /// clamped to leave prompt room inside the window ([`wire_output_cap`]), so
    /// `input + max_tokens` always fits. The per-Run reply reserve for the
    /// Compaction math derives from this in turn.
    pub max_tokens: u64,
    /// The Catalog's flat rates in dollars per million tokens. `None` for
    /// Models the Catalog cannot price: custom Providers and catalog misses.
    pub pricing: Option<Pricing>,
    /// Whether the model can emit reasoning/thinking tokens (a Catalog fact;
    /// `false` for synthesized Models).
    pub reasoning: bool,
    /// The input modalities the model accepts beyond text (ADR-0059): a Catalog
    /// fact, all-false for synthesized Models and catalog misses. Stamped onto
    /// the [`crate::tool::ToolCtx`] so read_file (P3 3b) gates media on it, and
    /// read by the wire-build-time degrade pass ([`crate::llm::transform`]).
    pub input_modalities: Modalities,
}

impl Model {
    /// The catalog-less constructor: pricing and the reasoning flag stay at
    /// their unpriced defaults. [`resolve`] fills both from the Catalog.
    pub fn new(
        provider: impl Into<String>,
        id: impl Into<String>,
        api: Api,
        context_window: u64,
        max_tokens: u64,
    ) -> Self {
        Model {
            provider: provider.into(),
            id: id.into(),
            api,
            context_window,
            max_tokens,
            pricing: None,
            reasoning: false,
            input_modalities: Modalities::default(),
        }
    }

    /// The scoped identifier - `provider/model-id`.
    pub fn scoped_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// The Provenance this Model stamps on assistant messages it produces
    /// (CONTEXT.md: Provenance): the two ids, never the Api.
    pub fn provenance(&self) -> Provenance {
        Provenance::new(self.provider.clone(), self.id.clone())
    }

    /// Prices one Response's [`Usage`] against this Model's Catalog rates:
    /// the pure [`cost::cost`] fold, `None` when the Model carries no
    /// pricing. Runs wherever usage is folded; the status bar surfaces the
    /// figures in a later stage.
    pub fn cost(&self, usage: &Usage) -> Option<Cost> {
        self.pricing.as_ref().map(|p| cost::cost(p, usage))
    }
}

/// Splits a scoped identifier into `(provider, model-id)` on the FIRST `/`
/// only. Either side empty (or no `/` at all) is an error naming the input.
pub fn split_scoped(scoped: &str) -> Result<(&str, &str), String> {
    match scoped.split_once('/') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() => Ok((provider, id)),
        _ => Err(format!(
            "model must be a scoped provider/model-id, got: {scoped:?}"
        )),
    }
}

/// Resolves a scoped identifier to a [`Model`] against the resolved Provider
/// set. The window precedence (ADR-0037, ADR-0031 amendment): the Catalog's
/// figure for a built-in Provider's known model, else the Provider's own
/// config `context_window`, else `fallback_window` (the config
/// `context_budget` figure, or its default when unset). `fallback_max_tokens`
/// (the config `max_tokens` knob) caps every Model the Catalog does not know.
/// An unknown Provider is an `Err` - failure stays loud (ADR-0031).
pub fn resolve(
    scoped: &str,
    providers: &[Provider],
    fallback_window: u64,
    fallback_max_tokens: u64,
) -> Result<Model, String> {
    let (provider_id, model_id) = split_scoped(scoped)?;
    let provider = providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| format!("unknown provider {provider_id:?} in model {scoped:?}"))?;

    let known = catalog::model(provider_id, model_id);
    let (context_window, catalog_max_tokens) = match known {
        Some(known) => (known.context_window, known.max_tokens),
        None => (
            provider.context_window.unwrap_or(fallback_window),
            fallback_max_tokens,
        ),
    };

    Ok(Model {
        provider: provider.id.clone(),
        id: model_id.to_string(),
        api: provider.api,
        context_window,
        max_tokens: wire_output_cap(catalog_max_tokens, context_window),
        pricing: known.and_then(|k| k.cost),
        reasoning: known.is_some_and(|k| k.reasoning),
        input_modalities: known.map(|k| k.input_modalities).unwrap_or_default(),
    })
}

/// The wire `max_tokens` (the Model's output cap): the reported ceiling, clamped
/// to leave prompt room inside the context window. A request spends
/// `input + max_tokens` tokens against the window, so a Provider that reports an
/// output cap equal to (or near) its window - some do, e.g. OpenRouter's
/// `gpt-oss-120b` lists `max_tokens == context_window` - would make the endpoint
/// 400 the moment the prompt is non-empty. Half the window is reserved for the
/// output, leaving the other half for the prompt; a cap already below that (the
/// common case, e.g. 8K out of 128K) is untouched. The tighter per-Run reply
/// reserve for the Compaction math still derives from this via
/// [`crate::session::Session::reply_reserve_for`].
fn wire_output_cap(reported: u64, context_window: u64) -> u64 {
    reported.min(context_window / 2)
}

#[cfg(test)]
#[path = "../../tests/unit/llm/model.rs"]
mod tests;
