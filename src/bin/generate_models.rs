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
use suspenders::content::Modalities;
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
        input_modalities: map_input_modalities(&entry["modalities"]["input"]),
        cost: map_cost(&entry["cost"]),
        id,
        name,
    })
}

/// The input modalities (ADR-0059) from models.dev's `modalities.input` array of
/// strings: `image` and `pdf` set the matching booleans; other entries (text,
/// audio, video) are not modalities Suspenders carries to the wire, so they are
/// ignored. A missing array is text-only (all-false).
fn map_input_modalities(input: &Value) -> Modalities {
    let has = |name: &str| {
        input
            .as_array()
            .is_some_and(|a| a.iter().any(|m| m.as_str() == Some(name)))
    };
    Modalities {
        image: has("image"),
        pdf: has("pdf"),
    }
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
#[path = "../../tests/unit/bin/generate_models.rs"]
mod tests;
