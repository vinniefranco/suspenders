use super::*;
use crate::mcp::{McpServerStatus, McpServerView, McpSource, McpToolAnnotations, McpToolView};
use serde_json::json;

// ---- Fixtures ----------------------------------------------------------

fn tool(name: &str, description: &str) -> McpToolView {
    McpToolView {
        name: name.to_string(),
        description: description.to_string(),
        annotations: McpToolAnnotations::default(),
        input_schema: json!({}),
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

// A dialog whose first fetch already landed with `servers`.
fn dialog(servers: Vec<McpServerView>) -> McpDialog {
    let mut d = McpDialog::open(1);
    d.fill_ready(1, servers);
    d
}

// The flat text of a row (styles dropped), for asserting copy.
fn text(row: &McpRow) -> String {
    row.spans.iter().map(|s| s.text.as_str()).collect()
}

// The flat text of every content row, one string per row.
fn content_text(view: &McpDialogView) -> Vec<String> {
    view.content.iter().map(text).collect()
}

// ---- Loading / empty states -------------------------------------------

#[test]
fn a_fresh_dialog_shows_loading_until_the_first_fill() {
    let d = McpDialog::open(7);
    let view = d.view();
    assert_eq!(text(&view.header[0]), "Manage MCP servers");
    assert_eq!(content_text(&view), vec!["Loading…"]);
}

#[test]
fn an_empty_server_list_shows_the_configured_and_get_started_lines() {
    let d = dialog(vec![]);
    let view = d.view();
    assert_eq!(text(&view.header[1]), "0 servers");
    assert_eq!(
        content_text(&view),
        vec![
            "No MCP servers configured.",
            "Add MCP servers to your settings to get started.",
        ]
    );
    // The empty-state footer is the bare closer (qwen renderStepFooter).
    assert_eq!(text(&view.footer), "Esc to close");
}

#[test]
fn a_populated_server_list_shows_the_full_navigation_footer() {
    let d = dialog(vec![server(
        "a",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    assert_eq!(
        text(&d.view().footer),
        "↑↓ to navigate · Enter to select · Esc to close"
    );
}

// ---- SERVER_LIST grouping / cursor / status ---------------------------

#[test]
fn servers_group_by_source_in_user_project_extension_order() {
    // Deliberately out of order in the input; the grouping re-orders them.
    let d = dialog(vec![
        server("ext", McpSource::Extension, McpServerStatus::Connected),
        server("usr", McpSource::User, McpServerStatus::Connected),
        server("proj", McpSource::Workspace, McpServerStatus::Connected),
    ]);
    let rows = content_text(&d.view());
    assert_eq!(rows[0], "  User MCPs");
    assert!(rows[1].contains("usr"));
    assert_eq!(rows[2], ""); // blank between groups
    assert_eq!(rows[3], "  Project MCPs");
    assert!(rows[4].contains("proj"));
    assert_eq!(rows[5], "");
    assert_eq!(rows[6], "  Extension MCPs");
    assert!(rows[7].contains("ext"));
}

#[test]
fn the_cursor_marks_the_active_server_across_group_boundaries() {
    let mut d = dialog(vec![
        server("usr", McpSource::User, McpServerStatus::Connected),
        server("proj", McpSource::Workspace, McpServerStatus::Connected),
    ]);
    // Flat index 0 (usr) is active: its row carries the `❯`, proj's does not.
    let rows = content_text(&d.view());
    assert!(rows[1].starts_with("❯ usr"));
    assert!(rows[4].starts_with("  proj"));
    // Down moves the flat cursor past the group boundary to proj.
    assert_eq!(d.fold_key(McpKey::Down), McpFold::None);
    let rows = content_text(&d.view());
    assert!(rows[1].starts_with("  usr"));
    assert!(rows[4].starts_with("❯ proj"));
}

#[test]
fn a_server_row_shows_the_status_icon_and_word() {
    let d = dialog(vec![
        server("ok", McpSource::User, McpServerStatus::Connected),
        server("down", McpSource::User, McpServerStatus::Disconnected),
    ]);
    let rows = content_text(&d.view());
    assert!(rows[1].contains("✓ connected"), "{}", rows[1]);
    assert!(rows[2].contains("✗ failed"), "{}", rows[2]);
}

#[test]
fn a_disabled_server_reads_disabled_in_the_status_column() {
    let mut s = server("off", McpSource::User, McpServerStatus::Disconnected);
    s.is_disabled = true;
    let d = dialog(vec![s]);
    assert!(content_text(&d.view())[1].contains("✗ disabled"));
}

#[test]
fn a_server_with_invalid_tools_shows_the_invalid_tools_warning() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = vec![tool("ok", "fine"), tool("bad", "")];
    let d = dialog(vec![s]);
    assert!(content_text(&d.view())[1].contains("1 invalid tools"));
}

// ---- TOOL_LIST scroll (driven through the live dialog) ----------------

#[test]
fn the_tool_list_scrolls_and_shows_a_current_total_indicator() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = (0..15).map(|i| tool(&format!("t{i}"), "d")).collect();
    let mut d = dialog(vec![s]);
    // Into the detail, then View tools.
    let _ = d.fold_key(McpKey::Enter); // -> detail
    let _ = d.fold_key(McpKey::Enter); // View tools (first action) -> tool list
    let rows = content_text(&d.view());
    // 15 tools > window of 10: exactly 10 rows, a blank, then the indicator.
    assert_eq!(rows.iter().filter(|r| r.contains("t")).count(), 10);
    assert!(rows.last().unwrap().contains("1/15"));
    // Scrolling down keeps the indicator honest and reveals the ↑/↓ chevrons.
    for _ in 0..11 {
        let _ = d.fold_key(McpKey::Down);
    }
    let rows = content_text(&d.view());
    let indicator = rows.last().unwrap();
    assert!(indicator.contains("12/15"), "{indicator}");
    assert!(indicator.starts_with("↑"), "hidden rows above: {indicator}");
}

// ---- Navigation (push / pop / select) ---------------------------------

#[test]
fn enter_pushes_detail_and_escape_pops_back() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    // At the root the header is the list title.
    assert_eq!(text(&d.view().header[0]), "Manage MCP servers");
    // Enter -> SERVER_DETAIL (header is the server name).
    assert_eq!(d.fold_key(McpKey::Enter), McpFold::None);
    assert_eq!(text(&d.view().header[0]), "srv");
    // Escape pops back to the list.
    assert_eq!(d.fold_key(McpKey::Escape), McpFold::None);
    assert_eq!(text(&d.view().header[0]), "Manage MCP servers");
}

#[test]
fn root_escape_closes_the_dialog() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    assert_eq!(d.fold_key(McpKey::Escape), McpFold::Close);
}

#[test]
fn view_tools_navigates_without_an_agent_action() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = vec![tool("t", "d")];
    let mut d = dialog(vec![s]);
    let _ = d.fold_key(McpKey::Enter); // -> detail
    // View tools is the first action: Enter navigates, no Act.
    assert_eq!(d.fold_key(McpKey::Enter), McpFold::None);
    assert_eq!(text(&d.view().header[0]), "Tools for srv");
}

