use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::ui::completion;
use crate::ui::composer::{self, OverlayStatus, OverlayView};
use crate::ui::mcp_command::{McpDialogView, McpRow, McpStyle};
use crate::ui::slash;
use crate::ui::theme::Theme;
use crate::view_model::{RowRole, SelectorRow};

use super::approval::{SELECTION_GUTTER_WIDTH, SELECTION_MARKER};
use super::picker::padded;
use super::style::{
    accent_style, error_style, primary_style, secondary_style, success_style, tui_color,
    warning_style,
};
use super::suggestion::suggestion_rows;
use super::text::push_cols;

/// Computes the bounding rect for the Composer overlay popup: body rows plus
/// top/bottom border, capped at `body_cap` so a long list never eats the
/// screen, positioned just above `anchor_y` and horizontally centered within
/// `area`. Pure - no frame access. `body_len` is the number of content lines
/// the popup will hold; `body_cap` is the most body rows it may occupy (System
/// A dialogs cap at [`POPUP_MAX_ROWS`] and scroll; System B is already windowed
/// to its suggestions + `▲/▼`/counter chrome, so it caps higher).
pub(super) fn popup_rect(anchor_y: u16, area: Rect, body_len: usize, body_cap: u16) -> Rect {
    let body_rows = body_len.max(1) as u16;
    let height = (body_rows + 2).min(body_cap + 2).min(area.height);
    let width = area.width.saturating_sub(2).max(1);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = anchor_y.saturating_sub(height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Resolves the title string for a Selector popup: the command's `list_title`
/// from the registry, or the raw command name if it is not registered.
fn selector_popup_title(command: &str) -> String {
    slash::lookup(command)
        .map(|c| c.list_title.to_string())
        .unwrap_or_else(|| command.to_string())
}

/// The body lines a status line spells: a Loading or Failed dialog draws one
/// muted/error line. `Ready` returns `None` (the caller draws the rows). Pure.
fn dialog_status_line(status: &OverlayStatus, title: &str, theme: &Theme) -> Option<Line<'static>> {
    match status {
        OverlayStatus::Loading => Some(Line::styled(
            format!("loading {title}…"),
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )),
        OverlayStatus::Failed(msg) => Some(Line::styled(
            format!("failed: {msg}"),
            Style::default()
                .fg(tui_color(theme.error))
                .add_modifier(Modifier::BOLD),
        )),
        OverlayStatus::Ready => None,
    }
}

// What one overlay draws: the box title, its body lines, and (System A only)
// the active row the box scrolls to keep visible + the body-row cap. A pure
// Parameter Object so the popup painter is one integration step over it (IOSP).
struct PopupDraw {
    title: String,
    lines: Vec<Line<'static>>,
    /// `Some` for System A (the box re-scrolls to this active row); `None` for
    /// System B (already windowed).
    scroll_active: Option<usize>,
    body_cap: u16,
}

/// The System B palette body cap: the [`MAX_SUGGESTIONS`] window plus the three
/// chrome rows (`▲`, `▼`, and the `(n/m)` counter) it may add.
const MENU_BODY_CAP: u16 = MAX_SUGGESTIONS as u16 + 3;

/// The System B palette's cursor state (Parameter Object): the highlighted row,
/// the scroll-window top, and whether the active row is expanded (`←/→`).
/// Bundled so the palette draw pipeline stays integration steps.
#[derive(Debug, Clone, Copy)]
pub(super) struct MenuCursor {
    pub(super) active: usize,
    pub(super) scroll: usize,
    pub(super) expanded: bool,
}

// The System B (`/` palette) draw plan: the color-only suggestion rows +
// windowing chrome; the box never re-scrolls (`scroll_active` None).
fn menu_draw(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    PopupDraw {
        title: "commands".to_string(),
        lines: suggestion_rows(suggestions, cursor, inner_width, theme),
        scroll_active: None,
        body_cap: MENU_BODY_CAP,
    }
}

// The `@path` file picker draw plan (Phase C2, qwen `useAtCompletion`): the
// fuzzy path rows drawn like System B (color-only, no numbers), titled "files".
// While the async fetch is in flight with nothing to show yet, a subtle
// "searching…" line stands in for the rows (qwen shows a loading state).
fn at_draw(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    loading: bool,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    let lines = if loading && suggestions.is_empty() {
        vec![searching_line(theme)]
    } else {
        suggestion_rows(suggestions, cursor, inner_width, theme)
    };
    PopupDraw {
        title: "files".to_string(),
        lines,
        scroll_active: None,
        body_cap: MENU_BODY_CAP,
    }
}

/// The "searching…" placeholder line (muted italic) shown while an AT file
/// search is in flight with no rows yet (qwen's loading state).
fn searching_line(theme: &Theme) -> Line<'static> {
    Line::styled(
        "searching…",
        Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
    )
}

