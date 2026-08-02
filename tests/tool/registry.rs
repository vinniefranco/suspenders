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
    // A second reveal of the same name is idempotent (a set insert).
    registry.reveal("hidden");
    assert!(registry.is_revealed("hidden"));
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

// ---- MCP tools in a registry (F8, ADR-0056) ----

/// A [`crate::mcp::McpConn`] answering with empty text, for an McpTool
/// fixture in the registry tests.
struct QuietConn;

#[async_trait]
impl crate::mcp::McpConn for QuietConn {
    async fn call_tool(
        &self,
        _tool: &str,
        _arguments: serde_json::Value,
    ) -> Result<crate::mcp::McpCallResult, crate::mcp::McpError> {
        Ok(crate::mcp::McpCallResult {
            content: vec![],
            is_error: false,
        })
    }
}

fn mcp_tool() -> Box<dyn Tool> {
    Box::new(crate::mcp::adapter::McpTool::new(
        crate::mcp::adapter::McpToolInfo::new(
            "srv",
            "do_thing",
            "does a thing",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        std::sync::Arc::new(QuietConn),
        None,
    ))
}

#[test]
fn an_mcp_tool_is_hidden_from_specs_but_listed_in_deferred_summary() {
    let registry = ToolRegistry::new(vec![fixture("core", false, false), mcp_tool()]);
    // Hidden from the base wire list (MCP tools are all deferred).
    assert_eq!(spec_names(&registry), vec!["core".to_string()]);
    // But listed for the model to discover.
    let deferred: Vec<String> = registry
        .deferred_summary()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(deferred, vec!["mcp__srv__do_thing".to_string()]);
}

#[test]
fn revealing_an_mcp_tool_joins_specs() {
    let registry = ToolRegistry::new(vec![fixture("core", false, false), mcp_tool()]);
    registry.reveal("mcp__srv__do_thing");
    let mut names = spec_names(&registry);
    names.sort();
    assert_eq!(
        names,
        vec!["core".to_string(), "mcp__srv__do_thing".to_string()]
    );
}

#[test]
fn is_mcp_is_true_for_the_adapter_and_false_for_a_builtin() {
    let registry = ToolRegistry::new(vec![fixture("core", false, false), mcp_tool()]);
    assert!(registry.is_mcp("mcp__srv__do_thing"));
    assert!(!registry.is_mcp("core"));
    assert!(!registry.is_mcp("missing"));
}

#[test]
fn with_shared_gives_two_registries_independent_reveals_over_one_arc() {
    let shared: std::sync::Arc<[Box<dyn Tool>]> = vec![
        fixture("core", false, false),
        fixture("hidden", true, false),
    ]
    .into();
    let a = ToolRegistry::with_shared(std::sync::Arc::clone(&shared));
    let b = ToolRegistry::with_shared(std::sync::Arc::clone(&shared));

    a.reveal("hidden");
    // The reveal is registry-local: `a` sees it, `b` does not, even though
    // both share the one tool-set Arc.
    assert!(a.is_revealed("hidden"));
    assert!(!b.is_revealed("hidden"));
    assert_eq!(spec_names(&b), vec!["core".to_string()]);
    let mut a_names = spec_names(&a);
    a_names.sort();
    assert_eq!(a_names, vec!["core".to_string(), "hidden".to_string()]);
}
