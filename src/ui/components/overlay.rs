//! The body-region overlays (qwen `Help` + `MCPManagementDialog`): the two
//! bordered boxes that take the whole pending-body slot, sharing the anchored
//! draw ([`render_anchored_overlay`]) and the box framing. Split from the
//! components god module by rendering responsibility; shared primitives arrive
//! via `use super::*`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::ui::mcp_command::{McpDialogView, McpRow, McpStyle};
use crate::ui::slash;
use crate::ui::theme::Theme;

use super::CONTENT_MARGIN;
use super::box_draw::frame_box;
use super::pending::{anchor_clip, draw_overflow_marker};
use super::style::{
    StyleFn, accent_style, border_style, error_style, primary_style, secondary_style,
    success_style, warning_style,
};
use super::text::{push_cols, truncate_cols};

// ---------------------------------------------------------------------------
// The keyboard-shortcuts Help overlay (qwen `Help.tsx`, the `?` affordance the
// footer's `? for shortcuts` promises). A single bordered panel - suspenders has
// only two built-in commands and none of qwen's custom-command/MCP/skill/plugin
// ecosystem, so qwen's THREE-tab chrome (general / commands / custom-commands)
// would be two empty tabs of vaporware; we port the CONTENT (shortcuts + the
// built-in COMMANDS registry) into one panel and drop the tab chrome. The
// Screen's `help_open` flag gates it and [`Screen::handle_help_key`] closes it.
// ---------------------------------------------------------------------------

/// The width of the accent key column in a shortcut row (qwen `KEY_COL_WIDTH`,
/// Help.tsx:42): the fixed-width column the key sits in before its description.
pub(super) const HELP_KEY_COL_WIDTH: usize = 12;

/// The gap (columns) between the two shortcut columns (qwen `GeneralHelp` `gap:2`).
pub(super) const HELP_COL_GAP: usize = 2;

/// The (key, description) shortcut rows the Help panel lists, verified against
/// `ui.rs` `map_key` + `screen.rs` routing. `@` is already promised by the
/// composer placeholder (the AT-completion phase wires its behaviour), so it is
/// listed here alongside the live bindings.
pub(super) const HELP_SHORTCUTS: &[(&str, &str)] = &[
    ("/", "Open the command menu"),
    ("@", "Add files or folders as context"),
    ("?", "Show this help"),
    ("Enter", "Submit (steer a running turn)"),
    ("Alt+Enter", "Insert a newline"),
    ("Esc", "Cancel a running turn / close a menu"),
    ("Ctrl+O", "Toggle compact mode"),
    ("Ctrl+S", "Peek the full pending output into scrollback"),
    ("Ctrl+C", "Quit"),
    ("Shift+Tab", "Cycle approval mode"),
    ("Tab", "Accept the highlighted suggestion"),
    ("↑/↓", "Cycle prompt history / move the cursor"),
];

/// Draws the Help overlay (qwen `Help`) into `area`: the bordered shortcuts +
/// commands panel, top-clipped to the zone if it is taller than the body. The
/// panel is built once by the pure [`help_panel_lines`] (measure==draw), then
/// bottom-anchored so its footer (`Esc to close`) sits just above the composer -
/// the same anchor the pending body uses (ADR-0046).
pub(super) fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme) {
    render_anchored_overlay(frame, area, theme, help_panel_lines);
}

/// Draws a body-region overlay whose lines are pre-built by `build` at the content
/// width: the Help panel and the `/mcp` dialog share this exact draw - inset the
/// [`CONTENT_MARGIN`] content column, bottom-anchor + top-clip the box (qwen's
/// `overflowDirection:"top"`, [`anchor_clip`]) so its footer meets the composer,
/// blit the scrolled Paragraph, then the `… Ctrl-S` overflow marker when it
/// overflowed. The ONE anchored-overlay body so the two draws don't duplicate the
/// inset/anchor/clip/marker sequence.
pub(super) fn render_anchored_overlay(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    build: impl FnOnce(u16, &Theme) -> Vec<Line<'static>>,
) {
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: area.width.saturating_sub(2 * CONTENT_MARGIN),
        ..area
    };
    let lines = build(content_area.width, theme);
    let clip = anchor_clip(lines.len(), area, content_area);
    frame.render_widget(
        Paragraph::new(lines).scroll((clip.scroll, 0)),
        clip.content_draw,
    );
    if let Some(marker_draw) = clip.marker_draw {
        draw_overflow_marker(frame, marker_draw, theme);
    }
}

