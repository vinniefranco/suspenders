//! `tool_search` - the discovery tool for on-demand loading of deferred tool
//! schemas.
//!
//! Only a curated set of core tools ride the wire list the model sees at the
//! start of a Run; tools marked deferred (MCP tools, low-frequency built-ins)
//! are hidden to keep the system prompt small. The model uses this tool to look
//! those hidden tools up by keyword or exact name, which reveals their full
//! schemas into the next request's wire list.
//!
//! Two query modes:
//!   * `select:Name1,Name2` - exact lookup by tool name.
//!   * free-text keywords - fuzzy match with scoring across name, description,
//!     and optional `search_hint`. MCP tools get a slight score boost since they
//!     are always deferred and thus always benefit from surfacing.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

const DEFAULT_MAX_RESULTS: i64 = 5;
const HARD_MAX_RESULTS: i64 = 20;

// Scoring weights mirror the Claude Code spec: MCP tools are weighted slightly
// higher because they are always deferred and discovery is the only way the
// model can reach them.
const SCORE_NAME_EXACT_BUILTIN: i64 = 10;
const SCORE_NAME_SUBSTR_BUILTIN: i64 = 5;
const SCORE_HINT_BUILTIN: i64 = 4;
const SCORE_DESC_BUILTIN: i64 = 2;
const SCORE_NAME_EXACT_MCP: i64 = 12;
const SCORE_NAME_SUBSTR_MCP: i64 = 6;

// Ported from qwen's `toolSearchDescription`, reworded where it named Gemini
// internals: qwen's `setTools()`/session-declaration-list machinery does not
// exist in Suspenders. Here the wire list is recomputed synchronously from the
// registry at the top of every request, so there is no separate declaration
// cache to sync - a reveal is picked up on the NEXT request automatically. The
// substantive guidance (deferred tools appear by name in the Deferred Tools
// section; select:/keyword/+must-word forms; the returned block is
// informational) is kept verbatim in spirit.
const TOOL_SEARCH_DESCRIPTION: &str = "Fetches full schema definitions for deferred tools so they can be called.

Deferred tools appear by name in the \"Deferred Tools\" section of the system prompt. Until fetched, only the name is known - there is no parameter schema, so the tool cannot be invoked. This tool takes a query, matches it against the deferred tool list, and returns the matched tools' function declarations (name + description + parameter schema) inside a <functions> block. Once fetched, a tool's schema is on the wire list for the rest of this Run and can be called on the next turn.

The returned <functions> block is informational - it shows what the schema looks like. Calling the tool itself happens via the normal Tool Call mechanism on the NEXT turn, once the wire list has been recomputed to include it.

Query forms:
- \"select:ToolA,ToolB\" - fetch these exact tools by name
- \"keyword phrase\" - keyword search, up to max_results best matches
- \"+must-word other\" - require \"must-word\" in the name, rank remaining terms
";

/// The discovery tool. A unit struct: its deferral flags say it is never hidden
/// (`always_load` true), so it is always on the wire list the model sees.
pub struct ToolSearch;

