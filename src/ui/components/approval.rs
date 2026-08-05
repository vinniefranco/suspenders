use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::ui::screen::{ConfirmKind, PendingApproval};
use crate::ui::theme::Theme;

use super::style::{primary_style, secondary_style, success_style};
use super::text::push_cols;

/// The `›` U+203A active-row marker (qwen `BaseSelectionList`), success-green
/// when the row is active. It sits in a 2-wide gutter (the marker + a trailing
/// space, or two spaces when inactive).
pub(super) const SELECTION_MARKER: &str = "›";
/// The width of the selection gutter (marker + one space).
pub(super) const SELECTION_GUTTER_WIDTH: usize = 2;

/// The numbered radio rows of a [`SelectionList`] (ADR-0049, qwen
/// `BaseSelectionList.tsx`): each row is `‹gutter›N. label`, where the gutter
/// carries the `›` marker (success-green) on the active row else two spaces, the
/// `N.` number is right-aligned in a fixed field (`showNumbers`) and turns
/// success-green on the active row (with the marker + label) else secondary, and
/// the label reads success-green when active else primary. Every row is truncate-end at
/// `inner_width` so the box wrapper never re-breaks it (measure==draw, ADR-0029).
/// `active` is the highlighted 0-based row.
pub(super) fn selection_rows(
    items: &[&str],
    active: usize,
    show_numbers: bool,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = inner_width as usize;
    // The number field is as wide as the widest `N.` (e.g. `9.` = 2, `12.` = 3).
    let num_field = if show_numbers {
        format!("{}.", items.len()).width()
    } else {
        0
    };
    items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let is_active = i == active;
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0;
            // The 2-wide `›`/space gutter.
            if is_active {
                used = push_cols(
                    &mut spans,
                    SELECTION_MARKER,
                    success_style(theme),
                    used,
                    width,
                );
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            } else {
                used = push_cols(
                    &mut spans,
                    &" ".repeat(SELECTION_GUTTER_WIDTH),
                    Style::default(),
                    used,
                    width,
                );
            }
            // The right-aligned `N.` number field: qwen turns the number
            // `status.success` (green) together with the marker + label on the
            // active row (`BaseSelectionList.tsx:113-118`), else `text.secondary`.
            if show_numbers {
                let num = format!("{}.", i + 1);
                let pad = num_field.saturating_sub(num.width());
                if pad > 0 {
                    used = push_cols(&mut spans, &" ".repeat(pad), Style::default(), used, width);
                }
                let num_style = if is_active {
                    success_style(theme)
                } else {
                    secondary_style(theme)
                };
                used = push_cols(&mut spans, &num, num_style, used, width);
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            }
            // The label: success-green when active, else primary.
            let label_style = if is_active {
                success_style(theme)
            } else {
                primary_style(theme)
            };
            let _ = push_cols(&mut spans, label, label_style, used, width);
            Line::from(spans)
        })
        .collect()
}

/// The verbatim Approval options in order (ADR-0049, qwen exec/info sets): once /
/// always-in-project / no-suggest. The single `Always allow in this project`
/// (the qwen no-`{{action}}` fallback) collapses BOTH qwen always-variants onto
/// suspenders' one session-scoped ApproveAlways (ADR-0005). Row indices match
/// `screen::decision_for_option` (0 Approve / 1 ApproveAlways / 2 Deny).
const APPROVAL_OPTIONS: [&str; 3] = [
    "Yes, allow once",
    "Always allow in this project",
    "No, suggest changes (esc)",
];

/// The Approval question line (ADR-0049, qwen verbatim): `Exec` reads `Allow
/// execution of: '{command}'?`, `Info` reads `Do you want to proceed?`.
fn approval_question(kind: ConfirmKind, command: &str) -> String {
    match kind {
        ConfirmKind::Exec => format!("Allow execution of: '{command}'?"),
        ConfirmKind::Info => "Do you want to proceed?".to_string(),
    }
}

/// The inline approval block's inner rows (ADR-0049), appended after the
/// confirming ToolCall's header INSIDE its box: a blank gap row (qwen
/// `marginBottom:1`), the question line (`primary`, truncate-end), then the
/// numbered radio rows driven by the pending [`SelectionList`]. Every row is
/// `<= inner_width` columns (measure==draw, ADR-0029) so [`box_row`] never
/// re-breaks it.
pub(super) fn approval_block_rows(
    pending: &PendingApproval,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = inner_width as usize;
    let mut rows = vec![Line::from(Vec::<Span<'static>>::new())];
    let question = approval_question(pending.kind(), pending.command());
    let mut spans = Vec::new();
    let _ = push_cols(&mut spans, &question, primary_style(theme), 0, width);
    rows.push(Line::from(spans));
    rows.extend(selection_rows(
        &APPROVAL_OPTIONS,
        pending.active_row(),
        true,
        inner_width,
        theme,
    ));
    rows
}
