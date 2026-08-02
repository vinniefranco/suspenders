use super::*;
use crate::mcp::{McpBlock, McpCallResult, McpError};
use serde_json::json;

/// A fake [`McpConn`] answering with a fixed outcome, for the adapter tests.
struct FakeConn {
    outcome: Result<McpCallResult, McpError>,
}

#[async_trait::async_trait]
impl McpConn for FakeConn {
    async fn call_tool(&self, _tool: &str, _arguments: Value) -> Result<McpCallResult, McpError> {
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
