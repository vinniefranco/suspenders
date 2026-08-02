//! The TOOL_LIST step render (ADR-0065 Phase E, qwen's `ToolListStep`): a
//! server's scrolling tool list with per-tool annotation words / invalid reasons
//! and a `current/total` scroll indicator. One pure builder over the
//! [`McpServerView`] read model (ADR-0001/0019); [`super::McpDialog::view`] calls
//! [`tool_list_view`].

use crate::mcp::{McpServerView, McpToolView};

use super::VISIBLE_TOOLS_COUNT;
use super::row::{self, McpDialogView, McpRow, McpSpan, McpStyle, nav_back_footer};

/// The TOOL_LIST step (qwen `ToolListStep`): the "Tools for {server}" header +
/// "(N tool(s))" count, the scrolling tool rows with annotation words / invalid
/// reasons, and the `current/total` scroll indicator.
pub(super) fn tool_list_view(server: &McpServerView, active: usize) -> McpDialogView {
    let header = vec![
        McpRow::bold_styled(McpStyle::Accent, format!("Tools for {}", server.name)),
        McpRow::styled(
            McpStyle::Secondary,
            row::tool_count_line(server.tool_count()),
        ),
    ];
    let content = if server.tools.is_empty() {
        vec![McpRow::styled(
            McpStyle::Secondary,
            "No tools available for this server.",
        )]
    } else {
        tool_rows(&server.tools, active)
    };
    McpDialogView {
        header,
        content,
        footer: nav_back_footer(),
    }
}

/// The TOOL_LIST content rows (qwen `ToolListStep`): a SCROLL WINDOW of at most
/// [`VISIBLE_TOOLS_COUNT`] tools around the active row, each a `❯`/` ` cursor +
/// name + either its `invalid: <reason>` warning or its annotation words, then a
/// `current/total` scroll indicator when the list overflows the window.
fn tool_rows(tools: &[McpToolView], active: usize) -> Vec<McpRow> {
    let offset = scroll_offset(active, tools.len());
    let mut rows: Vec<McpRow> = tools
        .iter()
        .enumerate()
        .skip(offset)
        .take(VISIBLE_TOOLS_COUNT)
        .map(|(i, tool)| tool_row(tool, i == active))
        .collect();
    if tools.len() > VISIBLE_TOOLS_COUNT {
        rows.push(McpRow::blank());
        rows.push(scroll_indicator(offset, active, tools.len()));
    }
    rows
}

/// One TOOL_LIST tool row (qwen's per-tool `Box`): the `❯`/` ` cursor + name,
/// then either the yellow `invalid: <reason>` (invalid tool) or the secondary
/// annotation words (valid tool with annotations).
fn tool_row(tool: &McpToolView, selected: bool) -> McpRow {
    let style = if selected {
        McpStyle::Accent
    } else {
        McpStyle::Primary
    };
    let mut spans = vec![
        McpSpan::new(style, if selected { "❯ " } else { "  " }),
        McpSpan::new(style, tool.name.clone()),
    ];
    if !tool.is_valid() {
        spans.push(McpSpan::new(
            McpStyle::Warning,
            format!("  invalid: {}", tool.invalid_reasons().join(", ")),
        ));
    } else if let Some(tags) = annotation_words(tool) {
        spans.push(McpSpan::new(McpStyle::Secondary, format!("  {tags}")));
    }
    McpRow::new(spans)
}

/// The annotation WORDS a tool carries (qwen `getToolAnnotations`), in qwen's
/// order - destructive, read-only, open-world, idempotent - joined by `, `.
/// `None` when the tool asserts no hint (the row shows nothing).
fn annotation_words(tool: &McpToolView) -> Option<String> {
    let a = tool.annotations;
    if !a.any() {
        return None;
    }
    let mut words = Vec::new();
    if a.destructive {
        words.push("destructive");
    }
    if a.read_only {
        words.push("read-only");
    }
    if a.open_world {
        words.push("open-world");
    }
    if a.idempotent {
        words.push("idempotent");
    }
    Some(words.join(", "))
}

/// The TOOL_LIST scroll-window top (qwen `ToolListStep`'s `scrollOffset`): 0
/// until the active row would fall off the bottom of the window, then it tracks
/// the active row, clamped so the window never runs past the list end.
fn scroll_offset(active: usize, total: usize) -> usize {
    if total <= VISIBLE_TOOLS_COUNT || active < VISIBLE_TOOLS_COUNT - 1 {
        return 0;
    }
    (active + 1 - VISIBLE_TOOLS_COUNT).min(total - VISIBLE_TOOLS_COUNT)
}

/// The TOOL_LIST scroll indicator (qwen's `↑ current/total ↓`): a leading `↑`
/// when rows are hidden above, the 1-based `active/total`, and a trailing `↓`
/// when rows are hidden below.
fn scroll_indicator(offset: usize, active: usize, total: usize) -> McpRow {
    let up = if offset > 0 { "↑ " } else { "  " };
    let down = if offset + VISIBLE_TOOLS_COUNT < total {
        " ↓"
    } else {
        ""
    };
    McpRow::styled(
        McpStyle::Secondary,
        format!("{up}{}/{total}{down}", active + 1),
    )
}

#[cfg(test)]
#[path = "../../../tests/ui/mcp_command/tool_list.rs"]
mod tests;
