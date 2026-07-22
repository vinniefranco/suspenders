//! Model - the facts of one model at one Provider (ADR-0037, CONTEXT.md:
//! Model): scoped identifier, Api, context window, output cap. Pricing and
//! compat quirks join in a later stage.
//!
//! The scoped identifier is `provider/model-id`, split on the FIRST `/` only -
//! model ids themselves contain slashes (`local/qwen/Qwen3.6-27B-MTP-GGUF`
//! names the `qwen/Qwen3.6-27B-MTP-GGUF` model at the `local` Provider).

use serde::{Deserialize, Serialize};

use crate::llm::catalog;
use crate::llm::provider::Provider;

/// A wire protocol an adapter speaks (CONTEXT.md: Api). The seam of the LLM
/// boundary: one hand-written adapter per Api, and every Provider is data that
/// selects one. The serde forms are the config strings (`anthropic-messages`,
/// `openai-completions`).
///
/// `OpenaiCompletions` ships now but has no adapter until Stage B: the
/// dispatcher answers it with an error-algebra Response, never a panic
/// (ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenaiCompletions,
}

/// The facts of one model at one Provider (CONTEXT.md: Model). Read from the
/// Catalog for built-in Providers; synthesized from config for custom ones.
/// Each Turn captures a Model when it begins (ADR-0033 amendment); the budget
/// figures derive from that capture in Stage E - today the launch Model feeds
/// the once-at-launch validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The Provider's identifier (the scope of the scoped id).
    pub provider: String,
    /// The model's own identifier at that Provider - may contain slashes.
    pub id: String,
    /// The wire protocol the Provider serves this model over.
    pub api: Api,
    /// The model's context window in tokens.
    pub context_window: u64,
    /// The model's output cap in tokens - the wire `max_tokens` and the
    /// Eviction reserve.
    pub max_tokens: u64,
}

impl Model {
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
        }
    }

    /// The scoped identifier - `provider/model-id`.
    pub fn scoped_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
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
/// set. The Catalog supplies the figures for built-in Providers' known models;
/// a custom Provider's window comes from its config entry, and anything the
/// Catalog does not know falls back to `fallback_window` (the config
/// `context_budget`, reinterpreted per ADR-0037) and `fallback_max_tokens`
/// (the config `max_tokens` knob). An unknown Provider is an `Err` - failure
/// stays loud (ADR-0031).
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

    let (context_window, max_tokens) = match catalog::model(provider_id, model_id) {
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
        max_tokens,
    })
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
    fn a_catalog_model_takes_the_catalog_figures() {
        let providers = catalog::builtin_providers();
        let model = resolve("anthropic/claude-fable-5", &providers, 64_000, 8_000).unwrap();
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.id, "claude-fable-5");
        assert_eq!(model.api, Api::AnthropicMessages);
        assert_eq!(model.context_window, 1_000_000);
        assert_eq!(model.max_tokens, 128_000);
    }

    #[test]
    fn a_catalog_miss_on_a_builtin_falls_back_to_the_config_figures() {
        let providers = catalog::builtin_providers();
        let model = resolve("anthropic/claude-experimental", &providers, 48_000, 4_000).unwrap();
        assert_eq!(model.context_window, 48_000);
        assert_eq!(model.max_tokens, 4_000);
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
}
