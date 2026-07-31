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