#[test]
fn picking_reconnect_emits_an_act_for_the_server() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Disconnected,
    )]);
    let _ = d.fold_key(McpKey::Enter); // -> detail; actions: [Reconnect, Disable, Authenticate]
    // Reconnect is the first action.
    assert_eq!(
        d.fold_key(McpKey::Enter),
        McpFold::Act(McpAction::Reconnect, "srv".to_string())
    );
}

#[test]
fn picking_disable_emits_a_disable_act() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter); // -> detail; actions: [Disable, Authenticate]
    // Disable is the first action (no tools -> no View tools).
    assert_eq!(
        d.fold_key(McpKey::Enter),
        McpFold::Act(McpAction::Disable, "srv".to_string())
    );
}

#[test]
fn selecting_a_tool_pushes_the_tool_detail() {
    let mut s = server("srv", McpSource::User, McpServerStatus::Connected);
    s.tools = vec![tool("alpha", "a"), tool("beta", "b")];
    let mut d = dialog(vec![s]);
    let _ = d.fold_key(McpKey::Enter); // -> detail
    let _ = d.fold_key(McpKey::Enter); // View tools -> tool list
    let _ = d.fold_key(McpKey::Down); // active tool: beta
    let _ = d.fold_key(McpKey::Enter); // -> tool detail
    assert_eq!(text(&d.view().header[0]), "beta");
}

// ---- Authenticate step + progress -------------------------------------

#[test]
fn authenticate_pushes_a_step_and_emits_the_run_act() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter); // -> detail; actions: [Disable, Authenticate]
    let _ = d.fold_key(McpKey::Down); // active: Authenticate
    assert_eq!(
        d.fold_key(McpKey::Enter),
        McpFold::Act(McpAction::Authenticate, "srv".to_string())
    );
    // The AUTHENTICATE step is now active.
    assert_eq!(text(&d.view().header[0]), "OAuth Authentication");
    assert_eq!(text(&d.view().footer), "Esc to go back");
}

#[test]
fn auth_progress_folds_into_the_open_authenticate_step() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter);
    let _ = d.fold_key(McpKey::Down);
    let _ = d.fold_key(McpKey::Enter); // -> Authenticate for srv
    assert!(d.fold_auth_progress("srv", "Starting OAuth…".into(), false));
    assert!(d.fold_auth_progress("srv", "https://auth.example/authorize".into(), true));
    let rows = content_text(&d.view());
    assert!(rows.iter().any(|r| r == "Starting OAuth…"));
    assert!(
        rows.iter()
            .any(|r| r.contains("https://auth.example/authorize"))
    );
    // A URL on screen surfaces the idle copy hint (qwen keys it off authUrl).
    assert!(
        rows.iter()
            .any(|r| r == "Press c to copy the authorization URL to your clipboard.")
    );
}

