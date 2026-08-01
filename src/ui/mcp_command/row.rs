//! The `/mcp` dialog's shared render vocabulary (ADR-0065 Phase E, ADR-0001/0019):
//! the semantic [`McpStyle`]/[`McpSpan`]/[`McpRow`]/[`McpDialogView`] data the
//! adapter maps to Theme slots, plus the small shared formatters (counts,
//! footers, the missing-selection guard) every step reuses. No ratatui/crossterm
//! here (the pure core, ADR-0019); the adapter ([`super::super::components`]) draws
//! the box.

use crate::mcp::{McpServerStatus, McpServerView};

/// The semantic role a rendered [`McpRow`] span plays, so the pure builders stay
/// ratatui/crossterm-free (ADR-0001/0019): the adapter maps each to a Theme slot
/// ([`super::super::components`]). Mirrors qwen's `semantic-colors` reads - `Accent` is
/// `text.accent` (the selected row + titles), `Primary`/`Secondary` the two body
/// tones, and `Success`/`Warning`/`Error` the status colours
/// (connected/disabled|connecting/failed) qwen's `getStatusColor` maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStyle {
    /// qwen `text.accent`: the selected/`❯` row, step titles.
    Accent,
    /// qwen `text.primary`: body text, key labels, server/tool names.
    Primary,
    /// qwen `text.secondary`: counts, separators, hints, annotation tags.
    Secondary,
    /// qwen `status.success` (green): a connected server's status glyph + word.
    Success,
    /// qwen `status.warning` (yellow): a disabled/connecting status, the invalid
    /// tool warning, and the invalid-tool annotation.
    Warning,
    /// qwen `status.error` (red): a failed server's status, the `Error:` line,
    /// and the invalid-tool detail warning.
    Error,
}

/// One styled run of text inside a rendered [`McpRow`] - the pure analog of a
/// ratatui `Span`. The adapter turns `(style, bold, text)` into a themed span.
/// `bold` is ORTHOGONAL to [`McpStyle`] (the colour axis): qwen renders several
/// dialog texts `bold` on top of a semantic colour ([`Modifier::BOLD`] in the
/// adapter), which the colour-only style cannot express - the header titles, the
/// SERVER_LIST group headings, and the TOOL_DETAIL `Description:`/`Parameters:`
/// labels + invalid warning. The default constructor is non-bold; [`McpSpan::bold`]
/// mints an emphasised one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSpan {
    pub style: McpStyle,
    pub text: String,
    /// Whether this run renders bold (qwen's `bold` prop). Orthogonal to `style`.
    pub bold: bool,
}

impl McpSpan {
    pub(super) fn new(style: McpStyle, text: impl Into<String>) -> Self {
        McpSpan {
            style,
            text: text.into(),
            bold: false,
        }
    }

    /// A bold run in `style` (qwen's `<Text bold>`): the emphasis is orthogonal
    /// to the colour, so any [`McpStyle`] can carry it.
    pub(super) fn bold(style: McpStyle, text: impl Into<String>) -> Self {
        McpSpan {
            style,
            text: text.into(),
            bold: true,
        }
    }
}

/// One rendered line of a dialog step - a header, content, or footer row - as a
/// run of styled spans. Plain data (no ratatui, ADR-0019); the adapter wraps
/// each in the bordered box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRow {
    pub spans: Vec<McpSpan>,
}

impl McpRow {
    /// A row from its styled spans.
    pub(super) fn new(spans: Vec<McpSpan>) -> Self {
        McpRow { spans }
    }

    /// A one-span row in `style`.
    pub(super) fn styled(style: McpStyle, text: impl Into<String>) -> Self {
        McpRow::new(vec![McpSpan::new(style, text)])
    }

    /// A one-span BOLD row in `style` (qwen's `<Text bold>` titles/labels).
    pub(super) fn bold_styled(style: McpStyle, text: impl Into<String>) -> Self {
        McpRow::new(vec![McpSpan::bold(style, text)])
    }

    /// A blank spacer row (qwen's `gap:1` between sections).
    pub(super) fn blank() -> Self {
        McpRow::new(Vec::new())
    }
}

