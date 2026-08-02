use super::*;
use serde_json::json;

fn tool(name: &str, description: &str) -> McpToolView {
    McpToolView {
        name: name.to_string(),
        description: description.to_string(),
        annotations: McpToolAnnotations::default(),
        input_schema: json!({}),
    }
}

#[test]
fn status_icon_and_label_match_qwen() {
    assert_eq!(McpServerStatus::Connected.icon(), '✓');
    assert_eq!(McpServerStatus::Connecting.icon(), '…');
    assert_eq!(McpServerStatus::Disconnected.icon(), '✗');
    assert_eq!(McpServerStatus::Connected.label(), "connected");
    assert_eq!(McpServerStatus::Connecting.label(), "connecting");
    assert_eq!(McpServerStatus::Disconnected.label(), "failed");
}

#[test]
fn a_tool_with_name_and_description_is_valid() {
    let t = tool("create_issue", "create a github issue");
    assert!(t.is_valid());
    assert!(t.invalid_reasons().is_empty());
}

#[test]
fn a_tool_missing_a_description_is_invalid_with_that_reason() {
    let t = tool("create_issue", "");
    assert!(!t.is_valid());
    assert_eq!(t.invalid_reasons(), vec!["missing description"]);
}

#[test]
fn a_tool_missing_both_lists_name_then_description() {
    let t = tool("", "");
    assert!(!t.is_valid());
    assert_eq!(
        t.invalid_reasons(),
        vec!["missing name", "missing description"]
    );
}

#[test]
fn annotations_any_is_false_only_when_all_hints_are_unset() {
    assert!(!McpToolAnnotations::default().any());
    assert!(
        McpToolAnnotations {
            read_only: true,
            ..Default::default()
        }
        .any()
    );
}

#[test]
fn tool_and_invalid_counts_read_the_tool_list() {
    let view = McpServerView {
        name: "github".into(),
        status: McpServerStatus::Connected,
        source: McpSource::User,
        transport_display: "gh (stdio)".into(),
        cwd: None,
        trust: false,
        tools: vec![tool("ok", "fine"), tool("bad", "")],
        is_disabled: false,
        has_oauth_tokens: false,
        error: None,
    };
    assert_eq!(view.tool_count(), 2);
    assert_eq!(view.invalid_tool_count(), 1);
}

#[test]
fn format_transport_renders_stdio_with_args_and_http_with_url() {
    let stdio = McpTransport::Stdio {
        command: "python".into(),
        args: vec!["-m".into(), "srv".into()],
        env: Default::default(),
        cwd: None,
    };
    assert_eq!(format_transport(&stdio), "python -m srv (stdio)");

    let no_args = McpTransport::Stdio {
        command: "srv".into(),
        args: vec![],
        env: Default::default(),
        cwd: None,
    };
    assert_eq!(format_transport(&no_args), "srv (stdio)");

    let http = McpTransport::Http {
        url: "https://host/mcp".into(),
        headers: Default::default(),
    };
    assert_eq!(format_transport(&http), "https://host/mcp (http)");
}

// The footer health count (qwen `MCPHealthPill`'s getPillLabel input).
fn server(name: &str, status: McpServerStatus, is_disabled: bool) -> McpServerView {
    McpServerView {
        name: name.into(),
        status,
        source: McpSource::User,
        transport_display: format!("{name} (stdio)"),
        cwd: None,
        trust: false,
        tools: Vec::new(),
        is_disabled,
        has_oauth_tokens: false,
        error: None,
    }
}

#[test]
fn offline_count_counts_only_disconnected_and_not_disabled() {
    let servers = vec![
        server("ok", McpServerStatus::Connected, false),
        server("booting", McpServerStatus::Connecting, false), // suppressed
        server("down", McpServerStatus::Disconnected, false),  // counted
        server("off", McpServerStatus::Disconnected, true),    // disabled: not counted
    ];
    assert_eq!(mcp_offline_count(&servers), 1);
}

#[test]
fn offline_count_is_zero_when_nothing_is_offline() {
    let servers = vec![
        server("ok", McpServerStatus::Connected, false),
        server("booting", McpServerStatus::Connecting, false),
    ];
    assert_eq!(mcp_offline_count(&servers), 0);
    assert_eq!(mcp_offline_count(&[]), 0);
}
