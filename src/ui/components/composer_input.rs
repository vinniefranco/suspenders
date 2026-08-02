//! The Composer input (ADR-0048, qwen InputPrompt): the top dash rule, the draft rows, the bottom border, and the terminal-cursor placement.
//!
//! Split from the components god module; shared primitives arrive via
//! `use super::*`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::composer::{self, ComposerLayout};
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;

use super::style::{accent_style, border_style, secondary_style, tui_color};

/// The two-row chrome the Composer wears above and below its draft (ADR-0048,
/// qwen `InputPrompt`): a top dash rule and a bottom single-border row. The
/// composer zone is grown by exactly this so the draft never loses a row to the
/// chrome, and the cursor y-offset accounts for the top rule (the correctness-
/// critical +1 unit-tested by `composer_cursor_sits_below_the_top_rule`).
pub(super) const COMPOSER_CHROME_ROWS: usize = 2;

/// The Composer placeholder shown when the draft is empty (qwen `InputPrompt`):
/// TWO leading spaces then the hint. The first glyph draws `REVERSED` (a resting
/// block cursor), the rest secondary.
pub(super) const COMPOSER_PLACEHOLDER: &str = "  Type your message or @path/to/file";

/// The Composer's border colour (qwen `borderColor`): `border.focused`
/// (link-blue) when the Composer owns the keyboard, else `border.default`
/// (grey). The Composer is unfocused exactly while the Approval modal holds the
/// keyboard (Phase-4 seam: mode variants of the prompt come later).
pub(super) fn composer_border_style(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(tui_color(theme.link))
    } else {
        border_style(theme)
    }
}

/// The Composer: a top dash rule, the draft, then a bottom border (ADR-0048,
/// qwen `InputPrompt`). The draft is pre-wrapped by the pure [`composer::layout`]
/// (char-based, so the cursor cell below is exact - `Paragraph`'s word-wrap
/// points can't be queried). The FIRST row wears the `> ` prompt in
/// `accent_style`; every continuation row - hard-newline and wrapped alike -
/// indents 2 spaces to align under it. An empty draft shows the placeholder.
///
/// When the draft is taller than the box, the Composer scrolls internally
/// ([`composer::first_visible_row`]) so the cursor row stays visible, near the
/// bottom like a terminal. The REAL terminal cursor is placed at the cursor's
/// cell (shifted DOWN one row by the top rule) - except while the Approval modal
/// owns the keyboard, when a blinking composer cursor would misstate where keys
/// go.
pub fn render_composer(
    frame: &mut Frame,
    area: Rect,
    t: &Screen,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    // Operation → Integration (IOSP): the pure [`composer_chrome`] carries the
    // fit decision (`None` = too small to draw), the border style, the bottom
    // rule Rect, and the terminal-cursor cell (`None` when the Approval owns
    // the keyboard); the slot below only issues the draw calls.
    render_composer_slot(frame, area, composer_chrome(area, t, theme), layout, theme);
}

/// The Composer's drawable chrome (the compute-plan behind [`render_composer`]):
/// the border style, the bottom-border Rect, and whether the terminal cursor is
/// parked (`focused` - false while the Approval modal owns the keyboard, when a
/// blinking composer cursor would misstate where keys go). Built by
/// [`composer_chrome`]; the cursor CELL is layout-dependent, computed at draw
/// time by [`composer_cursor`].
pub(super) struct ComposerChrome {
    border: Style,
    bottom: Rect,
    focused: bool,
}

/// Operation (IOSP): the Composer's chrome for `area`, or `None` when the zone
/// is too small to hold the two chrome rows plus a draft column (measure ==
/// draw, ADR-0029). Pure: no frame access. The fit and the `focused` decision
/// are made here so [`render_composer`] never branches.
pub(super) fn composer_chrome(area: Rect, t: &Screen, theme: &Theme) -> Option<ComposerChrome> {
    let fits = area.height as usize > COMPOSER_CHROME_ROWS && area.width >= 2;
    // The composer is focused when no modal holds the keyboard. A pending
    // question modal takes focus like an approval, EXCEPT while it is collecting
    // a free-form "Other" answer - then the composer is the interactive element
    // and stays focused (ADR-0057).
    let question_holds_focus = t
        .pending_question
        .as_ref()
        .is_some_and(|q| q.collecting_other.is_none());
    let focused = t.pending_approval.is_none() && !question_holds_focus;
    fits.then(|| ComposerChrome {
        border: composer_border_style(focused, theme),
        bottom: Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        },
        focused,
    })
}

