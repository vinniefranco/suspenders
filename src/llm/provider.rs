//! Provider - a host that serves models (ADR-0037, CONTEXT.md: Provider):
//! an identifier, a base URL, a resolved credential, and the Api its Models
//! speak. Pure data that selects an adapter - host quirks within an Api are
//! per-Model compat facts, never subclasses.
//!
//! Custom Providers come from the config `providers` table (ADR-0031
//! amendment); built-in Providers come from the seed [`crate::llm::catalog`]
//! with the token resolved from their own environment key (`ANTHROPIC_API_KEY`
//! for anthropic). The Session resolves the set once at launch; every request
//! travels over the Active Model's Provider.

use crate::llm::model::Api;

/// One host, resolved: part of the Session's fixed facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub base_url: String,
    /// The resolved credential: the config `token` for a custom Provider, the
    /// environment key's value for a built-in. Empty when neither supplied -
    /// local servers ignore it, remote ones answer 401.
    pub token: String,
    pub api: Api,
    /// The window a custom Provider's Models synthesize from (its config
    /// `context_window`). `None` for built-ins, whose windows the Catalog
    /// carries per Model.
    pub context_window: Option<u64>,
}

/// Finds a Provider by id in a resolved set.
pub fn find<'a>(providers: &'a [Provider], id: &str) -> Option<&'a Provider> {
    providers.iter().find(|p| p.id == id)
}