#[async_trait::async_trait]
impl Tool for ToolSearch {
    // Other (qwen tool-search.ts `Kind.Other`). Note: qwen keys plan-mode
    // blocking on the confirmation TYPE, not the Kind, and tool_search needs no
    // confirmation (it just reveals schemas), so qwen never blocks it in plan
    // mode. Suspenders keys the block on the Kind and `tool_search` is ungated
    // (never in GATED), so `classify` DOES block it in plan mode as an `Other`
    // Kind. This is a faithful-as-possible narrowing: discovering a tool's schema
    // mid-plan is rare and harmless to defer, and matching qwen's TYPE-based
    // carve-out would require a per-tool "needs no confirmation" fact this phase
    // does not introduce. Documented, not hidden.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Other
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "tool_search".to_string(),
            description: TOOL_SEARCH_DESCRIPTION.to_string(),
            // Schema ported VERBATIM from qwen (query minLength 1; max_results
            // integer min 1 max 20 default 5; additionalProperties false;
            // required ["query"]). NOTE: Suspenders' `validate` has no numeric
            // bound checking, so qwen's build-time `max_results > 20` rejection
            // is NOT reproduced here - the internal `clamp(1, 20)` in `run` is
            // the only cap (see `run`). The schema still advertises the bounds
            // so a schema-aware model self-limits.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query to find deferred tools. Use \"select:<tool_name>\" for direct selection, or keywords to search.",
                        "minLength": 1,
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)",
                        "minimum": 1,
                        "maximum": HARD_MAX_RESULTS,
                        "default": DEFAULT_MAX_RESULTS,
                    },
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        }
    }

    fn should_defer(&self) -> bool {
        false
    }

    fn always_load(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("tool search discover find schema")
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            // Runtime guard for whitespace-only inputs that pass minLength.
            return Err(
                "Error: query is empty. Use `select:ToolName` or free-text keywords.".to_string(),
            );
        }

        let max_results = clamp(
            input
                .get("max_results")
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_MAX_RESULTS),
            1,
            HARD_MAX_RESULTS,
        ) as usize;

        let registry = ctx.registry();

        // Mode 1: exact lookup via `select:Name1,Name2`. Dedupe so the same tool
        // isn't returned twice when the model writes the same name twice. Cap at
        // max_results - without a cap, `select:a,b,c,...` would return an
        // unbounded number of full schemas (token bloat). When truncation
        // happens, surface the dropped names so the model knows to re-issue
        // another tool_search for them instead of silently assuming they loaded.
        if query.to_lowercase().starts_with("select:") {
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            let mut names: Vec<String> = Vec::new();
            let mut truncated: Vec<String> = Vec::new();
            for raw in query["select:".len()..].split(',') {
                // The Deferred Tools system-prompt section renders names as JSON
                // string literals ("cron_list"), so models often paste them back
                // verbatim with surrounding quotes. Strip a single layer of
                // matching `"..."` or `'...'` so `select:"foo"` and `select:foo`
                // resolve to the same tool - otherwise the lookup would search
                // for a tool literally named `"foo"` (with quotes) and miss.
                let stripped = strip_matching_quotes(raw.trim());
                if stripped.is_empty() {
                    continue;
                }
                let key = stripped.to_lowercase();
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                if names.len() >= max_results {
                    truncated.push(stripped.to_string());
                    continue;
                }
                names.push(stripped.to_string());
            }
            return Ok(load_and_return_schemas(registry, &names, &truncated));
        }

        // Mode 2: keyword search. A `+` prefix marks a required term; any tool
        // missing a required term is excluded before scoring.
        let terms = tokenize(&query);
        let required_terms: Vec<String> = terms
            .iter()
            .filter(|t| t.starts_with('+'))
            .map(|t| t[1..].to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let search_terms: Vec<String> = terms
            .iter()
            .map(|t| {
                if let Some(rest) = t.strip_prefix('+') {
                    rest.to_string()
                } else {
                    t.to_string()
                }
            })
            .filter(|t| !t.is_empty())
            .collect();

        if search_terms.is_empty() {
            return Err("Error: no search terms extracted from query. Use `select:ToolName` or include keywords.".to_string());
        }

        // Candidates for keyword search: only deferred tools that have NOT yet
        // been revealed this Run. Already-loaded (core) tools are on the wire
        // list already, so surfacing them here would be noise; already-revealed
        // deferred tools are on the wire list too, so re-surfacing them wastes
        // tokens and risks the model retrying a tool it already has. `select:`
        // mode is unrestricted (the model may re-inspect a loaded schema).
        let mut scored: Vec<(String, i64)> = Vec::new();
        for name in registry.all_tool_names() {
            if !(registry.is_loadable(&name) && !registry.is_revealed(&name)) {
                continue;
            }
            let spec = match registry.spec_of(&name) {
                Some(s) => s,
                None => continue,
            };
            if !candidate_matches_required(&spec.name, &required_terms) {
                continue;
            }
            // MCP-discovered tools score slightly higher (F8, ADR-0056): they
            // are always deferred, so surfacing them through search is the only
            // way the model reaches them. The flag comes off the live registry.
            let score = score_tool(
                &spec,
                registry.search_hint_of(&name).as_deref(),
                &search_terms,
                registry.is_mcp(&name),
            );
            if score > 0 {
                scored.push((spec.name, score));
            }
        }

        // Sort by score desc, then name asc for a deterministic tie-break.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let matches: Vec<String> = scored
            .into_iter()
            .take(max_results)
            .map(|(n, _)| n)
            .collect();
        if matches.is_empty() {
            return Ok(format!(
                "No tools found matching '{query}'. Try broader keywords or use `select:ToolName`."
            ));
        }
        Ok(load_and_return_schemas(registry, &matches, &[]))
    }
}

