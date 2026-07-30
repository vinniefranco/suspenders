//! The Catalog (ADR-0037, CONTEXT.md: Catalog): the generated registry of
//! known built-in Providers and their Models.
//!
//! The data lives as per-Provider JSON under `catalog/data/`, written by
//! `cargo run --bin generate-models` from models.dev and COMMITTED to git
//! (the deliberate divergence from pi, ADR-0037): builds stay offline and a
//! regeneration is a reviewable diff. The files embed via `include_str!` and
//! parse once on first use.
//!
//! The `local` default Provider is hand-written config
//! ([`crate::session::SessionConfig::base`]), never catalog data.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::content::Modalities;
use crate::llm::cost::Pricing;
use crate::llm::model::Api;
use crate::llm::provider::Provider;

/// One known model's catalog facts. The serialized form is one entry of a
/// committed data file - the generator writes this exact shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    /// The display name as models.dev records it.
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Whether the model can emit reasoning/thinking tokens.
    pub reasoning: bool,
    /// The input modalities the model accepts beyond text (ADR-0059). `default`
    /// (all-false) so committed data written before the field parses as
    /// text-only; the generator populates it from `modalities.input` on the next
    /// regeneration.
    #[serde(default)]
    pub input_modalities: Modalities,
    /// Flat rates in dollars per million tokens. `None` where models.dev
    /// carries no pricing (router pseudo-models) - such Models go unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Pricing>,
}

/// One built-in Provider: its endpoint, Api, credential environment keys,
/// and known Models. The serialized form is one committed data file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub id: String,
    /// The display name as models.dev records it.
    pub name: String,
    pub api: Api,
    pub base_url: String,
    /// The environment variable names the Provider's credential resolves
    /// from, in order - the first set one wins.
    pub env: Vec<String>,
    /// Sorted by model id (the generator's determinism contract).
    pub models: Vec<CatalogModel>,
}

/// The committed data files, embedded at compile time. The generator writes
/// the files; this list names them, alphabetically by provider id. Adding a
/// Provider to the generator's allowlist means adding its line here - both
/// halves of one reviewed change.
const DATA: &[&str] = &[
    include_str!("catalog/data/anthropic.json"),
    include_str!("catalog/data/cerebras.json"),
    include_str!("catalog/data/deepseek.json"),
    include_str!("catalog/data/fireworks-ai.json"),
    include_str!("catalog/data/groq.json"),
    include_str!("catalog/data/mistral.json"),
    include_str!("catalog/data/moonshotai.json"),
    include_str!("catalog/data/openrouter.json"),
    include_str!("catalog/data/togetherai.json"),
    include_str!("catalog/data/xai.json"),
    include_str!("catalog/data/zai.json"),
];

/// Every built-in Provider's catalog entry, parsed once from the embedded
/// data. Committed data that fails to parse is a build defect, so the parse
/// panics rather than degrading.
pub fn providers() -> &'static [CatalogProvider] {
    static PARSED: OnceLock<Vec<CatalogProvider>> = OnceLock::new();
    PARSED.get_or_init(|| {
        DATA.iter()
            .map(|raw| serde_json::from_str(raw).expect("committed catalog data parses"))
            .collect()
    })
}

/// Looks up one Provider's catalog entry.
pub fn provider(id: &str) -> Option<&'static CatalogProvider> {
    providers().iter().find(|p| p.id == id)
}

/// Looks up one model's catalog facts at one built-in Provider.
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static CatalogModel> {
    provider(provider_id)?
        .models
        .iter()
        .find(|m| m.id == model_id)
}

/// Resolves every built-in Provider: the catalog facts plus the credential
/// from the Provider's own environment keys (empty when none are set -
/// requests then fail at the host, not at launch). The one ambient read
/// outside the config seam, deliberately per-invocation like the
/// `SUSPENDERS_*` overlay.
pub fn builtin_providers() -> Vec<Provider> {
    providers()
        .iter()
        .map(|p| Provider {
            id: p.id.clone(),
            base_url: p.base_url.clone(),
            token: p
                .env
                .iter()
                .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
                .unwrap_or_default(),
            api: p.api,
            context_window: None,
            custom: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_committed_file_parses_with_sane_figures() {
        let all = providers();
        assert!(!all.is_empty());
        for p in all {
            assert!(!p.base_url.is_empty(), "{}: empty base_url", p.id);
            assert!(!p.env.is_empty(), "{}: no credential env keys", p.id);
            assert!(!p.models.is_empty(), "{}: no models", p.id);
            for m in &p.models {
                assert!(m.context_window > 0, "{}/{}: zero window", p.id, m.id);
                assert!(m.max_tokens > 0, "{}/{}: zero output cap", p.id, m.id);
            }
        }
    }

    #[test]
    fn provider_ids_are_unique_and_model_ids_unique_within_each() {
        let all = providers();
        let mut provider_ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        provider_ids.sort_unstable();
        provider_ids.dedup();
        assert_eq!(provider_ids.len(), all.len());

        for p in all {
            let mut ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), p.models.len(), "{}: duplicate model ids", p.id);
        }
    }

    #[test]
    fn models_are_sorted_by_id_the_generators_determinism_contract() {
        for p in providers() {
            let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(ids, sorted, "{}: models not sorted", p.id);
        }
    }

    #[test]
    fn the_anthropic_provider_carries_its_endpoint_key_and_priced_models() {
        let anthropic = provider("anthropic").expect("anthropic is built in");
        // The adapter appends only `/messages`, so the version prefix rides
        // in the base URL.
        assert_eq!(anthropic.base_url, "https://api.anthropic.com/v1");
        assert_eq!(anthropic.api, Api::AnthropicMessages);
        assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY"]);

        let fable = model("anthropic", "claude-fable-5").expect("fable is cataloged");
        assert!(fable.context_window > 0);
        assert!(fable.max_tokens > 0);
        let cost = fable.cost.expect("anthropic models carry pricing");
        assert!(cost.input > 0.0);
        assert!(cost.output > 0.0);
        assert!(cost.cache_read.is_some());
        assert!(cost.cache_write.is_some());
    }

    #[test]
    fn openai_compatible_providers_ride_the_openai_completions_api() {
        for id in ["deepseek", "groq", "openrouter"] {
            let p = provider(id).unwrap_or_else(|| panic!("{id} is built in"));
            assert_eq!(p.api, Api::OpenaiCompletions, "{id}");
            // The adapter appends `/chat/completions` and `/models`.
            assert!(!p.base_url.ends_with('/'), "{id}: trailing slash");
        }
    }

    #[test]
    fn unknown_ids_miss() {
        assert!(provider("nowhere").is_none());
        assert!(model("anthropic", "claude-imaginary").is_none());
        assert!(model("nowhere", "claude-fable-5").is_none());
    }

    #[test]
    fn builtin_providers_resolve_with_catalog_facts_and_no_config_window() {
        let providers = builtin_providers();
        let anthropic = providers.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!(anthropic.base_url, "https://api.anthropic.com/v1");
        assert_eq!(anthropic.api, Api::AnthropicMessages);
        assert_eq!(anthropic.context_window, None);
    }
}
