//! The SERVER_LIST step render (ADR-0065 Phase E, qwen's `ServerListStep`): the
//! "Manage MCP servers" header + count, the servers grouped by source with a
//! cursor and status word, and the empty/loading states. One pure builder over
//! the [`McpServerView`] read model (ADR-0001/0019); [`super::McpDialog::view`]
//! calls [`server_list_view`].

use crate::mcp::{McpServerView, McpSource};

use super::row::{self, McpDialogView, McpRow, McpSpan, McpStyle, server_list_footer, status_span};

/// The SERVER_LIST step (qwen `ServerListStep` + the shared header): the
/// "Manage MCP servers" title + "N server(s)" count, then the servers grouped
/// by source with a `❯` cursor, status glyph + word, and an "N invalid tools"
/// warning; the empty/loading states; the footer.
pub(super) fn server_list_view(
    servers: &[McpServerView],
    loading: bool,
    active: usize,
) -> McpDialogView {
    let header = vec![
        McpRow::bold_styled(McpStyle::Accent, "Manage MCP servers"),
        McpRow::styled(McpStyle::Secondary, row::server_count_line(servers.len())),
    ];
    let content = if loading {
        vec![McpRow::styled(McpStyle::Secondary, "Loading…")]
    } else if servers.is_empty() {
        vec![
            McpRow::styled(McpStyle::Secondary, "No MCP servers configured."),
            McpRow::styled(
                McpStyle::Secondary,
                "Add MCP servers to your settings to get started.",
            ),
        ]
    } else {
        server_rows(servers, active)
    };
    McpDialogView {
        header,
        content,
        footer: server_list_footer(servers.is_empty()),
    }
}

/// The SERVER_LIST content rows (qwen `ServerListStep`): the servers grouped by
/// source under a bold `  User MCPs` / `  Project MCPs` / `  Extension MCPs`
/// heading, each server a `❯`/` ` cursor + name + ` · ` + status glyph & word (or
/// `disabled`), plus a trailing "N invalid tools" warning. A blank row separates
/// groups. `active` is the FLAT index; the row it lands on shows the accent `❯`.
fn server_rows(servers: &[McpServerView], active: usize) -> Vec<McpRow> {
    let mut rows = Vec::new();
    let mut flat = 0;
    for (group_index, (source, group)) in grouped_servers(servers).into_iter().enumerate() {
        if group_index > 0 {
            rows.push(McpRow::blank());
        }
        rows.push(McpRow::bold_styled(
            McpStyle::Primary,
            group_heading(source),
        ));
        for view in group {
            rows.push(server_row(view, flat == active));
            flat += 1;
        }
    }
    rows
}

/// One SERVER_LIST server row (qwen's per-server `Box`): the `❯`/` ` cursor, the
/// name, a ` · ` separator, the status glyph + word (`disabled` in yellow when
/// disabled), and the "N invalid tools" warning when any tool is invalid.
fn server_row(view: &McpServerView, selected: bool) -> McpRow {
    let cursor_style = if selected {
        McpStyle::Accent
    } else {
        McpStyle::Primary
    };
    let mut spans = vec![
        McpSpan::new(cursor_style, if selected { "❯ " } else { "  " }),
        McpSpan::new(cursor_style, view.name.clone()),
        McpSpan::new(McpStyle::Secondary, " · "),
        status_span(view),
    ];
    let invalid = view.invalid_tool_count();
    if invalid > 0 {
        spans.push(McpSpan::new(
            McpStyle::Warning,
            format!(" {invalid} invalid tools"),
        ));
    }
    McpRow::new(spans)
}

/// The bold source heading (qwen `getSourceDisplayName`, indented two spaces):
/// `  User MCPs` / `  Project MCPs` / `  Extension MCPs`.
fn group_heading(source: McpSource) -> String {
    match source {
        McpSource::User => "  User MCPs",
        McpSource::Workspace => "  Project MCPs",
        McpSource::Extension => "  Extension MCPs",
    }
    .to_string()
}

/// Groups the flat server list by source in qwen's `SOURCE_ORDER`
/// (user > project > extension), keeping each server's original order within its
/// group. A source with no servers is omitted (qwen skips empty groups). The flat
/// index the cursor uses walks these groups in the same order.
fn grouped_servers(servers: &[McpServerView]) -> Vec<(McpSource, Vec<&McpServerView>)> {
    const ORDER: [McpSource; 3] = [McpSource::User, McpSource::Workspace, McpSource::Extension];
    ORDER
        .into_iter()
        .filter_map(|source| {
            let group: Vec<&McpServerView> =
                servers.iter().filter(|s| s.source == source).collect();
            (!group.is_empty()).then_some((source, group))
        })
        .collect()
}
