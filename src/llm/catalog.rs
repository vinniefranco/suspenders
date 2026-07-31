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
#[path = "../../tests/unit/llm/catalog.rs"]
mod tests;
