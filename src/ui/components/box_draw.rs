use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::push_cols;

/// The rounded top border row `╭───╮` at `inner` interior width. Leaf.
pub(super) fn box_top(inner: usize, border: Style) -> Line<'static> {
    Line::styled(format!("╭{}╮", "─".repeat(inner)), border)
}

/// The rounded bottom border row `╰───╯` at `inner` interior width. Leaf.
pub(super) fn box_bottom(inner: usize, border: Style) -> Line<'static> {
    Line::styled(format!("╰{}╯", "─".repeat(inner)), border)
}

/// One boxed content row: the `│` left border, the row's spans (truncated so the
/// row never exceeds the inner width), a pad to exactly `inner` columns, then the
/// `│` right border. The rigidity workhorse - every boxed row is exactly
/// `inner + 2` columns so the right border always aligns (ADR-0029). qwen adds a
/// `paddingX:1` inside the border; that pad is the first/last inner column here.
pub(super) fn box_row(spans: &[Span<'static>], inner: usize, border: Style) -> Line<'static> {
    let mut out = vec![Span::styled("│", border)];
    // paddingX:1 left.
    let mut used = 0;
    used = push_cols(&mut out, " ", Style::default(), used, inner);
    for span in spans {
        used = push_cols(&mut out, &span.content, span.style, used, inner);
    }
    if used < inner {
        out.push(Span::raw(" ".repeat(inner - used)));
    }
    out.push(Span::styled("│", border));
    Line::from(out)
}
