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

/// Rebuilds `model` on a server-reported context window (ADR-0037: the server's
/// live `n_ctx` is authoritative for a custom Provider's Model, above config and
/// fallback). The `max_tokens` invariant is re-established against the new
/// window: it is derived from `reported_max_tokens` (the config `max_tokens`
/// knob, the same reported ceiling `resolve` fed [`wire_output_cap`]) so it still
/// leaves prompt room inside the window. Every other Model fact is preserved -
/// only the window and its dependent cap move.
pub(crate) fn with_server_window(model: &Model, window: u64, reported_max_tokens: u64) -> Model {
    Model {
        context_window: window,
        max_tokens: wire_output_cap(reported_max_tokens, window),
        ..model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(id: &str, window: u64) -> Provider {
        Provider {
            id: id.into(),
            base_url: "http://localhost:1234/v1".into(),
            token: "".into(),
            api: Api::AnthropicMessages,
            context_window: Some(window),
            custom: true,
        }
    }

    // ---- split_scoped ----

    #[test]
    fn splits_on_the_first_slash_only() {
        assert_eq!(
            split_scoped("local/qwen/Qwen3.6-27B-MTP-GGUF"),
            Ok(("local", "qwen/Qwen3.6-27B-MTP-GGUF"))
        );
        assert_eq!(
            split_scoped("anthropic/claude-fable-5"),
            Ok(("anthropic", "claude-fable-5"))
        );
    }

    #[test]
    fn rejects_unscoped_and_empty_sides() {
        assert!(split_scoped("bare-model").is_err());
        assert!(split_scoped("/model").is_err());
        assert!(split_scoped("provider/").is_err());
        assert!(split_scoped("").is_err());
    }

    #[test]
    fn scoped_id_round_trips_a_slashed_model_id() {
        let model = Model::new(
            "local",
            "qwen/Qwen3.6-27B-MTP-GGUF",
            Api::AnthropicMessages,
            64_000,
            8_000,
        );
        let scoped = model.scoped_id();
        assert_eq!(scoped, "local/qwen/Qwen3.6-27B-MTP-GGUF");
        let (provider, id) = split_scoped(&scoped).unwrap();
        assert_eq!((provider, id), ("local", "qwen/Qwen3.6-27B-MTP-GGUF"));
    }

    // ---- resolve ----

    #[test]
    fn a_custom_provider_synthesizes_from_its_config_window_and_the_fallback_cap() {
        let providers = vec![custom("local", 32_768)];
        let model = resolve("local/qwen/some-model", &providers, 64_000, 8_000).unwrap();
        assert_eq!(model.provider, "local");
        assert_eq!(model.id, "qwen/some-model");
        assert_eq!(model.api, Api::AnthropicMessages);
        assert_eq!(model.context_window, 32_768);
        assert_eq!(model.max_tokens, 8_000);
    }

    #[test]
    fn a_custom_provider_without_a_window_falls_back_to_the_global_figure() {
        // The window precedence's last step (ADR-0031 amendment): no catalog
        // entry, no per-provider window, so the fallback figure supplies it.
        let mut provider = custom("local", 0);
        provider.context_window = None;
        let model = resolve("local/some-model", &[provider], 48_000, 8_000).unwrap();
        assert_eq!(model.context_window, 48_000);
        assert_eq!(model.max_tokens, 8_000);
    }

    #[test]
    fn a_catalog_model_takes_the_catalog_figures_pricing_and_reasoning() {
        let providers = catalog::builtin_providers();
        let model = resolve("anthropic/claude-fable-5", &providers, 64_000, 8_000).unwrap();
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.id, "claude-fable-5");
        assert_eq!(model.api, Api::AnthropicMessages);
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 128_000);
        assert!(model.reasoning);
        let pricing = model.pricing.expect("catalog pricing rides the Model");
        assert!(pricing.input > 0.0);
        assert!(pricing.output > pricing.input);
    }

    #[test]
    fn a_catalog_miss_on_a_builtin_falls_back_to_the_config_figures() {
        let providers = catalog::builtin_providers();
        let model = resolve("anthropic/claude-experimental", &providers, 48_000, 4_000).unwrap();
        assert_eq!(model.context_window, 48_000);
        assert_eq!(model.max_tokens, 4_000);
        assert_eq!(model.pricing, None);
        assert!(!model.reasoning);
    }

    #[test]
    fn the_wire_output_cap_leaves_prompt_room_inside_the_window() {
        // Halved only when the reported ceiling would not leave prompt room.
        assert_eq!(wire_output_cap(131_072, 131_072), 65_536); // cap == window
        assert_eq!(wire_output_cap(100_000, 128_000), 64_000); // cap over half
        assert_eq!(wire_output_cap(8_000, 128_000), 8_000); // modest cap untouched
    }

    #[test]
    fn a_catalog_model_whose_output_cap_equals_its_window_is_clamped() {
        // OpenRouter reports gpt-oss-120b with max_tokens == context_window; the
        // resolved wire cap leaves half the window for the prompt, so the
        // endpoint no longer 400s on a non-empty prompt.
        let providers = catalog::builtin_providers();
        let model = resolve("openrouter/openai/gpt-oss-120b", &providers, 64_000, 8_000).unwrap();
        assert_eq!(model.context_window, 131_072);
        assert_eq!(model.max_tokens, 65_536);
    }

    // ---- cost ----

    #[test]
    fn a_priced_model_prices_usage_an_unpriced_one_returns_none() {
        let mut model = Model::new("p", "m", Api::OpenaiCompletions, 64_000, 8_000);
        let usage = Usage {
            input_tokens: Some(2_000_000),
            output_tokens: Some(1_000_000),
            ..Usage::default()
        };
        assert_eq!(model.cost(&usage), None, "catalog-less Models go unpriced");

        model.pricing = Some(Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: None,
            cache_write: None,
        });
        let cost = model.cost(&usage).unwrap();
        assert_eq!(cost.input, 6.0);
        assert_eq!(cost.output, 15.0);
        assert_eq!(cost.total, 21.0);
    }

    #[test]
    fn an_unknown_provider_is_a_loud_error() {
        let err = resolve("nowhere/model", &[], 64_000, 8_000).unwrap_err();
        assert!(err.contains("nowhere"), "error was: {err}");
    }

    // ---- Api serde ----

    #[test]
    fn api_serializes_to_the_config_strings() {
        assert_eq!(
            serde_json::to_string(&Api::AnthropicMessages).unwrap(),
            "\"anthropic-messages\""
        );
        assert_eq!(
            serde_json::to_string(&Api::OpenaiCompletions).unwrap(),
            "\"openai-completions\""
        );
        let api: Api = serde_json::from_str("\"anthropic-messages\"").unwrap();
        assert_eq!(api, Api::AnthropicMessages);
    }

    // ---- with_server_window ----

    #[test]
    fn with_server_window_overrides_the_window_and_rederives_the_cap() {
        // The host's live window replaces the resolved one; the dependent
        // output cap re-derives against it (still leaving prompt room).
        let base = Model::new("local", "m", Api::AnthropicMessages, 64_000, 8_000);

        // A wide window leaves a comfortably small reported cap untouched.
        let wide = with_server_window(&base, 145_664, 8_000);
        assert_eq!(wide.context_window, 145_664);
        assert_eq!(wide.max_tokens, 8_000); // 8000 <= 145664/2

        // A narrow window clamps the cap to half the window.
        let narrow = with_server_window(&base, 10_000, 8_000);
        assert_eq!(narrow.context_window, 10_000);
        assert_eq!(narrow.max_tokens, 5_000); // 10000/2
    }
}