/// The Help panel's lines (qwen `Help`), framed with a single-line border to the
/// `inner` inner width (the same box-drawing the header panel uses): a title row
/// (`suspenders` bold accent + ` keyboard shortcuts`), a `Shortcuts` heading + the
/// shortcut rows (two columns when the width allows, one otherwise), a `Commands`
/// heading + the built-in [`slash::COMMANDS`] (derived, so a future command shows
/// up automatically), and an italic `Esc to close` footer. Every row is exactly
/// `inner + 2` columns (measure==draw, ADR-0029) so the viewport never re-breaks it.
pub(super) fn help_panel_lines(content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    // The panel takes the full content width minus the 2-col box chrome (`│…│`),
    // floored so a tiny terminal still draws a legible sliver.
    let inner = (content_width as usize).saturating_sub(2).max(1);
    let border = border_style(theme);
    frame_box(&help_panel_body_rows(inner, theme), inner, border)
}

/// The Help panel's borderless content rows (qwen `GeneralHelp` + `CommandsHelp`),
/// each clipped to `inner` columns - [`help_panel_lines`] wraps them in the box.
/// The order: title, blank, `Shortcuts` heading, the shortcut rows, blank,
/// `Commands` heading, the built-in command rows, blank, the `Esc to close` footer.
pub(super) fn help_panel_body_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    // Title row: `suspenders` bold accent + ` keyboard shortcuts` primary (qwen's
    // `Qwen Code` help header).
    {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let used = push_cols(
            &mut spans,
            "suspenders",
            accent_style(theme).add_modifier(Modifier::BOLD),
            0,
            inner,
        );
        push_cols(
            &mut spans,
            " keyboard shortcuts",
            primary_style(theme),
            used,
            inner,
        );
        out.push(Line::from(spans));
    }
    out.push(Line::default());

    // Shortcuts section.
    out.push(help_heading_row("Shortcuts", inner, theme));
    out.extend(help_shortcut_rows(inner, theme));
    out.push(Line::default());

    // Commands section, derived from the registry so a future command appears
    // without touching this panel.
    out.push(help_heading_row("Commands", inner, theme));
    for cmd in slash::COMMANDS {
        out.push(help_command_row(cmd, inner, theme));
    }
    out.push(Line::default());

    // Footer: `Esc to close`, italic secondary (qwen's `Esc to cancel`).
    out.push(Line::from(Span::styled(
        truncate_cols("Esc to close", inner),
        secondary_style(theme).add_modifier(Modifier::ITALIC),
    )));

    out
}

/// A section heading row (qwen `Text bold`): the label bold primary, clipped to
/// `inner`.
pub(super) fn help_heading_row(label: &str, inner: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        truncate_cols(label, inner),
        primary_style(theme).add_modifier(Modifier::BOLD),
    ))
}

/// The widest single shortcut cell that fits every entry WITHOUT truncation: the
/// fixed key column plus the longest description in [`HELP_SHORTCUTS`]. The
/// two-column layout only engages when the inner width holds two of these side by
/// side (plus the gap), so neither column ever chops a description - suspenders'
/// descriptions are longer than qwen's, so a single clean column is the norm.
pub(super) fn help_full_cell_width() -> usize {
    let longest_desc = HELP_SHORTCUTS
        .iter()
        .map(|(_, desc)| desc.width())
        .max()
        .unwrap_or(0);
    HELP_KEY_COL_WIDTH + longest_desc
}