// The System A (numbered `›` dialog) draw plan: a status line, or the numbered
// rows the box scrolls to keep the active row visible.
fn dialog_draw(
    command: &str,
    status: &OverlayStatus,
    rows: &[SelectorRow],
    active: usize,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    let title = selector_popup_title(command);
    let lines = match dialog_status_line(status, &title, theme) {
        Some(line) => vec![line],
        None => dialog_rows(rows, active, inner_width, theme),
    };
    PopupDraw {
        title,
        lines,
        scroll_active: Some(active),
        body_cap: POPUP_MAX_ROWS,
    }
}

/// The inline Composer overlay popup (ADR-0051): a compact bordered list
/// anchored just above `anchor_y`. The TWO systems render distinctly - `Menu`
/// (System B) is the fuzzy `/` palette ([`menu_draw`]), `Dialog` (System A) a
/// committed command's numbered `›` list ([`dialog_draw`]). This is the one
/// integration step: pick the draw plan, size the box, blit the scrolled
/// window (IOSP). Inline and height-bounded - never the full screen.
pub(super) fn render_composer_popup(
    frame: &mut Frame,
    anchor_y: u16,
    area: Rect,
    view: &OverlayView,
    theme: &Theme,
) {
    let inner_width = area.width.saturating_sub(4).max(1);
    let plan = match view {
        OverlayView::Menu {
            suggestions,
            active,
            scroll,
            query: _,
            expanded,
        } => menu_draw(
            suggestions,
            MenuCursor {
                active: *active,
                scroll: *scroll,
                expanded: *expanded,
            },
            inner_width,
            theme,
        ),
        OverlayView::Dialog {
            command,
            status,
            rows,
            active,
            detail: _,
        } => dialog_draw(command, status, rows, *active, inner_width, theme),
        OverlayView::AtFiles {
            suggestions,
            active,
            scroll,
            query: _,
            loading,
        } => at_draw(
            suggestions,
            MenuCursor {
                active: *active,
                scroll: *scroll,
                expanded: false,
            },
            *loading,
            inner_width,
            theme,
        ),
        // The `/mcp` management wizard (ADR-0065 Phase E): a heterogeneous
        // multi-step surface (grouped server list, key/value detail with a radio
        // action list, tool list, tool-schema detail, OAuth progress). Its active
        // row is already baked into each row's semantic `McpStyle`, so it renders
        // as the flattened header/content/footer the fold minted - no re-scroll.
        OverlayView::McpDialog(dialog) => mcp_dialog_draw(dialog, theme),
    };

    let popup = popup_rect(anchor_y, area, plan.lines.len(), plan.body_cap);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(padded(&plan.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tui_color(theme.popup_border)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let visible = inner.height as usize;
    let top = plan
        .scroll_active
        .map(|a| composer::first_visible_row(a, visible.max(1)))
        .unwrap_or(0);
    let shown: Vec<Line> = plan.lines.into_iter().skip(top).take(visible).collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// The `/mcp` dialog's popup plan (ADR-0065 Phase E): the current step's
/// [`McpDialogView`] flattened to lines - the header rows, a blank, the content
/// rows, a blank, then the footer - titled `/mcp` and capped like a System A
/// dialog. The popup Block draws the border, so these rows carry none. Pure over
/// the view + Theme; the active row's highlight already rides each row's
/// [`McpStyle`].
fn mcp_dialog_draw(dialog: &McpDialogView, theme: &Theme) -> PopupDraw {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(dialog.header.iter().map(|r| mcp_row_line(r, theme)));
    lines.push(Line::default());
    lines.extend(dialog.content.iter().map(|r| mcp_row_line(r, theme)));
    lines.push(Line::default());
    lines.push(mcp_row_line(&dialog.footer, theme));
    PopupDraw {
        title: "/mcp".to_string(),
        lines,
        scroll_active: None,
        body_cap: POPUP_MAX_ROWS,
    }
}

/// One rendered [`McpRow`] as a borderless [`Line`]: each [`McpSpan`] mapped from
/// its semantic [`McpStyle`] to the active Theme, plus [`Modifier::BOLD`] when the
/// span asserts `bold` (qwen's orthogonal `<Text bold>` emphasis on header titles,
/// group headings, and TOOL_DETAIL labels).
fn mcp_row_line(row: &McpRow, theme: &Theme) -> Line<'static> {
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

/// The Theme [`Style`] a semantic [`McpStyle`] maps to (qwen's `semantic-colors`
/// reads): the accent/primary/secondary body tones and the success/warning/error
/// status colours, drawn from the same slots the rest of the UI reads.
fn mcp_span_style(style: McpStyle, theme: &Theme) -> Style {
    match style {
        McpStyle::Accent => accent_style(theme),
        McpStyle::Primary => primary_style(theme),
        McpStyle::Secondary => secondary_style(theme),
        McpStyle::Success => success_style(theme),
        McpStyle::Warning => warning_style(theme),
        McpStyle::Error => error_style(theme),
    }
}

/// The System A (numbered `›` dialog) rows for a committed command's list
/// (ADR-0051): the `selection_rows` shape, but over [`SelectorRow`]s whose role
/// decides the disabled mask (headers/greyed rows are dim + unnavigable, the
/// active navigable row is the `›`-marked success-green one) and whose hint
/// trails dimmed. Numbered per the row's position, matching the digit
/// quick-select.
fn dialog_rows(
    rows: &[SelectorRow],
    active: usize,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::styled(
            "no matches",
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )];
    }
    let width = inner_width as usize;
    // The number field is as wide as the widest `N.`.
    let num_field = format!("{}.", rows.len()).width();
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let is_active = i == active && row.is_stop();
            let navigable = row.is_stop();
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0;
            // The 2-wide `›`/space gutter (only a navigable active row marks).
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
            // The right-aligned `N.` number (success-green when active, else
            // secondary; a non-navigable row draws no number).
            if navigable {
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
            } else {
                // A header/greyed row keeps the number field's width blank so
                // its label lines up with the members below it.
                used = push_cols(
                    &mut spans,
                    &" ".repeat(num_field + 1),
                    Style::default(),
                    used,
                    width,
                );
            }
            // The label: success-green when active, dim for header/greyed/note,
            // else primary.
            let label_style = match (is_active, row.role) {
                (true, _) => success_style(theme),
                (false, RowRole::Member) => primary_style(theme),
                (false, _) => secondary_style(theme),
            };
            used = push_cols(&mut spans, &row.label, label_style, used, width);
            // The hint trails dimmed (a "(current)" marker, a note's reason).
            if let Some(hint) = &row.hint {
                used = push_cols(&mut spans, "  ", Style::default(), used, width);
                let _ = push_cols(
                    &mut spans,
                    hint,
                    Style::default()
                        .fg(tui_color(theme.muted))
                        .add_modifier(Modifier::ITALIC),
                    used,
                    width,
                );
            }
            Line::from(spans)
        })
        .collect()
}

/// The most System-B suggestion rows the palette shows before it scrolls (qwen
/// `MAX_SUGGESTIONS_TO_SHOW`); mirrors [`completion::MAX_SUGGESTIONS_TO_SHOW`].
pub(super) const MAX_SUGGESTIONS: usize = completion::MAX_SUGGESTIONS_TO_SHOW;

/// The most body rows the Slash popup shows before it scrolls internally - keeps
/// the overlay compact even against a long model list.
pub(super) const POPUP_MAX_ROWS: u16 = 8;

/// The minimum guaranteed Session Picker width in columns.
pub(super) const MODAL_MIN_WIDTH: u16 = 44;

/// The minimum content width (columns) of the Session Picker popup, including its
/// horizontal padding (+4 for the two border columns plus two inner padding cols).
pub(super) const PICKER_MIN_WIDTH_EXTRA: u16 = 4;

/// The header/footer row overhead added to entry count to size the Picker height
/// (borders top+bottom plus the key-hint footer row).
pub(super) const PICKER_HEIGHT_OVERHEAD: u16 = 3;