/// The Composer slot: draws the body, the bottom rule, and (when focused) parks
/// the terminal cursor - but only when the plan says the zone fits (`Some`).
/// The presence + focus branches live HERE so [`render_composer`] is call-only
/// (IOSP).
pub fn render_composer_slot(
    frame: &mut Frame,
    area: Rect,
    chrome: Option<ComposerChrome>,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    if let Some(chrome) = chrome {
        draw_composer(frame, area, &chrome, layout, theme);
    }
}

/// Draws a fitted Composer (call-only assembler): the body Paragraph, the
/// bottom rule, and the terminal cursor when the chrome carries a cell.
pub(super) fn draw_composer(
    frame: &mut Frame,
    area: Rect,
    chrome: &ComposerChrome,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    let rule_width = area.width as usize;
    frame.render_widget(
        Paragraph::new(composer_body_lines(
            layout,
            area.height,
            rule_width,
            chrome.border,
            theme,
        )),
        area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("─".repeat(rule_width), chrome.border)),
        chrome.bottom,
    );
    place_composer_cursor(frame, chrome.focused, layout, area);
}

/// Parks the terminal cursor at the draft cell when the chrome is `focused`,
/// else leaves it (the Approval owns the keyboard). The focus branch lives HERE
/// (IOSP).
pub(super) fn place_composer_cursor(
    frame: &mut Frame,
    focused: bool,
    layout: &ComposerLayout,
    area: Rect,
) {
    if focused {
        frame.set_cursor_position(composer_cursor(layout, area));
    }
}

/// Operation (IOSP): the Composer's body lines - the top dash rule (qwen's
/// hand-drawn `─`×`rule_width`; the `top_right_label` seam is deferred, no
/// session-name concept yet) then the draft rows (the `> ` prompt on row 0,
/// 2-space indent on continuations) or the placeholder when empty. The bottom
/// border is a separate draw (a different rect), so it is not in this list.
/// `zone_height` is the full composer zone height; the draft window is it less
/// the two chrome rows. Pure.
pub(super) fn composer_body_lines(
    layout: &ComposerLayout,
    zone_height: u16,
    rule_width: usize,
    border: Style,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("─".repeat(rule_width), border)];
    if composer_is_empty(layout) {
        lines.push(composer_placeholder_line(theme));
        return lines;
    }
    let visible = zone_height as usize - COMPOSER_CHROME_ROWS;
    let top = composer::first_visible_row(layout.cursor_row, visible);
    let prompt = accent_style(theme).add_modifier(Modifier::BOLD);
    lines.extend(
        layout
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(i, row)| {
                let prefix = if i == 0 { "> " } else { "  " };
                Line::from(vec![Span::styled(prefix, prompt), Span::raw(row.clone())])
            }),
    );
    lines
}

/// Operation (IOSP): the real terminal cursor cell for the draft cursor - the
/// `> ` prompt shifts it right by [`PROMPT_GUTTER_COLS`], and the top dash rule
/// shifts it DOWN one row (the correctness-critical `+1`, Risk #1). `cursor_col <
/// width` by the layout contract, so the cell is always inside `area`. Pure.
pub(super) fn composer_cursor(layout: &ComposerLayout, area: Rect) -> (u16, u16) {
    let visible = area.height as usize - COMPOSER_CHROME_ROWS;
    let top = composer::first_visible_row(layout.cursor_row, visible);
    (
        area.x + PROMPT_GUTTER_COLS as u16 + layout.cursor_col as u16,
        area.y + 1 + (layout.cursor_row - top) as u16,
    )
}

/// The width of the `> ` prompt gutter every draft row hangs under.
pub(super) const PROMPT_GUTTER_COLS: usize = 2;

/// Whether the Composer draft is empty (one blank row, cursor at the origin) -
/// the placeholder condition. Pure over the layout.
pub(super) fn composer_is_empty(layout: &ComposerLayout) -> bool {
    layout.cursor_row == 0 && layout.cursor_col == 0 && layout.rows.iter().all(|r| r.is_empty())
}

/// The placeholder line (qwen `InputPrompt`): the two-space-lead hint in
/// secondary, its FIRST glyph `REVERSED` so a resting block cursor sits where
/// typing begins.
pub(super) fn composer_placeholder_line(theme: &Theme) -> Line<'static> {
    let secondary = secondary_style(theme);
    let mut chars = COMPOSER_PLACEHOLDER.chars();
    let first: String = chars.by_ref().take(1).collect();
    let rest: String = chars.collect();
    Line::from(vec![
        Span::styled(first, secondary.add_modifier(Modifier::REVERSED)),
        Span::styled(rest, secondary),
    ])
}
