use super::*;
use serde_json::json;

/// A models.dev-shaped fixture: one usable model, one tool-less model,
/// one audio model, one without limits.
fn fixture() -> Value {
    json!({
        "id": "acme",
        "name": "Acme AI",
        "env": ["ACME_API_KEY", "ACME_KEY"],
        "api": "https://api.acme.dev/v1/",
        "models": {
            "acme-large": {
                "id": "acme-large",
                "name": "Acme Large",
                "tool_call": true,
                "reasoning": true,
                "modalities": { "input": ["text"], "output": ["text"] },
                "limit": { "context": 200_000, "output": 32_000 },
                "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 }
            },
            "acme-chat": {
                "id": "acme-chat",
                "tool_call": false,
                "modalities": { "input": ["text"], "output": ["text"] },
                "limit": { "context": 8_000, "output": 4_000 },
                "cost": { "input": 1.0, "output": 2.0 }
            },
            "acme-whisper": {
                "id": "acme-whisper",
                "tool_call": true,
                "modalities": { "input": ["audio"], "output": ["audio"] },
                "limit": { "context": 8_000, "output": 4_000 }
            },
            "acme-preview": {
                "id": "acme-preview",
                "tool_call": true,
                "modalities": { "input": ["text"], "output": ["text"] },
                "limit": { "context": 128_000 }
            }
        }
    })
}

#[test]
fn maps_a_provider_from_its_entry_taking_the_api_field_base_url() {
    let p = map_provider(Api::OpenaiCompletions, None, &fixture()).unwrap();
    assert_eq!(p.id, "acme");
    assert_eq!(p.name, "Acme AI");
    assert_eq!(p.api, Api::OpenaiCompletions);
    // Trailing slash trimmed so the adapter's `{base_url}/...` joins cleanly.
    assert_eq!(p.base_url, "https://api.acme.dev/v1");
    assert_eq!(p.env, vec!["ACME_API_KEY", "ACME_KEY"]);
}

#[test]
fn a_base_url_override_beats_the_api_field() {
    let p = map_provider(
        Api::AnthropicMessages,
        Some("https://api.acme.dev/anthropic/v1"),
        &fixture(),
    )
    .unwrap();
    assert_eq!(p.base_url, "https://api.acme.dev/anthropic/v1");
}

#[test]
fn only_tool_calling_text_models_with_both_limits_survive() {
    let p = map_provider(Api::OpenaiCompletions, None, &fixture()).unwrap();
    let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
    // acme-chat: no tool_call; acme-whisper: no text output;
    // acme-preview: no output limit.
    assert_eq!(ids, vec!["acme-large"]);
}

#[test]
fn a_model_carries_its_figures_flags_and_flat_rates() {
    let p = map_provider(Api::OpenaiCompletions, None, &fixture()).unwrap();
    let m = &p.models[0];
    assert_eq!(m.name, "Acme Large");
    assert_eq!(m.context_window, 200_000);
    assert_eq!(m.max_tokens, 32_000);
    assert!(m.reasoning);
    let cost = m.cost.unwrap();
    assert_eq!(cost.input, 3.0);
    assert_eq!(cost.output, 15.0);
    assert_eq!(cost.cache_read, Some(0.3));
    assert_eq!(cost.cache_write, None);
}

#[test]
fn models_sort_by_id_for_deterministic_diffs() {
    let mut entry = fixture();
    entry["models"]["acme-chat"]["tool_call"] = json!(true);
    let p = map_provider(Api::OpenaiCompletions, None, &entry).unwrap();
    let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["acme-chat", "acme-large"]);
}

#[test]
fn an_unpriced_model_rides_with_no_cost() {
    let mut entry = fixture();
    entry["models"]["acme-large"]
        .as_object_mut()
        .unwrap()
        .remove("cost");
    let p = map_provider(Api::OpenaiCompletions, None, &entry).unwrap();
    assert_eq!(p.models[0].cost, None);
}

#[test]
fn a_missing_name_falls_back_to_the_model_id() {
    let mut entry = fixture();
    entry["models"]["acme-large"]
        .as_object_mut()
        .unwrap()
        .remove("name");
    let p = map_provider(Api::OpenaiCompletions, None, &entry).unwrap();
    assert_eq!(p.models[0].name, "acme-large");
}

#[test]
fn generate_emits_one_deterministic_file_per_included_provider() {
    // A models.dev table carrying every included provider id.
    let mut raw = serde_json::Map::new();
    for (id, _, _) in INCLUDED {
        let mut entry = fixture();
        entry["id"] = json!(id);
        raw.insert(id.to_string(), entry);
    }
    let files = generate(&Value::Object(raw)).unwrap();

    assert_eq!(files.len(), INCLUDED.len());
    for (file, (id, api, _)) in files.iter().zip(INCLUDED) {
        assert_eq!(file.name, format!("{id}.json"));
        assert_eq!(file.models, 1, "{id}: the fixture's one usable model");
        assert!(file.contents.ends_with("}\n"), "{id}: trailing newline");
        // The bytes are exactly the parseable catalog shape.
        let parsed: CatalogProvider = serde_json::from_str(&file.contents).unwrap();
        assert_eq!(parsed.id, *id);
        assert_eq!(parsed.api, *api);
    }
}

#[test]
fn generate_fails_loudly_when_models_dev_drops_an_included_provider() {
    let err = generate(&json!({})).unwrap_err();
    assert!(err.contains("no longer lists anthropic"), "err: {err}");
}

#[test]
fn provider_level_gaps_fail_loudly() {
    // No env keys: the credential could never resolve.
    let mut entry = fixture();
    entry["env"] = json!([]);
    assert!(
        map_provider(Api::OpenaiCompletions, None, &entry)
            .unwrap_err()
            .contains("env")
    );

    // No base URL from either source.
    let mut entry = fixture();
    entry.as_object_mut().unwrap().remove("api");
    assert!(
        map_provider(Api::OpenaiCompletions, None, &entry)
            .unwrap_err()
            .contains("api")
    );

    // Every model filtered out: an empty file would be a silent lie.
    let mut entry = fixture();
    entry["models"] = json!({});
    assert!(
        map_provider(Api::OpenaiCompletions, None, &entry)
            .unwrap_err()
            .contains("no usable models")
    );
}
