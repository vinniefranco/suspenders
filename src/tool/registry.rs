//! The Tool Registry: the Run-scoped home of the tool set and its reveal state.
//!
//! qwen keeps a live `ToolRegistry` on its `Config` and mutates a chat's
//! declaration list through `setTools()`. Suspenders has no such cache: the
//! wire list the model sees is recomputed synchronously from the registry at
//! the top of every request (`loop_.rs`). So the registry here holds the boxed
//! tool set plus a set of *revealed* deferred-tool names, and [`specs`] filters
//! the wire list on every read. A `tool_search` reveal is infallible - it just
//! flips a name into the revealed set, and the next request picks it up.
//!
//! The registry is built once per Run and shared through the [`ToolCtx`] on an
//! `Arc`; reveals are therefore Run-scoped and reset when the next Run builds a
//! fresh registry (matching qwen's `clearRevealedDeferredTools` on session
//! reset).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

use crate::tool::{Tool, ToolCtx, ToolResult, ToolSpec, validate};

/// The tool set plus the revealed-deferred name set. A `Box<dyn Tool>` is not
/// `Debug`, so the [`Debug`] impl below is hand-written (tool count + names)
/// rather than derived.
///
/// The tool set rides an `Arc<[Box<dyn Tool>]>` so one Session-stable set backs
/// every per-Run registry without re-boxing the tools each Run (F8, ADR-0056):
/// the Agent builds the set once (built-ins + discovered MCP tools) and each Run
/// gets a fresh [`with_shared`](ToolRegistry::with_shared) registry over the
/// same `Arc` with its own empty revealed set. `Arc<[T]>` derefs to `[T]`, so
/// every read below is unchanged.
pub struct ToolRegistry {
    tools: Arc<[Box<dyn Tool>]>,
    revealed: Mutex<BTreeSet<String>>,
}

