
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