/// Resolves the requested names to schemas, revealing deferred ones, and renders
/// the `<functions>` block plus any "Not found" / "Truncated" notices. Ported
/// from qwen's `loadAndReturnSchemas`, minus the setTools/rollback machinery
/// (Suspenders reveals are infallible; see `tool_registry`).
fn load_and_return_schemas(
    registry: &crate::tool_registry::ToolRegistry,
    names: &[String],
    truncated: &[String],
) -> String {
    if names.is_empty() {
        return "Error: no tool names provided.".to_string();
    }

    let mut loaded: Vec<ToolSpec> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for requested in names {
        // Case-insensitive lookup across all known names. Preserve the
        // user-supplied casing in the missing list so the response matches what
        // the model asked for.
        let canonical = match registry.canonical_name(requested) {
            Some(c) => c,
            None => {
                missing.push(requested.clone());
                continue;
            }
        };
        let spec = match registry.spec_of(&canonical) {
            Some(s) => s,
            None => {
                missing.push(requested.clone());
                continue;
            }
        };
        // Only reveal when the tool is actually deferred. `select:` mode also
        // accepts already-loaded / always_load tools (the model may re-inspect a
        // schema) - those don't need revealing (they're already on the wire
        // list). Reveal happens BEFORE the read so the schema is coherent with
        // the next request's wire list.
        if registry.is_loadable(&canonical) {
            registry.reveal(&canonical);
        }
        loaded.push(spec);
    }

    // Each block carries the tool's Anthropic spec verbatim - name,
    // description, and `input_schema`. qwen emits its Gemini
    // function-declaration shape (a `parametersJsonSchema` field); Suspenders is
    // Anthropic-native (ADR-0002), so the block serializes the real `ToolSpec`
    // (`input_schema`), matching how the model actually calls the tool on the
    // wire. This field-name difference is the one forced adaptation here.
    //
    // Escape `<` in the JSON-stringified schema so any `</function>` (or
    // `</functions>`) substring inside a tool's description / enum / examples
    // can't prematurely close the pseudo-XML wrapper. The `<` JSON unicode
    // escape decodes back to `<` when the model interprets the JSON, but as raw
    // text inside the wrapper it is no longer the start of a closing tag.
    let schema_blocks: Vec<String> = loaded
        .iter()
        .map(|spec| {
            let encoded = serde_json::to_string(spec).unwrap_or_default();
            format!("<function>{}</function>", encoded.replace('<', "\\u003c"))
        })
        .collect();

    let mut content = String::new();
    if !schema_blocks.is_empty() {
        content.push_str(&format!(
            "<functions>\n{}\n</functions>",
            schema_blocks.join("\n")
        ));
    }
    if !missing.is_empty() {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&format!("Not found: {}", missing.join(", ")));
    }
    if !truncated.is_empty() {
        // Surface the dropped names so the model knows it must re-issue another
        // tool_search for them - without this, the model would assume every
        // requested name loaded and later hit an "unknown tool" error.
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&format!(
            "Truncated by max_results - request these in a follow-up call: {}",
            truncated.join(", ")
        ));
    }

    content
}

// ---------- pure helpers (exported within the crate for tests) ----------

/// Splits a query on whitespace, lowercases, and drops empty tokens.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split_whitespace()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Clamps `n` into `[lo, hi]`. (Suspenders passes an already-integer i64, so
/// there is no float-finiteness branch to port.)
pub(crate) fn clamp(n: i64, lo: i64, hi: i64) -> i64 {
    hi.min(lo.max(n))
}

/// Strips a single layer of surrounding `"..."` or `'...'` if present. Used to
/// normalize `select:"foo"` -> `foo` so models that paste tool names back as
/// JSON-quoted literals (the form they appear in in the Deferred Tools section)
/// resolve correctly. Mismatched / unbalanced quotes are returned unchanged.
pub(crate) fn strip_matching_quotes(s: &str) -> &str {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return s;
    }
    let first = chars[0];
    let last = chars[chars.len() - 1];
    if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
        // Strip exactly one char from each end (both are single-byte quotes).
        return &s[1..s.len() - 1];
    }
    s
}

/// True when every required term is a substring of the tool's (lowercased) name.
pub(crate) fn candidate_matches_required(name: &str, required_terms: &[String]) -> bool {
    if required_terms.is_empty() {
        return true;
    }
    let name_lower = name.to_lowercase();
    required_terms
        .iter()
        .all(|t| name_lower.contains(t.as_str()))
}

/// Scores a tool against the search terms. Returns 0 if no signal matched; the
/// caller filters by `> 0`. `is_mcp` selects the MCP weight branch (all real
/// tools pass false today; the branch is pinned by unit tests for when MCP
/// lands).
pub(crate) fn score_tool(
    spec: &ToolSpec,
    search_hint: Option<&str>,
    terms: &[String],
    is_mcp: bool,
) -> i64 {
    let name_lower = spec.name.to_lowercase();
    let desc_lower = spec.description.to_lowercase();
    let hint_lower = search_hint.unwrap_or("").to_lowercase();
    let hint_parts: Vec<String> = if hint_lower.is_empty() {
        Vec::new()
    } else {
        hint_lower
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    };

    let mut total = 0;
    for term in terms {
        if term.is_empty() {
            continue;
        }
        if name_lower == *term
            || name_lower.ends_with(&format!("_{term}"))
            || name_lower.ends_with(&format!(".{term}"))
        {
            total += if is_mcp {
                SCORE_NAME_EXACT_MCP
            } else {
                SCORE_NAME_EXACT_BUILTIN
            };
        } else if name_lower.contains(term.as_str()) {
            total += if is_mcp {
                SCORE_NAME_SUBSTR_MCP
            } else {
                SCORE_NAME_SUBSTR_BUILTIN
            };
        }
        // Hint matches are per-word, mirroring Claude's "word boundary" rule.
        if hint_parts.iter().any(|p| p == term) {
            total += SCORE_HINT_BUILTIN;
        }
        if desc_lower.contains(term.as_str()) {
            total += SCORE_DESC_BUILTIN;
        }
    }
    total
}

#[cfg(test)]
#[path = "../../tests/tools/tool_search.rs"]
mod tests;
