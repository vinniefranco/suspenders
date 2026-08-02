use super::*;

fn custom(id: &str, window: u64) -> Provider {
    Provider {
        id: id.into(),
        base_url: "http://localhost:1234/v1".into(),
        token: "".into(),
        api: Api::AnthropicMessages,
        context_window: Some(window),
        custom: true,
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
fn a_custom_provider_without_a_window_falls_back_to_the_global_figure() {
    // The window precedence's last step (ADR-0031 amendment): no catalog
    // entry, no per-provider window, so the fallback figure supplies it.
    let mut provider = custom("local", 0);
    provider.context_window = None;
    let model = resolve("local/some-model", &[provider], 48_000, 8_000).unwrap();
    assert_eq!(model.context_window, 48_000);
    assert_eq!(model.max_tokens, 8_000);
}

#[test]
fn a_catalog_model_takes_the_catalog_figures_pricing_and_reasoning() {
    let providers = catalog::builtin_providers();
    let model = resolve("anthropic/claude-fable-5", &providers, 64_000, 8_000).unwrap();
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.id, "claude-fable-5");
    assert_eq!(model.api, Api::AnthropicMessages);
    assert_eq!(model.context_window, 1_000_000);
    assert_eq!(model.max_tokens, 128_000);
    assert!(model.reasoning);
    let pricing = model.pricing.expect("catalog pricing rides the Model");
    assert!(pricing.input > 0.0);
    assert!(pricing.output > pricing.input);
}

#[test]
fn a_catalog_miss_on_a_builtin_falls_back_to_the_config_figures() {
    let providers = catalog::builtin_providers();
    let model = resolve("anthropic/claude-experimental", &providers, 48_000, 4_000).unwrap();
    assert_eq!(model.context_window, 48_000);
    assert_eq!(model.max_tokens, 4_000);
    assert_eq!(model.pricing, None);
    assert!(!model.reasoning);
}

#[test]
fn the_wire_output_cap_leaves_prompt_room_inside_the_window() {
    // Halved only when the reported ceiling would not leave prompt room.
    assert_eq!(wire_output_cap(131_072, 131_072), 65_536); // cap == window
    assert_eq!(wire_output_cap(100_000, 128_000), 64_000); // cap over half
    assert_eq!(wire_output_cap(8_000, 128_000), 8_000); // modest cap untouched
}

#[test]
fn a_catalog_model_whose_output_cap_equals_its_window_is_clamped() {
    // OpenRouter reports gpt-oss-120b with max_tokens == context_window; the
    // resolved wire cap leaves half the window for the prompt, so the
    // endpoint no longer 400s on a non-empty prompt.
    let providers = catalog::builtin_providers();
    let model = resolve("openrouter/openai/gpt-oss-120b", &providers, 64_000, 8_000).unwrap();
    assert_eq!(model.context_window, 131_072);
    assert_eq!(model.max_tokens, 65_536);
}

// ---- cost ----

#[test]
fn a_priced_model_prices_usage_an_unpriced_one_returns_none() {
    let mut model = Model::new("p", "m", Api::OpenaiCompletions, 64_000, 8_000);
    let usage = Usage {
        input_tokens: Some(2_000_000),
        output_tokens: Some(1_000_000),
        ..Usage::default()
    };
    assert_eq!(model.cost(&usage), None, "catalog-less Models go unpriced");

    model.pricing = Some(Pricing {
        input: 3.0,
        output: 15.0,
        cache_read: None,
        cache_write: None,
    });
    let cost = model.cost(&usage).unwrap();
    assert_eq!(cost.input, 6.0);
    assert_eq!(cost.output, 15.0);
    assert_eq!(cost.total, 21.0);
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

// ---- with_server_window ----

#[test]
fn with_server_window_overrides_the_window_and_rederives_the_cap() {
    // The host's live window replaces the resolved one; the dependent
    // output cap re-derives against it (still leaving prompt room).
    let base = Model::new("local", "m", Api::AnthropicMessages, 64_000, 8_000);

    // A wide window leaves a comfortably small reported cap untouched.
    let wide = with_server_window(&base, 145_664, 8_000);
    assert_eq!(wide.context_window, 145_664);
    assert_eq!(wide.max_tokens, 8_000); // 8000 <= 145664/2

    // A narrow window clamps the cap to half the window.
    let narrow = with_server_window(&base, 10_000, 8_000);
    assert_eq!(narrow.context_window, 10_000);
    assert_eq!(narrow.max_tokens, 5_000); // 10000/2
}
