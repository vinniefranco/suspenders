//! The seed Catalog (ADR-0037, CONTEXT.md: Catalog): the registry of known
//! built-in Providers and their Models.
//!
//! HAND-WRITTEN SEED, Stage A only: Stage C replaces this with per-Provider
//! JSON generated from models.dev by a committed generator binary and embedded
//! with `include_str!`. The figures below are current as of 2026-07 and are
//! not tuned - the generator owns exactness.

use crate::llm::model::Api;
use crate::llm::provider::Provider;

/// One known model's catalog facts.
pub struct CatalogModel {
    pub id: &'static str,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// One built-in Provider: its endpoint, Api, credential environment key, and
/// known Models.
pub struct CatalogProvider {
    pub id: &'static str,
    pub base_url: &'static str,
    pub api: Api,
    /// The environment variable the Provider's credential resolves from.
    pub env_key: &'static str,
    pub models: &'static [CatalogModel],
}

/// The built-in Providers. One entry today; the Stage C generator grows it.
pub const PROVIDERS: &[CatalogProvider] = &[CatalogProvider {
    id: "anthropic",
    base_url: "https://api.anthropic.com",
    api: Api::AnthropicMessages,
    env_key: "ANTHROPIC_API_KEY",
    models: &[
        CatalogModel {
            id: "claude-fable-5",
            context_window: 1_000_000,
            max_tokens: 128_000,
        },
        CatalogModel {
            id: "claude-opus-4-8",
            context_window: 1_000_000,
            max_tokens: 128_000,
        },
        CatalogModel {
            id: "claude-sonnet-4-6",
            context_window: 1_000_000,
            max_tokens: 64_000,
        },
        CatalogModel {
            id: "claude-haiku-4-5-20251001",
            context_window: 200_000,
            max_tokens: 64_000,
        },
    ],
}];

/// Looks up one Provider's catalog entry.
pub fn provider(id: &str) -> Option<&'static CatalogProvider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Looks up one model's catalog facts at one built-in Provider.
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static CatalogModel> {
    provider(provider_id)?
        .models
        .iter()
        .find(|m| m.id == model_id)
}

/// Resolves every built-in Provider: the catalog facts plus the credential
/// from the Provider's own environment key (empty when unset - requests then
/// fail at the host, not at launch). The one ambient read outside the config
/// seam, deliberately per-invocation like the `SUSPENDERS_*` overlay.
pub fn builtin_providers() -> Vec<Provider> {
    PROVIDERS
        .iter()
        .map(|p| Provider {
            id: p.id.to_string(),
            base_url: p.base_url.to_string(),
            token: std::env::var(p.env_key).unwrap_or_default(),
            api: p.api,
            context_window: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_anthropic_provider_and_its_models_are_seeded() {
        let anthropic = provider("anthropic").expect("anthropic is built in");
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(anthropic.api, Api::AnthropicMessages);
        assert_eq!(anthropic.env_key, "ANTHROPIC_API_KEY");

        for id in [
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ] {
            let m = model("anthropic", id).unwrap_or_else(|| panic!("{id} is seeded"));
            assert!(m.context_window > 0);
            assert!(m.max_tokens > 0);
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
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(anthropic.api, Api::AnthropicMessages);
        assert_eq!(anthropic.context_window, None);
    }
}
