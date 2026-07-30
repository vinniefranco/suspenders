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
use std::sync::Mutex;

use crate::tool::{Tool, ToolCtx, ToolSpec, validate};
use crate::tools::ToolResult;

/// The tool set plus the revealed-deferred name set. A `Box<dyn Tool>` is not
/// `Debug`, so the [`Debug`] impl below is hand-written (tool count + names)
/// rather than derived.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    revealed: Mutex<BTreeSet<String>>,
}

impl ToolRegistry {
    /// Builds a registry over the given tool set (prompt order preserved).
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
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
        self.revealed
            .lock()
            .expect("revealed set mutex poisoned")
            .insert(name.to_string());
    }

    /// Removes a single name from the revealed set (`unrevealDeferredTool`).
    ///
    /// Why kept without a production caller yet: qwen unreveals on the
    /// MCP-disconnect rollback path (a server drops, its tools stop being
    /// reachable). Suspenders gains that caller when the MCP subsystem lands
    /// (F8); the parity surface is built now so the reveal state model is
    /// complete.
    pub fn unreveal(&self, name: &str) {
        self.revealed
            .lock()
            .expect("revealed set mutex poisoned")
            .remove(name);
    }

    /// Whether a tool has been revealed via [`reveal`] this Run.
    pub fn is_revealed(&self, name: &str) -> bool {
        self.revealed
            .lock()
            .expect("revealed set mutex poisoned")
            .contains(name)
    }

    /// Empties the revealed set (`clearRevealedDeferredTools`). Suspenders builds
    /// a fresh registry per Run, so this exists for parity and explicit resets
    /// rather than being on the hot path.
    ///
    /// Why kept without a production caller yet: qwen clears reveals on session
    /// reset (`/clear`). Suspenders' per-Run fresh registry already gives a
    /// clean slate, but the explicit reset stays on the parity surface for an
    /// in-Run reset the later phases may need.
    pub fn clear_revealed(&self) {
        self.revealed
            .lock()
            .expect("revealed set mutex poisoned")
            .clear();
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
            None => {
                return ToolResult {
                    content: format!("unknown tool: {name:?}"),
                    is_error: true,
                };
            }
        };

        // Validate against the tool's own schema before dispatch. The empty-map
        // case is handled by using an empty object when input is not an object.
        let empty = serde_json::Map::new();
        let input_map = input.as_object().unwrap_or(&empty);
        if let Err(reason) = validate(&tool.spec().input_schema, input_map) {
            return ToolResult {
                content: reason,
                is_error: true,
            };
        }

        match tool.run(input, ctx).await {
            Ok(content) => ToolResult {
                content,
                is_error: false,
            },
            Err(reason) => ToolResult {
                content: reason,
                is_error: true,
            },
        }
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

/// A registry over the full built-in tool set, for tests that need a real
/// `ToolCtx` but don't care about the registry's contents. Kept here so a
/// future ctx field (F1) touches one construction site, not every test helper.
#[cfg(test)]
pub fn test_registry() -> std::sync::Arc<ToolRegistry> {
    std::sync::Arc::new(ToolRegistry::new(crate::tools::tools()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolSpec;
    use async_trait::async_trait;
    use serde_json::json;

    /// A minimal fixture tool whose deferral flags are set per test.
    struct Fixture {
        name: &'static str,
        defer: bool,
        always: bool,
    }

    #[async_trait]
    impl Tool for Fixture {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: format!("{} description", self.name),
                input_schema: json!({"type": "object", "properties": {}, "required": []}),
            }
        }
        async fn run(&self, _input: &serde_json::Value, _ctx: &ToolCtx) -> Result<String, String> {
            Ok(self.name.to_string())
        }
        fn should_defer(&self) -> bool {
            self.defer
        }
        fn always_load(&self) -> bool {
            self.always
        }
    }

    fn fixture(name: &'static str, defer: bool, always: bool) -> Box<dyn Tool> {
        Box::new(Fixture {
            name,
            defer,
            always,
        })
    }

    fn spec_names(registry: &ToolRegistry) -> Vec<String> {
        registry.specs().into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn specs_excludes_deferred_non_always_load_unrevealed() {
        let registry = ToolRegistry::new(vec![
            fixture("visible", false, false),
            fixture("hidden", true, false),
        ]);
        assert_eq!(spec_names(&registry), vec!["visible".to_string()]);
    }

    #[test]
    fn specs_includes_always_load_even_when_deferred() {
        let registry = ToolRegistry::new(vec![
            fixture("visible", false, false),
            fixture("meta", true, true),
        ]);
        assert_eq!(
            spec_names(&registry),
            vec!["visible".to_string(), "meta".to_string()]
        );
    }

    #[test]
    fn specs_includes_a_revealed_deferred_tool() {
        let registry = ToolRegistry::new(vec![
            fixture("visible", false, false),
            fixture("hidden", true, false),
        ]);
        registry.reveal("hidden");
        let mut names = spec_names(&registry);
        names.sort();
        assert_eq!(names, vec!["hidden".to_string(), "visible".to_string()]);
    }

    #[test]
    fn reveal_round_trip() {
        let registry = ToolRegistry::new(vec![fixture("hidden", true, false)]);
        assert!(!registry.is_revealed("hidden"));
        registry.reveal("hidden");
        assert!(registry.is_revealed("hidden"));
        registry.unreveal("hidden");
        assert!(!registry.is_revealed("hidden"));
        registry.reveal("hidden");
        registry.clear_revealed();
        assert!(!registry.is_revealed("hidden"));
    }

    #[test]
    fn deferred_summary_is_sorted_and_excludes_always_load() {
        let registry = ToolRegistry::new(vec![
            fixture("zebra", true, false),
            fixture("alpha", true, false),
            fixture("meta", true, true),
            fixture("visible", false, false),
        ]);
        let summary = registry.deferred_summary();
        let names: Vec<String> = summary.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["alpha".to_string(), "zebra".to_string()]);
        assert_eq!(summary[0].1, "alpha description");
    }

    #[test]
    fn canonical_name_is_case_insensitive() {
        let registry = ToolRegistry::new(vec![fixture("Cron_Create", true, false)]);
        assert_eq!(
            registry.canonical_name("cron_create"),
            Some("Cron_Create".to_string())
        );
        assert_eq!(registry.canonical_name("missing"), None);
    }

    #[test]
    fn is_loadable_tracks_defer_and_always_load() {
        let registry = ToolRegistry::new(vec![
            fixture("deferred", true, false),
            fixture("meta", true, true),
            fixture("core", false, false),
        ]);
        assert!(registry.is_loadable("deferred"));
        assert!(!registry.is_loadable("meta"));
        assert!(!registry.is_loadable("core"));
        assert!(!registry.is_loadable("missing"));
    }
}