#[test]
fn auth_progress_for_the_wrong_server_or_step_is_dropped() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    // Not on the AUTHENTICATE step: dropped.
    assert!(!d.fold_auth_progress("srv", "x".into(), false));
    let _ = d.fold_key(McpKey::Enter);
    let _ = d.fold_key(McpKey::Down);
    let _ = d.fold_key(McpKey::Enter); // Authenticate for srv
    // Wrong server name: dropped.
    assert!(!d.fold_auth_progress("other", "x".into(), false));
}

// ---- OSC52 copy-URL (qwen AuthenticateStep `c`) -----------------------

// Drives a dialog into the AUTHENTICATE step for `srv` with an auth URL on
// screen, ready for the copy-key tests.
fn authenticating_with_url() -> McpDialog {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter);
    let _ = d.fold_key(McpKey::Down);
    let _ = d.fold_key(McpKey::Enter); // -> Authenticate for srv
    assert!(d.fold_auth_progress("srv", "https://auth.example/authorize".into(), true));
    d
}

#[test]
fn pressing_c_with_a_url_requests_an_osc52_copy() {
    let mut d = authenticating_with_url();
    assert_eq!(
        d.fold_key(McpKey::Copy),
        McpFold::CopyUrl("https://auth.example/authorize".to_string())
    );
}

#[test]
fn pressing_c_with_no_url_yet_is_a_no_op() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter);
    let _ = d.fold_key(McpKey::Down);
    let _ = d.fold_key(McpKey::Enter); // -> Authenticate, no URL streamed yet
    assert_eq!(d.fold_key(McpKey::Copy), McpFold::None);
}

#[test]
fn pressing_c_off_the_authenticate_step_is_a_no_op() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    // At the SERVER_LIST root, `c` does nothing (qwen only binds it in the
    // AuthenticateStep).
    assert_eq!(d.fold_key(McpKey::Copy), McpFold::None);
}

#[test]
fn the_copy_hint_reads_idle_then_copied_or_unsupported() {
    let mut d = authenticating_with_url();
    // Idle: the "Press c" prompt.
    assert!(
        content_text(&d.view())
            .iter()
            .any(|r| r == "Press c to copy the authorization URL to your clipboard.")
    );
    // A successful OSC52 write flips the hint to the copied feedback.
    d.fold_copy_result(true);
    assert!(content_text(&d.view()).iter().any(|r| r
        == "Copy request sent to your terminal. If paste is empty, copy the URL above manually."));
    // A failed write (no TTY) reads the unsupported feedback.
    d.fold_copy_result(false);
    assert!(
        content_text(&d.view())
            .iter()
            .any(|r| r == "Cannot write to terminal - copy the URL above manually.")
    );
}

// ---- Generation guard (stale fills dropped) ---------------------------

#[test]
fn a_fill_for_a_stale_generation_is_dropped() {
    let mut d = McpDialog::open(5);
    // A fill from an earlier activation must not land on this dialog.
    d.fill_ready(
        4,
        vec![server("stale", McpSource::User, McpServerStatus::Connected)],
    );
    assert!(d.is_loading(), "a stale ready is ignored, still loading");
    // The matching generation fills it.
    d.fill_ready(
        5,
        vec![server("live", McpSource::User, McpServerStatus::Connected)],
    );
    assert!(!d.is_loading());
    assert!(content_text(&d.view())[1].contains("live"));
}

#[test]
fn a_ready_fill_resets_the_stack_to_a_fresh_server_list() {
    let mut d = dialog(vec![server(
        "srv",
        McpSource::User,
        McpServerStatus::Connected,
    )]);
    let _ = d.fold_key(McpKey::Enter); // deep into SERVER_DETAIL
    assert_eq!(text(&d.view().header[0]), "srv");
    // A re-fetch (after an action) resets to the SERVER_LIST root.
    d.fill_ready(
        1,
        vec![server(
            "srv",
            McpSource::User,
            McpServerStatus::Disconnected,
        )],
    );
    assert_eq!(text(&d.view().header[0]), "Manage MCP servers");
}

#[test]
fn an_empty_ready_fill_reads_as_the_no_servers_state() {
    // `mcp_views()` fails open to an empty list (a dead Agent answers `[]`),
    // so an empty ready fill is the "failed" path too - it clears loading and
    // renders the empty state. No separate failed event exists.
    let mut d = McpDialog::open(2);
    d.fill_ready(2, vec![]);
    assert!(!d.is_loading());
    assert_eq!(
        content_text(&d.view()),
        vec![
            "No MCP servers configured.",
            "Add MCP servers to your settings to get started.",
        ]
    );
}