impl ToolRegistry {
    /// Builds a registry over the given tool set (prompt order preserved). The
    /// `Vec` is moved into the backing `Arc<[…]>`; a caller that needs to share
    /// one set across Runs uses [`with_shared`](ToolRegistry::with_shared).
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        ToolRegistry {
            tools: tools.into(),
            revealed: Mutex::new(BTreeSet::new()),
        }
    }

    /// Builds a registry sharing an existing `Arc<[Box<dyn Tool>]>` - the
    /// Session-stable tool set the Agent built once - with a FRESH empty
    /// revealed set (F8, ADR-0056). Cloning the `Arc` is a refcount bump; the
    /// tools are not re-boxed. Two registries over the same `Arc` keep
    /// independent reveal state (each Run reveals on its own copy).
    pub fn with_shared(tools: Arc<[Box<dyn Tool>]>) -> Self {
        ToolRegistry {
            tools,
            revealed: Mutex::new(BTreeSet::new()),
        }
    }

    /// The wire list the model sees: every tool EXCEPT deferred, non-always-load
    /// tools that have not been revealed this Run. Mirrors qwen's
    /// `getFunctionDeclarations` default (`shouldDefer && !alwaysLoad &&
    /// !revealed` are dropped).
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|t| !self.is_deferred_hidden(t.as_ref()))
            .map(|t| t.spec())
            .collect()
    }

    /// True when a tool is a deferred, non-always-load tool that has not yet
    /// been revealed - i.e. it is currently hidden from the wire list.
    fn is_deferred_hidden(&self, tool: &dyn Tool) -> bool {
        tool.should_defer() && !tool.always_load() && !self.is_revealed(&tool.spec().name)
    }

    /// Every registered tool name, in registry order. Used by `tool_search` for
    /// case-insensitive canonical-name lookup.
    pub fn all_tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    /// Looks a tool up by name, case-insensitively, returning the canonical
    /// (registry-cased) name. `tool_search` uses this to resolve `select:` names
    /// the model may have re-cased.
    pub fn canonical_name(&self, requested: &str) -> Option<String> {
        let lower = requested.to_lowercase();
        self.tools
            .iter()
            .map(|t| t.spec().name)
            .find(|n| n.to_lowercase() == lower)
    }

    /// The spec of a tool by exact (canonical) name, or `None`. `tool_search`
    /// uses this to render a resolved tool's schema block.
    pub fn spec_of(&self, canonical: &str) -> Option<ToolSpec> {
        self.tools
            .iter()
            .find(|t| t.spec().name == canonical)
            .map(|t| t.spec())
    }

    /// The search-hint keywords of a tool by exact (canonical) name, if it has
    /// any. Used by `tool_search`'s keyword scoring.
    pub fn search_hint_of(&self, canonical: &str) -> Option<String> {
        self.tools
            .iter()
            .find(|t| t.spec().name == canonical)
            .and_then(|t| t.search_hint().map(str::to_string))
    }

    /// True when the registry holds a tool with this exact (canonical) name that
    /// is deferred and not always-load - i.e. revealing it actually matters.
    pub fn is_loadable(&self, canonical: &str) -> bool {
        self.tools
            .iter()
            .find(|t| t.spec().name == canonical)
            .map(|t| t.should_defer() && !t.always_load())
            .unwrap_or(false)
    }

    /// Whether the tool with this exact (canonical) name was discovered from an
    /// MCP server (F8, ADR-0056). `tool_search` reads it to weigh MCP tools
    /// slightly higher in its scoring. An unknown name answers false.
    pub fn is_mcp(&self, canonical: &str) -> bool {
        self.tools
            .iter()
            .find(|t| t.spec().name == canonical)
            .map(|t| t.is_mcp())
            .unwrap_or(false)
    }

    /// The lightweight `(name, description)` summary of deferred (non-always-load)
    /// tools, sorted by name, that describes the on-demand set in the system
    /// prompt's Deferred Tools section. Mirrors qwen's `getDeferredToolSummary`.
    pub fn deferred_summary(&self) -> Vec<(String, String)> {
        let mut summary: Vec<(String, String)> = self
            .tools
            .iter()
            .filter(|t| t.should_defer() && !t.always_load())
            .map(|t| {
                let spec = t.spec();
                (spec.name, spec.description)
            })
            .collect();
        // Stable order so the system-prompt text is deterministic across runs.
        summary.sort_by(|a, b| a.0.cmp(&b.0));
        summary
    }

    /// Marks a deferred tool as revealed for the rest of this Run. Infallible:
    /// there is no declaration cache to re-sync, so a reveal is just a set
    /// insert - the next request's [`specs`] picks it up. Mirrors
    /// `revealDeferredTool`.
    pub fn reveal(&self, name: &str) {
        self.revealed_set().insert(name.to_string());
    }

    /// Whether a tool has been revealed via [`reveal`] this Run.
    pub fn is_revealed(&self, name: &str) -> bool {
        self.revealed_set().contains(name)
    }

    /// The revealed-name set, recovering a poisoned lock rather than panicking.
    /// A poisoned lock means a prior holder panicked mid-insert; the `BTreeSet`
    /// of owned `String`s is still structurally sound, so recover the guard the
    /// same way [`crate::tool::read_cache::FileReadCache`] does - a reveal must
    /// never bring down a tool dispatch.
    fn revealed_set(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.revealed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Validates the model-supplied input against the named tool's schema, then
    /// dispatches. An unknown name and an `Err` return both come back as
    /// `is_error` results. This is the body that used to live in
    /// `tools::execute` - it moved here so the registry owns dispatch over its
    /// own tool set (the wire list and dispatch share one source of truth).
    pub async fn execute(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolResult {
        let tool = match self.tools.iter().find(|t| t.spec().name == name) {
            Some(t) => t,
            None => return error_result(format!("unknown tool: {name:?}")),
        };

        // Validate against the tool's own schema before dispatch. The empty-map
        // case is handled by using an empty object when input is not an object.
        let empty = serde_json::Map::new();
        let input_map = input.as_object().unwrap_or(&empty);
        if let Err(reason) = validate(&tool.spec().input_schema, input_map) {
            return error_result(reason);
        }

        // Dispatch through the rich variant (ADR-0059): a text tool's `String`
        // return becomes one Text block via the default `run_rich`; only a media
        // tool (P3 3b's read_file) yields more. An `Err` is a single Text error
        // block.
        match tool.run_rich(input, ctx).await {
            Ok(output) => ToolResult {
                content: output.blocks,
                is_error: false,
            },
            Err(reason) => error_result(reason),
        }
    }
}

/// An `is_error` Tool Result carrying a single Text block - the validate and
/// unknown-tool paths, and a tool's `Err` return (ADR-0059).
fn error_result(reason: String) -> ToolResult {
    ToolResult {
        content: vec![crate::content::ResultBlock::text(reason)],
        is_error: true,
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.tools.iter().map(|t| t.spec().name).collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names.len())
            .field("names", &names)
            .field("revealed", &self.revealed)
            .finish()
    }
}

#[cfg(test)]
#[path = "../../tests/tool/registry.rs"]
mod tests;
