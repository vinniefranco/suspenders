//! Cost math (ADR-0037): pricing one Response's [`Usage`] against a Model's
//! Catalog rates.
//!
//! Rates are flat dollars per million tokens, exactly as models.dev records
//! them - the included Providers carry no pricing tiers, so the Catalog
//! carries none (the generator documents the few tiered outliers it drops).
//! The [`cost`] fold is pure; surfacing the figures is deferred - the status
//! bar consumes [`crate::llm::model::Model::cost`] in a later stage.

use serde::{Deserialize, Serialize};

use crate::content::Usage;

/// One model's flat rates in dollars per million tokens. The serialized form
/// is the `cost` object of the committed Catalog data (models.dev key names).
/// An absent cache rate means the host does not meter that figure - it bills
/// as zero, it is not a data error.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// One Response's price in dollars, broken down by meter. `total` is always
/// the sum of the four parts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// Tokens per million: rates in the Catalog are dollars per million tokens.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// The fallback rate for an absent cache field: the host does not meter that
/// figure, so it bills as zero (not a data error).
const NO_CACHE_RATE: f64 = 0.0;

/// Prices a [`Usage`] against a [`Pricing`]. Pure: absent usage figures count
/// zero tokens (the four meters are disjoint on the wire - `input_tokens`
/// never includes the cache figures), and absent cache rates bill zero.
pub fn cost(pricing: &Pricing, usage: &Usage) -> Cost {
    let dollars =
        |tokens: Option<u64>, rate: f64| tokens.unwrap_or(0) as f64 * rate / TOKENS_PER_MILLION;

    let input = dollars(usage.input_tokens, pricing.input);
    let output = dollars(usage.output_tokens, pricing.output);
    let cache_read = dollars(
        usage.cache_read_input_tokens,
        pricing.cache_read.unwrap_or(NO_CACHE_RATE),
    );
    let cache_write = dollars(
        usage.cache_creation_input_tokens,
        pricing.cache_write.unwrap_or(NO_CACHE_RATE),
    );

    Cost {
        input,
        output,
        cache_read,
        cache_write,
        total: input + output + cache_read + cache_write,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/llm/cost.rs"]
mod tests;