/// The shortcut rows (qwen `GeneralHelp`), DEFAULTING TO ONE CLEAN COLUMN:
/// suspenders' descriptions are longer than qwen's, so a single column reads
/// better and never truncates, and an overlay has the vertical room for ~12 rows.
/// Two columns engage ONLY at genuinely wide widths where BOTH columns' FULL
/// descriptions fit without truncation (`inner >= 2*(key_col + longest_desc) +
/// gap`, ~114 cols given the ~44-col longest description) - so the two-column
/// branch is rarely hit. In it, the left cell is padded to an exact fixed width so
/// the right column aligns vertically; if a description must ever clip it does so
/// with one trailing ellipsis (never a hard mid-word cut).
pub(super) fn help_shortcut_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let full_cell = help_full_cell_width();
    let two_col = inner >= full_cell * 2 + HELP_COL_GAP;
    if two_col {
        // Both columns get the full untruncated cell width; the leftover inner
        // padding rides in the gap so the left cell stays a fixed column.
        let col_width = full_cell;
        let half = HELP_SHORTCUTS.len().div_ceil(2);
        let (left, right) = HELP_SHORTCUTS.split_at(half);
        (0..half)
            .map(|i| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                match right.get(i) {
                    // A left cell WITH a right column: pad the left cell out to the
                    // fixed width so the right column aligns vertically.
                    Some(row) => {
                        help_shortcut_cell(&mut spans, left[i], col_width, true, theme);
                        spans.push(Span::raw(" ".repeat(HELP_COL_GAP)));
                        help_shortcut_cell(&mut spans, *row, col_width, false, theme);
                    }
                    // The shorter (right) half leaves later rows single-column.
                    None => help_shortcut_cell(&mut spans, left[i], col_width, false, theme),
                }
                Line::from(spans)
            })
            .collect()
    } else {
        HELP_SHORTCUTS
            .iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                help_shortcut_cell(&mut spans, *row, inner, false, theme);
                Line::from(spans)
            })
            .collect()
    }
}

/// One shortcut cell up to `cell_width` columns: the accent key padded to the
/// fixed [`HELP_KEY_COL_WIDTH`], then the secondary description. The key column is
/// always padded (so descriptions line up). `pad_trailing` pads the WHOLE cell out
/// to `cell_width` - set only for a LEFT cell that a right column must align after;
/// a single/last cell leaves no trailing filler. A description that overflows the
/// cell is clipped with ONE trailing ellipsis ([`truncate_cols`]), never a hard
/// mid-word cut.
pub(super) fn help_shortcut_cell(
    spans: &mut Vec<Span<'static>>,
    (key, desc): (&str, &str),
    cell_width: usize,
    pad_trailing: bool,
    theme: &Theme,
) {
    let key_col = HELP_KEY_COL_WIDTH.min(cell_width);
    // The accent key, clipped to and padded out to the fixed key column.
    let key_text = truncate_cols(key, key_col);
    let key_pad = key_col.saturating_sub(key_text.width());
    spans.push(Span::styled(key_text, accent_style(theme)));
    if key_pad > 0 {
        spans.push(Span::raw(" ".repeat(key_pad)));
    }
    // The description fills the rest of the cell, ellipsis-clipped if it must.
    let desc_room = cell_width.saturating_sub(key_col);
    let desc_text = truncate_cols(desc, desc_room);
    let desc_width = desc_text.width();
    spans.push(Span::styled(desc_text, secondary_style(theme)));
    // A left cell pads out to the full cell so the right column starts on grid.
    if pad_trailing {
        let pad = desc_room.saturating_sub(desc_width);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
    }
}