/// The whole render surface of the active step (ADR-0065 Phase E): the box
/// `header`, the `content` rows, and the `footer` key hints, each a run of
/// [`McpRow`]s. The adapter draws them in a single bordered box (qwen
/// `MCPManagementDialog`'s header/content/footer). A pure value the Composer
/// exposes through [`super::super::composer::OverlayView::McpDialog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDialogView {
    pub header: Vec<McpRow>,
    pub content: Vec<McpRow>,
    pub footer: McpRow,
}

/// The status glyph + word span for a server (qwen's `{getStatusIcon} {status}`):
/// a disabled server reads its icon + `disabled` in yellow; else the icon + the
/// `.label()` word in the status colour (connected green / connecting yellow /
/// failed red). Shared by the SERVER_LIST row and the SERVER_DETAIL Status line.
pub(super) fn status_span(view: &McpServerView) -> McpSpan {
    if view.is_disabled {
        return McpSpan::new(
            McpStyle::Warning,
            format!("{} disabled", view.status.icon()),
        );
    }
    McpSpan::new(
        status_style(view.status),
        format!("{} {}", view.status.icon(), view.status.label()),
    )
}

/// qwen `getStatusColor`: connected -> success (green), connecting -> warning
/// (yellow), disconnected -> error (red).
fn status_style(status: McpServerStatus) -> McpStyle {
    match status {
        McpServerStatus::Connected => McpStyle::Success,
        McpServerStatus::Connecting => McpStyle::Warning,
        McpServerStatus::Disconnected => McpStyle::Error,
    }
}

/// The "N server(s)" count line (qwen's `{n} {n === 1 ? 'server' : 'servers'}`).
pub(super) fn server_count_line(count: usize) -> String {
    format!("{count} {}", plural(count, "server", "servers"))
}

/// The "(N tool(s))" count line (qwen's TOOL_LIST header count).
pub(super) fn tool_count_line(count: usize) -> String {
    format!("({count} {})", plural(count, "tool", "tools"))
}

/// English count-agreement: `one` at exactly 1, `many` otherwise (qwen's inline
/// ternary). Pure grammar, kept out of the render.
pub(super) fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// The SERVER_LIST footer (qwen `renderStepFooter` SERVER_LIST): a bare
/// `Esc to close` when there are no servers, else the full nav hint.
pub(super) fn server_list_footer(empty: bool) -> McpRow {
    let text = if empty {
        "Esc to close"
    } else {
        "↑↓ to navigate · Enter to select · Esc to close"
    };
    McpRow::styled(McpStyle::Secondary, text)
}

/// The SERVER_DETAIL / TOOL_LIST footer (qwen `renderStepFooter`): the
/// navigate/select/back hint.
pub(super) fn nav_back_footer() -> McpRow {
    McpRow::styled(
        McpStyle::Secondary,
        "↑↓ to navigate · Enter to select · Esc to back",
    )
}

/// The TOOL_DETAIL footer (qwen `renderStepFooter` TOOL_DETAIL): `Esc to back`.
pub(super) fn back_footer() -> McpRow {
    McpRow::styled(McpStyle::Secondary, "Esc to back")
}

/// The AUTHENTICATE footer (qwen `renderStepFooter` AUTHENTICATE): `Esc to go
/// back` (qwen's wording differs from TOOL_DETAIL's, ported verbatim).
pub(super) fn go_back_footer() -> McpRow {
    McpRow::styled(McpStyle::Secondary, "Esc to go back")
}

/// A one-line "missing selection" view (qwen's `No server selected` /
/// `No tool selected` guards). Defensive - the navigation never pushes a step
/// for an out-of-range index - but keeps the render total.
pub(super) fn missing_view(message: &str) -> McpDialogView {
    McpDialogView {
        header: vec![McpRow::bold_styled(McpStyle::Accent, "Manage MCP servers")],
        content: vec![McpRow::styled(McpStyle::Error, message.to_string())],
        footer: back_footer(),
    }
}

#[cfg(test)]
#[path = "../../../tests/ui/mcp_command/row.rs"]
mod tests;
