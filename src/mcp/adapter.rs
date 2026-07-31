//! [`McpTool`] - a discovered MCP tool wearing the Suspenders [`Tool`] contract
//! (ADR-0056).
//!
//! One discovered remote tool becomes one `McpTool`. Its spec carries the wire
//! name `mcp__<server>__<tool>` (built by [`valid_name`]) plus the description
//! and input schema the server reported. Its `run` ignores the ctx and calls
//! the [`McpConn`] seam over the bare server tool name, wrapping in a timeout
//! when the config set one, then collapses the result to canonical text
//! ([`crate::mcp::result::render`]). Every MCP tool is deferred (`should_defer`
//! true, `always_load` false) and `is_mcp` true, so it never rides the wire list
//! the model sees at Run start and the tool-search scorer weighs it slightly
//! higher.

use std::sync::Arc;

use serde_json::Value;

use crate::mcp::{McpConn, result};
use crate::tool::{Tool, ToolCtx, ToolSpec};

/// The discovered-tool identity a server reports (ADR-0056): the bare server +
/// tool names, the description, and the input schema. Grouped so the six-field
/// construction reads as `identity + connection`, and so the wire [`ToolSpec`]
/// can be built once at construction rather than re-cloned on every `spec()`.
pub struct McpToolInfo {
    pub server: String,
    pub tool: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpToolInfo {
    /// Builds the info from the parts a server's tool listing yields.
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        McpToolInfo {
            server: server.into(),
            tool: tool.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// The maximum wire-name length before elision (qwen: Gemini's API rejects names
/// over 63, despite advertising 64).
const MAX_WIRE_NAME_LEN: usize = 63;

/// The head slice kept when a wire name is elided.
const ELIDE_HEAD: usize = 28;

/// The tail slice kept when a wire name is elided.
const ELIDE_TAIL: usize = 32;

/// A discovered MCP tool as a Suspenders [`Tool`]. Holds the bare server + tool
/// names, the wire [`ToolSpec`] it presents to the model (built once so `spec()`
/// is a single clone, not three field clones), the [`McpConn`] to call, the
/// optional per-server call timeout, and the precomputed search hint.
pub struct McpTool {
    server: String,
    tool: String,
    wire_name: String,
    spec: ToolSpec,
    conn: Arc<dyn McpConn>,
    timeout_ms: Option<u64>,
    search_hint: String,
}

impl McpTool {
    /// Builds an `McpTool` from a discovered [`McpToolInfo`] and its connection.
    /// The wire name is computed once via [`valid_name`] and the wire [`ToolSpec`]
    /// is assembled here (so `spec()` clones one struct); the search hint is
    /// `mcp <server>` (qwen parity), so the model's mention of the server boosts
    /// fuzzy matching.
    pub fn new(info: McpToolInfo, conn: Arc<dyn McpConn>, timeout_ms: Option<u64>) -> Self {
        let McpToolInfo {
            server,
            tool,
            description,
            input_schema,
        } = info;
        let wire_name = valid_name(&server, &tool);
        let search_hint = format!("mcp {server}");
        let spec = ToolSpec {
            name: wire_name.clone(),
            description,
            input_schema,
        };
        McpTool {
            server,
            tool,
            wire_name,
            spec,
            conn,
            timeout_ms,
            search_hint,
        }
    }
}

/// The wire name for a discovered tool: `mcp__<server>__<tool>`, sanitized and
/// elided by qwen's `generateValidName` rules. Any character outside
/// `[a-zA-Z0-9_.-]` becomes `_`; a name over 63 chars keeps the first 28 and the
/// last 32 joined by `___`.
pub(crate) fn valid_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.chars().count() > MAX_WIRE_NAME_LEN {
        let chars: Vec<char> = sanitized.chars().collect();
        let head: String = chars.iter().take(ELIDE_HEAD).collect();
        let tail: String = chars[chars.len() - ELIDE_TAIL..].iter().collect();
        format!("{head}___{tail}")
    } else {
        sanitized
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        // The MCP call ignores the ctx: an MCP tool reaches its host through the
        // McpConn seam, not the Project Root / Result Cap the ctx carries. The
        // Result Cap still applies - `tools::run` Shapes the returned text like
        // any other tool's.
        let call = self.conn.call_tool(&self.tool, input.clone());

        let outcome = match self.timeout_ms {
            Some(ms) => {
                match tokio::time::timeout(std::time::Duration::from_millis(ms), call).await {
                    Ok(res) => res,
                    Err(_) => {
                        return Err(format!(
                            "MCP tool {:?} on server {:?} timed out after {}ms",
                            self.tool, self.server, ms
                        ));
                    }
                }
            }
            None => call.await,
        };

        match outcome {
            // The placeholder names the tool by its wire name, matching qwen
            // (its `funcResponse.name` is the wire name), so a media/blob result
            // reads `[Tool '<wire_name>' provided ...]`.
            Ok(result) => result::render(&result, &self.wire_name),
            Err(err) => Err(err.0),
        }
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn always_load(&self) -> bool {
        false
    }

    fn is_mcp(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some(&self.search_hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpBlock, McpCallResult, McpError};
    use serde_json::json;

    /// A fake [`McpConn`] answering with a fixed outcome, for the adapter tests.
    struct FakeConn {
        outcome: Result<McpCallResult, McpError>,
    }

    #[async_trait::async_trait]
    impl McpConn for FakeConn {
        async fn call_tool(
            &self,
            _tool: &str,
            _arguments: Value,
        ) -> Result<McpCallResult, McpError> {
            self.outcome.clone()
        }
    }

    fn tool_over(outcome: Result<McpCallResult, McpError>) -> McpTool {
        McpTool::new(
            McpToolInfo::new(
                "github",
                "create_issue",
                "create a github issue",
                json!({"type": "object", "properties": {}, "required": []}),
            ),
            Arc::new(FakeConn { outcome }),
            None,
        )
    }

    fn ctx() -> ToolCtx {
        ToolCtx::for_test(std::path::PathBuf::from("/tmp"), 100_000)
    }

    // ---- valid_name ----

    #[test]
    fn valid_name_is_the_plain_convention_for_a_normal_pair() {
        assert_eq!(
            valid_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
    }

    #[test]
    fn valid_name_sanitizes_spaces_and_slashes() {
        assert_eq!(
            valid_name("my server", "do/thing"),
            "mcp__my_server__do_thing"
        );
    }

    #[test]
    fn valid_name_elides_a_name_over_sixty_three_chars() {
        let long_tool = "a".repeat(80);
        let name = valid_name("srv", &long_tool);
        assert!(name.len() <= MAX_WIRE_NAME_LEN + 3); // head + "___" + tail
        assert!(name.contains("___"));
        assert!(name.starts_with("mcp__srv__aaa"));
    }

    // ---- spec passthrough ----

    #[test]
    fn spec_carries_the_wire_name_description_and_schema() {
        let tool = tool_over(Ok(McpCallResult {
            content: vec![],
            is_error: false,
        }));
        let spec = tool.spec();
        assert_eq!(spec.name, "mcp__github__create_issue");
        assert_eq!(spec.description, "create a github issue");
        assert_eq!(spec.input_schema["type"], "object");
    }

    // ---- deferral flags + hint ----

    #[test]
    fn mcp_tools_are_deferred_never_always_load_and_is_mcp() {
        let tool = tool_over(Ok(McpCallResult {
            content: vec![],
            is_error: false,
        }));
        assert!(tool.should_defer());
        assert!(!tool.always_load());
        assert!(tool.is_mcp());
        assert_eq!(tool.search_hint(), Some("mcp github"));
    }

    // ---- run() ----

    #[tokio::test]
    async fn run_collapses_text_content_to_the_joined_text() {
        let tool = tool_over(Ok(McpCallResult {
            content: vec![
                McpBlock::Text("first".into()),
                McpBlock::Text("second".into()),
            ],
            is_error: false,
        }));
        assert_eq!(
            tool.run(&json!({}), &ctx()).await,
            Ok("first\nsecond".to_string())
        );
    }

    #[tokio::test]
    async fn run_maps_media_to_a_placeholder() {
        let tool = tool_over(Ok(McpCallResult {
            content: vec![McpBlock::Media {
                kind: "image".into(),
                mime: "image/png".into(),
            }],
            is_error: false,
        }));
        assert_eq!(
            tool.run(&json!({}), &ctx()).await,
            Ok(
                "[Tool 'mcp__github__create_issue' provided the following image data with mime-type: image/png]"
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn run_maps_an_error_result_to_err() {
        let tool = tool_over(Ok(McpCallResult {
            content: vec![McpBlock::Text("bad request".into())],
            is_error: true,
        }));
        assert_eq!(
            tool.run(&json!({}), &ctx()).await,
            Err("bad request".to_string())
        );
    }

    #[tokio::test]
    async fn run_maps_a_conn_error_to_err() {
        let tool = tool_over(Err(McpError("server dropped".into())));
        assert_eq!(
            tool.run(&json!({}), &ctx()).await,
            Err("server dropped".to_string())
        );
    }
}
