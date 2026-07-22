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

/// Prices a [`Usage`] against a [`Pricing`]. Pure: absent usage figures count
/// zero tokens (the four meters are disjoint on the wire - `input_tokens`
/// never includes the cache figures), and absent cache rates bill zero.
pub fn cost(pricing: &Pricing, usage: &Usage) -> Cost {
    let dollars = |tokens: Option<u64>, rate: f64| tokens.unwrap_or(0) as f64 * rate / 1_000_000.0;

    let input = dollars(usage.input_tokens, pricing.input);
    let output = dollars(usage.output_tokens, pricing.output);
    let cache_read = dollars(
        usage.cache_read_input_tokens,
        pricing.cache_read.unwrap_or(0.0),
    );
    let cache_write = dollars(
        usage.cache_creation_input_tokens,
        pricing.cache_write.unwrap_or(0.0),
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
mod tests {
    use super::*;

    fn full_rates() -> Pricing {
        // Claude Fable 5's published rates.
        Pricing {
            input: 10.0,
            output: 50.0,
            cache_read: Some(1.0),
            cache_write: Some(12.5),
        }
    }

    #[test]
    fn every_meter_prices_at_its_own_rate_and_total_sums() {
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(200_000),
            cache_read_input_tokens: Some(3_000_000),
            cache_creation_input_tokens: Some(400_000),
        };
        let c = cost(&full_rates(), &usage);
        assert_eq!(c.input, 10.0);
        assert_eq!(c.output, 10.0);
        assert_eq!(c.cache_read, 3.0);
        assert_eq!(c.cache_write, 5.0);
        assert_eq!(c.total, 28.0);
    }

    #[test]
    fn absent_usage_figures_count_zero_tokens() {
        let c = cost(&full_rates(), &Usage::default());
        assert_eq!(
            c,
            Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.0
            }
        );
    }

    #[test]
    fn absent_cache_rates_bill_zero_even_with_cache_tokens() {
        let uncached = Pricing {
            input: 0.14,
            output: 0.28,
            cache_read: None,
            cache_write: None,
        };
        let usage = Usage {
            input_tokens: Some(500_000),
            output_tokens: None,
            cache_read_input_tokens: Some(1_000_000),
            cache_creation_input_tokens: Some(1_000_000),
        };
        let c = cost(&uncached, &usage);
        assert_eq!(c.input, 0.07);
        assert_eq!(c.cache_read, 0.0);
        assert_eq!(c.cache_write, 0.0);
        assert_eq!(c.total, 0.07);
    }

    #[test]
    fn pricing_serde_round_trips_and_omits_absent_cache_rates() {
        let p = Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"input":3.0,"output":15.0,"cache_read":0.3}"#);
        let back: Pricing = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
