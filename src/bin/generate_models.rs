//! Regenerates the committed Catalog data from models.dev (ADR-0037).
//!
//! `cargo run --bin generate-models` fetches <https://models.dev/api.json>,
//! filters to the Providers in [`INCLUDED`], maps each entry to the
//! [`CatalogProvider`] shape, and rewrites `src/llm/catalog/data/<id>.json`.
//! The output is deterministic - models sorted by id, stable field order -
//! so a regeneration is a minimal reviewable diff (the data is committed,
//! the deliberate divergence from pi).
//!
//! The allowlist keeps the committed data reviewable: only well-known hosts
//! whose models speak an implemented Api ride along, not everything
//! models.dev tracks (169 providers). Skipped by policy: hosts needing
//! unimplemented protocols (openai and its Responses-only models, google,
//! bedrock) and the long tail of resellers.
//!
//! models.dev records no `max_tokens` vs `max_completion_tokens` compat
//! fact, so the MAX_TOKENS_FIELD seam in `openai_completions/request.rs`
//! stays untouched by generated data.

use serde_json::Value;
use suspenders::llm::catalog::{CatalogModel, CatalogProvider};
use suspenders::llm::cost::Pricing;
use suspenders::llm::model::Api;

const SOURCE: &str = "https://models.dev/api.json";

/// The allowlist: (models.dev id, Api, base URL override). `None` takes the
/// entry's `api` field (models.dev's name for the base URL), trimmed of any
/// trailing slash. Overrides carry the well-known endpoints models.dev does
/// not record for hosts with dedicated vendor SDKs. Anthropic's includes the
/// `/v1` prefix because the adapter appends only `/messages`; the
/// openai-completions endpoints likewise end where `/chat/completions`
/// begins.
const INCLUDED: &[(&str, Api, Option<&str>)] = &[
    (
        "anthropic",
        Api::AnthropicMessages,
        Some("https://api.anthropic.com/v1"),
    ),
    (
        "cerebras",
        Api::OpenaiCompletions,
        Some("https://api.cerebras.ai/v1"),
    ),
    ("deepseek", Api::OpenaiCompletions, None),
    ("fireworks-ai", Api::OpenaiCompletions, None),
    (
        "groq",
        Api::OpenaiCompletions,
        Some("https://api.groq.com/openai/v1"),
    ),
    (
        "mistral",
        Api::OpenaiCompletions,
        Some("https://api.mistral.ai/v1"),
    ),
    ("moonshotai", Api::OpenaiCompletions, None),
    ("openrouter", Api::OpenaiCompletions, None),
    (
        "togetherai",
        Api::OpenaiCompletions,
        Some("https://api.together.xyz/v1"),
    ),
    ("xai", Api::OpenaiCompletions, Some("https://api.x.ai/v1")),
    ("zai", Api::OpenaiCompletions, None),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let files = generate(&fetch().await?).map_err(anyhow::Error::msg)?;
    write(&files)
}

async fn fetch() -> anyhow::Result<Value> {
    Ok(reqwest::get(SOURCE)
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// One data file, ready to land under `src/llm/catalog/data/`.
#[derive(Debug)]
struct GeneratedFile {
    name: String,
    contents: String,
    models: usize,
}

/// The pure step: maps the fetched models.dev table through [`INCLUDED`] to
/// the exact bytes each committed data file receives. A provider models.dev
/// dropped, or one that maps to nothing usable, fails the whole run - the
/// Catalog never silently shrinks.
fn generate(raw: &Value) -> Result<Vec<GeneratedFile>, String> {
    INCLUDED
        .iter()
        .map(|(id, api, base_url)| {
            let entry = raw
                .get(id)
                .ok_or_else(|| format!("models.dev no longer lists {id}"))?;
            let provider =
                map_provider(*api, *base_url, entry).map_err(|e| format!("{id}: {e}"))?;
            let json =
                serde_json::to_string_pretty(&provider).expect("catalog types serialize to JSON");
            Ok(GeneratedFile {
                name: format!("{id}.json"),
                contents: json + "\n",
                models: provider.models.len(),
            })
        })
        .collect()
}

fn write(files: &[GeneratedFile]) -> anyhow::Result<()> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/llm/catalog/data");
    std::fs::create_dir_all(&dir)?;
    for file in files {
        std::fs::write(dir.join(&file.name), &file.contents)?;
        println!("{}: {} models", file.name, file.models);
    }
    Ok(())
}

/// Maps one models.dev provider entry to the Catalog shape. Pure: the id,
/// name, and credential env keys come from the entry; the base URL from the
/// override or the entry's `api` field; the models from [`map_model`],
/// sorted by id (the determinism contract catalog.rs tests enforce).
fn map_provider(
    api: Api,
    base_url: Option<&str>,
    entry: &Value,
) -> Result<CatalogProvider, String> {
    let id = str_field(entry, "id")?;
    let base_url = match base_url {
        Some(url) => url.to_string(),
        None => str_field(entry, "api")?.trim_end_matches('/').to_string(),
    };
    let env: Vec<String> = entry["env"]
        .as_array()
        .map(|keys| {
            keys.iter()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if env.is_empty() {
        return Err("no credential env keys".into());
    }

    let mut models: Vec<CatalogModel> = entry["models"]
        .as_object()
        .ok_or("no models table")?
        .values()
        .filter_map(map_model)
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    if models.is_empty() {
        return Err("no usable models".into());
    }

    Ok(CatalogProvider {
        id,
        name: str_field(entry, "name")?,
        api,
        base_url,
        env,
        models,
    })
}

/// Maps one model entry, or `None` when the agent cannot use it: Suspenders
/// requires tool calls and text output, and the budget requires both limit
/// figures. Cost shapes beyond the four flat rates (the tiered pricing a few
/// openrouter models carry) are dropped - flat rates are the Catalog's
/// contract.
fn map_model(entry: &Value) -> Option<CatalogModel> {
    if entry["tool_call"] != Value::Bool(true) {
        return None;
    }
    if !entry["modalities"]["output"]
        .as_array()?
        .iter()
        .any(|m| m == "text")
    {
        return None;
    }
    let id = entry["id"].as_str()?.to_string();
    let name = entry["name"].as_str().unwrap_or(&id).to_string();
    Some(CatalogModel {
        context_window: entry["limit"]["context"].as_u64().filter(|n| *n > 0)?,
        max_tokens: entry["limit"]["output"].as_u64().filter(|n| *n > 0)?,
        reasoning: entry["reasoning"] == Value::Bool(true),
        cost: map_cost(&entry["cost"]),
        id,
        name,
    })
}

/// The four flat rates. `None` when the entry carries no pricing at all
/// (router pseudo-models); missing cache rates stay `None` within a priced
/// entry (the host does not meter them).
fn map_cost(cost: &Value) -> Option<Pricing> {
    Some(Pricing {
        input: cost["input"].as_f64()?,
        output: cost["output"].as_f64()?,
        cache_read: cost["cache_read"].as_f64(),
        cache_write: cost["cache_write"].as_f64(),
    })
}

fn str_field(entry: &Value, key: &str) -> Result<String, String> {
    entry[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field {key:?}"))
}

#[cfg(test)]
mod tests {
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
}
