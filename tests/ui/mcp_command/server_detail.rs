use super::*;
use crate::mcp::McpToolView;

fn tool(name: &str, description: &str) -> McpToolView {
    McpToolView {
        name: name.to_string(),
        description: description.to_string(),
        annotations: crate::mcp::McpToolAnnotations::default(),
        input_schema: serde_json::json!({}),
    }
}

fn server(name: &str, source: McpSource, status: McpServerStatus) -> McpServerView {
    McpServerView {
        name: name.to_string(),
        status,
        source,
        transport_display: format!("{name} (stdio)"),
        cwd: None,
        trust: false,
        tools: Vec::new(),
        is_disabled: false,
        has_oauth_tokens: false,
        error: None,
    }
}

fn text(row: &McpRow) -> String {
    row.spans.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn a_connected_server_with_tools_offers_view_disable_authenticate() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = vec![tool("t", "d")];
    assert_eq!(
        detail_actions(&s),
        vec![
            McpAction::ViewTools,
            McpAction::Disable,
            McpAction::Authenticate
        ]
    );
}

#[test]
fn a_disconnected_server_adds_reconnect_and_hides_view_tools_with_no_tools() {
    let s = server("srv", McpSource::User, McpServerStatus::Disconnected);
    // No tools + disconnected: View tools hidden, Reconnect shown.
    assert_eq!(
        detail_actions(&s),
        vec![
            McpAction::Reconnect,
            McpAction::Disable,
            McpAction::Authenticate
        ]
    );
}

#[test]
fn a_disabled_server_offers_only_enable() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Disconnected);
    s.is_disabled = true;
    s.tools = vec![tool("t", "d")];
    s.has_oauth_tokens = true;
    // Disabled: no View tools, no Reconnect, no Authenticate, no Clear - just
    // Enable (qwen shows the toggle always, everything else gated on !disabled).
    assert_eq!(detail_actions(&s), vec![McpAction::Enable]);
}

#[test]
fn an_authenticated_server_adds_clear_and_reads_re_authenticate() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = vec![tool("t", "d")];
    s.has_oauth_tokens = true;
    assert_eq!(
        detail_actions(&s),
        vec![
            McpAction::ViewTools,
            McpAction::Disable,
            McpAction::Authenticate,
            McpAction::ClearAuth,
        ]
    );
    // The Authenticate label flips to Re-authenticate when tokens exist.
    assert_eq!(
        action_label(McpAction::Authenticate, true),
        "Re-authenticate"
    );
    assert_eq!(action_label(McpAction::Authenticate, false), "Authenticate");
}

#[test]
fn the_detail_lines_show_the_key_value_facts() {
    let mut s = server("srv", McpSource::Workspace, McpServerStatus::Connected);
    s.transport_display = "python -m srv (stdio)".into();
    s.cwd = Some("/work".into());
    s.tools = vec![tool("ok", "fine"), tool("bad", "")];
    let rows = detail_lines(&s);
    let joined: Vec<String> = rows.iter().map(text).collect();
    assert!(joined[0].contains("Status:") && joined[0].contains("✓ connected"));
    assert!(joined[1].contains("Source:") && joined[1].contains("Workspace Settings"));
    assert!(joined[2].contains("Command:") && joined[2].contains("python -m srv (stdio)"));
    assert!(joined[3].contains("Working Directory:") && joined[3].contains("/work"));
    assert!(joined[4].contains("Tools:") && joined[4].contains("2 tools"));
    assert!(joined[4].contains("(1 invalid)"));
}

#[test]
fn the_error_line_shows_only_when_the_server_failed() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Disconnected);
    s.error = Some("connection refused".into());
    let rows = detail_lines(&s);
    assert!(
        rows.iter()
            .map(text)
            .any(|r| r.contains("Error:") && r.contains("connection refused"))
    );
    // A connected server has no Error line.
    let ok = server("ok", McpSource::User, McpServerStatus::Connected);
    assert!(
        !detail_lines(&ok)
            .iter()
            .map(text)
            .any(|r| r.contains("Error:"))
    );
}
