use super::*;

// ---- context_floor/1 ----

#[test]
fn context_floor_sums_all_four_figures() {
    let usage = Usage {
        input_tokens: Some(200),
        output_tokens: Some(300),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: Some(1_500),
    };
    assert_eq!(usage.context_floor(), Some(92_000));
}

#[test]
fn context_floor_counts_absent_figures_as_zero() {
    assert_eq!(Usage::with_input_tokens(200).context_floor(), Some(200));
}

#[test]
fn context_floor_is_none_without_input_tokens() {
    // A usage map without input_tokens is no signal, not a zero floor -
    // even when the cache figures are present.
    assert_eq!(Usage::default().context_floor(), None);
    let cache_only = Usage {
        input_tokens: None,
        output_tokens: Some(300),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: Some(1_500),
    };
    assert_eq!(cache_only.context_floor(), None);
}
