use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::ui::theme::Theme;

use super::style::tui_color;

/// The bottom-anchor + top-clip geometry a pending body draws at (ADR-0046),
/// resolved from the stack's `total_lines` against the zone `area`/`content_area`.
/// Every field is a ready-to-draw value, so [`render_pending_body_at`] holds no
/// layout arithmetic of its own (IOSP). `marker_draw` is `Some` only when the
/// stack overflows the zone.
pub(super) struct PendingClip {
    /// The stack's total wrapped rows, echoed back for the caller's return value.
    pub(super) total_lines: usize,
    /// Content Paragraph scroll offset (the top-clipped row count, saturated).
    pub(super) scroll: u16,
    pub(super) content_draw: Rect,
    /// The `… Ctrl-S to show more` marker row, present only on overflow.
    pub(super) marker_draw: Option<Rect>,
}

/// Operation (IOSP): the pure anchor/clip math for a pending body of
/// `total_lines` wrapped rows in a `content_area` inside the zone `area`. When the
/// stack overflows, keep the LAST `height` rows (drop from the top, qwen's
/// `overflowDirection:"top"`) and reserve the top row for the overflow marker; when
/// it fits, bottom-anchor it via `pad_top`. No frame access, no side effects.
///
/// Bottom-anchored (Stage 1 / help) - equivalent to [`scrolled_clip`] with a zero
/// scroll intent. Kept as the thin wrapper the help overlay draws through.
pub(super) fn anchor_clip(total_lines: usize, area: Rect, content_area: Rect) -> PendingClip {
    scrolled_clip(total_lines, area, content_area, ScrollIntent::FOLLOW)
}

/// The transcript's app-owned scroll INTENT (ADR-0046, Stage 2), passed from the
/// pure [`Screen`] to the render clamp: `follow_tail` pins to the bottom (the
/// Stage 1 default), else `lines` is how many wrapped rows the view is scrolled UP
/// from the bottom (`usize::MAX` = as far up as possible). Geometry-free - the
/// clamp turns it into a valid top-clip against the live viewport each frame.
#[derive(Clone, Copy)]
pub(super) struct ScrollIntent {
    pub(super) follow_tail: bool,
    pub(super) lines: usize,
}

impl ScrollIntent {
    /// The bottom-anchored, tail-following default (Stage 1 behavior).
    pub(super) const FOLLOW: ScrollIntent = ScrollIntent {
        follow_tail: true,
        lines: 0,
    };
}

/// Operation (IOSP): the anchor/clip math generalized to an app-owned scroll
/// INTENT (ADR-0046, Stage 2). Following the tail bottom-anchors exactly as Stage
/// 1 did; a detached `intent` lifts the window UP by `intent.lines`, CLAMPED here
/// to the valid range so the pure core stays geometry-free: `max_scroll =
/// total - height` (0 when the stack fits, so scroll is a no-op), and the effective
/// lift is `min(intent.lines, max_scroll)` - an over-scroll or `usize::MAX` (Home)
/// simply pins to the oldest row, and a grown terminal auto-re-clamps. No frame
/// access, no side effects.
pub(super) fn scrolled_clip(
    total_lines: usize,
    area: Rect,
    content_area: Rect,
    intent: ScrollIntent,
) -> PendingClip {
    let height = area.height as usize;
    let overflowed = total_lines > height;

    // The rows scrolled up from the bottom, clamped to what the stack allows.
    // `follow_tail` (or a stack that fits) means no lift.
    let max_scroll = total_lines.saturating_sub(height);
    let effective = if intent.follow_tail {
        0
    } else {
        intent.lines.min(max_scroll)
    };
    // The rows still hidden ABOVE the viewport once the lift is applied: the
    // overflow marker (`…`) shows only while some remain (so Home, fully scrolled
    // up, reveals the oldest row instead of hiding it under the marker).
    let clipped_above = max_scroll - effective;
    let has_marker = overflowed && clipped_above > 0;

    let (top, drawn_rows, pad_top) = if overflowed {
        // Bottom-origin top-clip lifted by `effective` (qwen's
        // `overflowDirection:"top"`): drop `clipped_above` rows from the top, plus
        // one more for the marker row when it shows.
        (clipped_above + has_marker as usize, height, 0)
    } else {
        (0, total_lines, height - total_lines)
    };

    // When the marker shows, the top visible row is it, so the content starts one
    // row down and loses that row of height.
    let content_top_pad: u16 = if has_marker { 1 } else { 0 };
    let draw_height = drawn_rows.saturating_sub(content_top_pad as usize) as u16;
    let y_off = pad_top as u16 + content_top_pad;

    PendingClip {
        total_lines,
        scroll: u16::try_from(top).unwrap_or(u16::MAX),
        content_draw: Rect {
            y: content_area.y + y_off,
            height: draw_height,
            ..content_area
        },
        marker_draw: has_marker.then_some(Rect {
            y: area.y + pad_top as u16,
            height: 1,
            ..area
        }),
    }
}

/// Draws the `…` overflow marker on the reserved top row when rows are clipped
/// ABOVE the viewport (ADR-0046): the "more above" affordance the app-owned scroll
/// (Stage 2) reveals - wheel/PageUp/Ctrl-S walk up into those rows, and the marker
/// clears once the view reaches the very top.
pub(super) fn draw_overflow_marker(frame: &mut Frame, area: Rect, theme: &Theme) {
    let marker_style = Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC);
    frame.render_widget(Paragraph::new(Line::styled("…", marker_style)), area);
}