/// One built-in command row (qwen `CommandsHelp` signature + description): the
/// accent `/name`, then a secondary ` — help` on the same row, clipped to `inner`.
pub(super) fn help_command_row(cmd: &slash::SlashCommand, inner: usize, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let key_end = HELP_KEY_COL_WIDTH.min(inner);
    let mut used = push_cols(
        &mut spans,
        &format!("/{}", cmd.name),
        accent_style(theme),
        0,
        key_end,
    );
    if used < key_end {
        spans.push(Span::raw(" ".repeat(key_end - used)));
        used = key_end;
    }
    push_cols(&mut spans, cmd.help, secondary_style(theme), used, inner);
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// The `/mcp` management dialog overlay (qwen `MCPManagementDialog`, ADR-0065
// Phase E). A single bordered box drawn in the BODY region (like the Help
// overlay), NOT the compact composer popup: a navigation-stack wizard whose
// active step's header / content / footer the pure [`crate::ui::mcp_command`]
// builds as styled [`McpRow`]s. This adapter only maps each row's semantic
// [`McpStyle`] to the active Theme and frames the rows in the box.
// ---------------------------------------------------------------------------

/// Draws the `/mcp` dialog (qwen `MCPManagementDialog`) into `area`: the active
/// step's bordered box (header, a blank, the content, a blank, the footer),
/// bottom-anchored and top-clipped exactly like the Help overlay + pending body
/// (ADR-0046) so its footer meets the composer. The box lines are built once by
/// the pure [`mcp_dialog_lines`] (measure==draw), so the viewport never
/// re-breaks a row.
pub(super) fn render_mcp_dialog(frame: &mut Frame, area: Rect, dialog: &McpDialogView, theme: &Theme) {
    render_anchored_overlay(frame, area, theme, |width, theme| {
        mcp_dialog_lines(dialog, width, theme)
    });
}

/// The `/mcp` dialog's box lines (qwen `MCPManagementDialog`'s single-border
/// box): the header rows, a blank (qwen's `gap:1`), the content rows, a blank,
/// then the footer, framed with the same single-line border the header/Help
/// panels use. Every row is exactly `inner + 2` columns (measure==draw,
/// ADR-0029). Pure over the [`McpDialogView`] + width; the adapter maps each
/// [`McpStyle`] to a Theme slot.
pub(super) fn mcp_dialog_lines(
    dialog: &McpDialogView,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner = (content_width as usize).saturating_sub(2).max(1);
    let border = border_style(theme);

    let mut body: Vec<Line<'static>> = Vec::new();
    body.extend(dialog.header.iter().map(|r| mcp_row_line(r, theme)));
    body.push(Line::default());
    body.extend(dialog.content.iter().map(|r| mcp_row_line(r, theme)));
    body.push(Line::default());
    body.push(mcp_row_line(&dialog.footer, theme));

    frame_box(&body, inner, border)
}

/// One rendered [`McpRow`] as a borderless [`Line`]: each [`McpSpan`] mapped
/// from its semantic [`McpStyle`] to the active Theme, plus [`Modifier::BOLD`]
/// when the span asserts `bold` (qwen's orthogonal `<Text bold>` emphasis on the
/// header titles, group headings, and TOOL_DETAIL labels). [`mcp_dialog_lines`]
/// wraps the line in the box (so no border chars here).
pub(super) fn mcp_row_line(row: &McpRow, theme: &Theme) -> Line<'static> {
    Line::from(
        row.spans
            .iter()
            .map(|span| {
                let mut style = mcp_span_style(span.style, theme);
                if span.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Span::styled(span.text.clone(), style)
            })
            .collect::<Vec<_>>(),
    )
}

/// The `McpStyle` → style-builder table (qwen's `semantic-colors` reads): each
/// semantic role paired with the SAME `*_style` slot the rest of the UI reads,
/// as data rather than a hand-written match arm ([`mcp_span_style`] looks up
/// here). Extending the palette is a row, not another branch (BP-006).
pub(super) const MCP_SPAN_STYLES: [(McpStyle, StyleFn); 6] = [
    (McpStyle::Accent, accent_style),
    (McpStyle::Primary, primary_style),
    (McpStyle::Secondary, secondary_style),
    (McpStyle::Success, success_style),
    (McpStyle::Warning, warning_style),
    (McpStyle::Error, error_style),
];

/// The Theme [`Style`] a semantic [`McpStyle`] maps to (qwen's `semantic-colors`
/// reads): the accent/primary/secondary body tones and the success/warning/error
/// status colours, drawn from the same slots the rest of the UI reads. A
/// table-driven lookup over [`MCP_SPAN_STYLES`] (the exhaustive palette), falling
/// back to `primary` defensively (unreachable: the table lists every variant).
pub(super) fn mcp_span_style(style: McpStyle, theme: &Theme) -> Style {
    MCP_SPAN_STYLES
        .iter()
        .find(|(role, _)| *role == style)
        .map(|(_, build)| build(theme))
        .unwrap_or_else(|| primary_style(theme))
}
