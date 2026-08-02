//! The rounded single-line box primitives (qwen `borderStyle:"round"`): the
//! corner/border rows and the column-rigid boxed content row every framed panel
//! shares - the header info panel, the Help overlay, the `/mcp` dialog, the
//! sticky "Current tasks" box, the question modal, and the tool-group box.
//!
//! Split from the components god module so the ONE box assembly (measure==draw,
//! ADR-0029) lives in a single place; shared column primitives arrive via
//! `use super::*`.

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

/// Frames borderless `body` content rows in a rounded single-line box at `inner`
/// interior width: the `╭───╮` top, each body row funneled through [`box_row`] to
/// exactly `inner + 2` columns, then the `╰───╯` bottom. The ONE box-assembly the
/// header/help/mcp/question/sticky/tool boxes share (measure==draw, ADR-0029) -
/// so the corner glyphs + row-padding live in a single place. Pure.
pub(super) fn frame_box(body: &[Line<'static>], inner: usize, border: Style) -> Vec<Line<'static>> {
    let mut rows = Vec::with_capacity(body.len() + 2);
    rows.push(box_top(inner, border));
    rows.extend(body.iter().map(|line| box_row(&line.spans, inner, border)));
    rows.push(box_bottom(inner, border));
    rows
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
