//! The SERVER_DETAIL step render + its action model (ADR-0065 Phase E, qwen's
//! `ServerDetailStep`): one server's key/value detail (Status/Source/Command/…)
//! and the conditionally-shown action radio. [`detail_actions`] is the shared
//! source of truth for which actions a server offers - the nav fold reads it to
//! resolve a picked row, and the render walks it into rows. Pure over the
//! [`McpServerView`] read model (ADR-0001/0019).

use crate::mcp::{McpServerStatus, McpServerView, McpSource};

use super::McpAction;
use super::row::{McpDialogView, McpRow, McpSpan, McpStyle, nav_back_footer, status_span};

/// The label-column width the SERVER_DETAIL key/value lines pad to (qwen
/// `LABEL_WIDTH`): `Status:` / `Source:` / `Command:` etc. sit left-aligned in a
/// fixed column so their values line up.
const LABEL_WIDTH: usize = 15;

/// The SERVER_DETAIL step (qwen `ServerDetailStep`): the server name header, the
/// Status/Source/Command/Working Directory/Tools/Error key-value lines, then the
/// conditional action radio.
pub(super) fn server_detail_view(view: &McpServerView, active: usize) -> McpDialogView {
    let header = vec![McpRow::bold_styled(McpStyle::Accent, view.name.clone())];
    let mut content = detail_lines(view);
    content.push(McpRow::blank());
    content.extend(action_rows(&detail_actions(view), active));
    McpDialogView {
        header,
        content,
        footer: nav_back_footer(),
    }
}

/// The SERVER_DETAIL action list, shown conditionally EXACTLY as qwen's
/// `ServerDetailStep` builds it (ADR-0065's faithfulness list):
/// - `View tools` only when NOT disabled and the server has tools.
/// - `Reconnect` only when NOT disabled and the status is disconnected (failed).
/// - `Disable`/`Enable` ALWAYS (the label flips on `is_disabled`).
/// - `Authenticate`/`Re-authenticate` only when NOT disabled (the label flips on
///   `has_oauth_tokens`) - carried as [`McpAction::Authenticate`] either way.
/// - `Clear Authentication` only when NOT disabled AND tokens exist.
///
/// qwen's `DISABLE_SCOPE_SELECT` is omitted (unreachable in v0.16.0): Disable
/// dispatches `mcp_set_enabled(name, false)` directly. The nav fold
/// ([`super::McpDialog::pick_action`]) reads this to resolve a picked row.
pub(super) fn detail_actions(view: &McpServerView) -> Vec<McpAction> {
    let mut actions = Vec::new();
    if !view.is_disabled && view.tool_count() > 0 {
        actions.push(McpAction::ViewTools);
    }
    if !view.is_disabled && view.status == McpServerStatus::Disconnected {
        actions.push(McpAction::Reconnect);
    }
    actions.push(if view.is_disabled {
        McpAction::Enable
    } else {
        McpAction::Disable
    });
    if !view.is_disabled {
        actions.push(McpAction::Authenticate);
    }
    if !view.is_disabled && view.has_oauth_tokens {
        actions.push(McpAction::ClearAuth);
    }
    actions
}

/// The label a SERVER_DETAIL action row shows (qwen's action `label`s): the
/// Authenticate label reads `Re-authenticate` when tokens exist, matching qwen's
/// `hasOAuthTokens ? 'Re-authenticate' : 'Authenticate'`.
fn action_label(action: McpAction, has_oauth_tokens: bool) -> &'static str {
    match action {
        McpAction::ViewTools => "View tools",
        McpAction::Reconnect => "Reconnect",
        McpAction::Disable => "Disable",
        McpAction::Enable => "Enable",
        McpAction::Authenticate => {
            if has_oauth_tokens {
                "Re-authenticate"
            } else {
                "Authenticate"
            }
        }
        McpAction::ClearAuth => "Clear Authentication",
    }
}

/// The SERVER_DETAIL action rows (qwen `RadioButtonSelect`): a `❯`/` ` cursor +
/// the action label; the active row reads accent, the rest primary.
fn action_rows(actions: &[McpAction], active: usize) -> Vec<McpRow> {
    let has_oauth = actions.contains(&McpAction::ClearAuth);
    actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let selected = i == active;
            let style = if selected {
                McpStyle::Accent
            } else {
                McpStyle::Primary
            };
            McpRow::new(vec![
                McpSpan::new(style, if selected { "❯ " } else { "  " }),
                McpSpan::new(style, action_label(*action, has_oauth)),
            ])
        })
        .collect()
}

/// The SERVER_DETAIL key/value lines (qwen `ServerDetailStep`), each a
/// fixed-width label + value: Status, Source, Command, Working Directory (when
/// set), Tools (when NOT disabled), Error (when set). The Status value carries
/// the status colour; the Error value reads red.
fn detail_lines(view: &McpServerView) -> Vec<McpRow> {
    let mut rows = vec![
        McpRow::new(vec![label_span("Status:"), status_span(view)]),
        McpRow::new(vec![label_span("Source:"), source_span(view.source)]),
        McpRow::new(vec![
            label_span("Command:"),
            McpSpan::new(McpStyle::Primary, view.transport_display.clone()),
        ]),
    ];
    if let Some(cwd) = &view.cwd {
        rows.push(McpRow::new(vec![
            label_span("Working Directory:"),
            McpSpan::new(McpStyle::Primary, cwd.clone()),
        ]));
    }
    if !view.is_disabled {
        rows.push(McpRow::new(tools_line_spans(view)));
    }
    if let Some(error) = &view.error {
        rows.push(McpRow::new(vec![
            McpSpan::new(McpStyle::Error, pad_label("Error:")),
            McpSpan::new(McpStyle::Error, error.clone()),
        ]));
    }
    rows
}

/// The `Tools:` line spans (qwen's Tools row): `N tool(s)` plus, when any tool is
/// invalid, a yellow `(M invalid)` suffix.
fn tools_line_spans(view: &McpServerView) -> Vec<McpSpan> {
    let count = view.tool_count();
    let mut spans = vec![
        label_span("Tools:"),
        McpSpan::new(
            McpStyle::Primary,
            format!("{count} {}", super::row::plural(count, "tool", "tools")),
        ),
    ];
    let invalid = view.invalid_tool_count();
    if invalid > 0 {
        spans.push(McpSpan::new(
            McpStyle::Warning,
            format!(" ({invalid} invalid)"),
        ));
    }
    spans
}

/// A fixed-width SERVER_DETAIL label span (qwen's `LABEL_WIDTH` column), primary.
fn label_span(label: &str) -> McpSpan {
    McpSpan::new(McpStyle::Primary, pad_label(label))
}

/// Pads a detail label to the fixed label column width (qwen's `Box width`), so
/// the values line up. A label wider than the column is left as-is (qwen's Box
/// would wrap; we never truncate a label).
fn pad_label(label: &str) -> String {
    let width = label.chars().count();
    if width >= LABEL_WIDTH {
        format!("{label} ")
    } else {
        format!("{label}{}", " ".repeat(LABEL_WIDTH - width))
    }
}

/// The `Source:` value (qwen's source ternary): `User Settings` / `Workspace
/// Settings` / `Extension`.
fn source_span(source: McpSource) -> McpSpan {
    let text = match source {
        McpSource::User => "User Settings",
        McpSource::Workspace => "Workspace Settings",
        McpSource::Extension => "Extension",
    };
    McpSpan::new(McpStyle::Primary, text)
}

#[cfg(test)]
#[path = "../../../tests/ui/mcp_command/server_detail.rs"]
mod tests;
