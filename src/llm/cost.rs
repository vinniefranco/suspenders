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

/// Tokens per rate unit: Catalog rates are dollars per MILLION tokens (the
/// models.dev convention), so a token count is priced by scaling against this.
const PER_MILLION: f64 = 1_000_000.0;

/// The billed rate for a meter the host does not price: an absent cache rate
/// bills as zero, which is a data fact (the host does not meter that figure),
/// not a missing value.
const UNPRICED_RATE: f64 = 0.0;

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

/// Prices a [`Usage`] against a [`Pricing`]. Pure: absent usage figures count
/// zero tokens (the four meters are disjoint on the wire - `input_tokens`
/// never includes the cache figures), and absent cache rates bill zero.
pub fn cost(pricing: &Pricing, usage: &Usage) -> Cost {
    let dollars = |tokens: Option<u64>, rate: f64| tokens.unwrap_or(0) as f64 * rate / PER_MILLION;

    let input = dollars(usage.input_tokens, pricing.input);
    let output = dollars(usage.output_tokens, pricing.output);
    let cache_read = dollars(
        usage.cache_read_input_tokens,
        pricing.cache_read.unwrap_or(UNPRICED_RATE),
    );
    let cache_write = dollars(
        usage.cache_creation_input_tokens,
        pricing.cache_write.unwrap_or(UNPRICED_RATE),
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
#[path = "../../tests/llm/cost.rs"]
mod tests;
