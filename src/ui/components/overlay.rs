use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::approvals::ApprovalMode;
use crate::ui::screen::{OTHER_OPTION_LABEL, PendingPlan, PendingQuestion};
use crate::ui::slash;
use crate::ui::theme::Theme;

use super::approval::selection_rows;
use super::box_draw::{box_bottom, box_row, box_top};
use super::markdown_render::markdown_lines;
use super::scroll::{anchor_clip, draw_overflow_marker};
use super::style::{accent_style, border_style, primary_style, secondary_style, success_style};
use super::text::{push_cols, truncate_cols};
use super::tool_group::{CONTENT_MARGIN, content_width};

// ---------------------------------------------------------------------------
// The keyboard-shortcuts Help overlay (qwen `Help.tsx`, the `?` affordance the
// footer's `? for shortcuts` promises). A single bordered panel: qwen spreads
// its help over THREE tabs (general / commands / custom-commands); suspenders
// ports the CONTENT (shortcuts + the built-in COMMANDS registry) into one
// panel and drops the tab chrome. The Screen's `help_open` flag gates it and
// [`Screen::handle_help_key`] closes it.
// ---------------------------------------------------------------------------

/// The width of the accent key column in a shortcut row (qwen `KEY_COL_WIDTH`,
/// Help.tsx:42): the fixed-width column the key sits in before its description.
const HELP_KEY_COL_WIDTH: usize = 12;

/// The gap (columns) between the two shortcut columns (qwen `GeneralHelp` `gap:2`).
const HELP_COL_GAP: usize = 2;

/// The (key, description) shortcut rows the Help panel lists, verified against
/// `ui.rs` `map_key` + `screen.rs` routing. `@` is already promised by the
/// composer placeholder (the AT-completion phase wires its behaviour), so it is
/// listed here alongside the live bindings.
const HELP_SHORTCUTS: &[(&str, &str)] = &[
    ("/", "Open the command menu"),
    ("@", "Add files or folders as context"),
    ("?", "Show this help"),
    ("Enter", "Submit (steer a running turn)"),
    ("Alt+Enter", "Insert a newline"),
    ("Esc", "Cancel a running turn / close a menu"),
    ("Ctrl+O", "Toggle compact mode"),
    ("Ctrl+S", "Scroll up a page through the transcript"),
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
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: content_width(area.width),
        ..area
    };
    let lines = help_panel_lines(content_area.width, theme);
    // Bottom-anchor + top-clip exactly like the pending body: keep the LAST
    // `height` rows (qwen's `overflowDirection:"top"`) when the panel is tall, else
    // pad the top so the footer meets the composer.
    let total = lines.len();
    let clip = anchor_clip(total, area, content_area);
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

    let mut rows: Vec<Line<'static>> = Vec::new();
    rows.push(Line::styled(format!("╭{}╮", "─".repeat(inner)), border));
    for row in help_panel_body_rows(inner, theme) {
        rows.push(box_row(&row.spans, inner, border));
    }
    rows.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), border));
    rows
}

/// The Help panel's borderless content rows (qwen `GeneralHelp` + `CommandsHelp`),
/// each clipped to `inner` columns - [`help_panel_lines`] wraps them in the box.
/// The order: title, blank, `Shortcuts` heading, the shortcut rows, blank,
/// `Commands` heading, the built-in command rows, blank, the `Esc to close` footer.
fn help_panel_body_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
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
fn help_heading_row(label: &str, inner: usize, theme: &Theme) -> Line<'static> {
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
fn help_full_cell_width() -> usize {
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
fn help_shortcut_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
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
fn help_shortcut_cell(
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
fn help_command_row(cmd: &slash::SlashCommand, inner: usize, theme: &Theme) -> Line<'static> {
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

/// The question-modal title (ADR-0057, qwen `askUserQuestion` confirmation
/// title, VERBATIM).
const QUESTION_MODAL_TITLE: &str = "Please answer the following question(s):";

/// The free-form "Other" capture hint shown under a question while the composer
/// collects the answer (ADR-0057): tells the user to type below and submit.
const QUESTION_OTHER_HINT: &str = "Type your answer below, then press Enter.";

/// The question modal as a standalone bordered box (ADR-0057, qwen
/// `ask_user_question`): a rounded box with the title, then each question's text
/// and its numbered radio (its options PLUS the auto-appended "Other" row).
/// Answered questions show their recorded answer; the one collecting a free-form
/// "Other" answer shows the composer hint. Every content row is `<= inner`
/// columns and boxed to exactly `inner + 2` (measure==draw, ADR-0029). Rendered
/// bottom-most in the pending body so the top-clip never eats it.
pub(super) fn question_modal_lines(
    pending: &PendingQuestion,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The box interior width (border + paddingX:1 each side is the box's own; the
    // interior here is width - 2 for the two border columns).
    let inner = (width as usize).saturating_sub(2);
    let inner_u16 = inner as u16;
    let border = border_style(theme);

    let mut body: Vec<Line<'static>> = Vec::new();

    // Title row.
    let mut title_spans = Vec::new();
    let _ = push_cols(
        &mut title_spans,
        QUESTION_MODAL_TITLE,
        primary_style(theme).add_modifier(Modifier::BOLD),
        0,
        inner,
    );
    body.push(Line::from(title_spans));

    for (i, question) in pending.questions().iter().enumerate() {
        // A blank gap before each question (qwen `marginBottom:1`).
        body.push(Line::from(Vec::<Span<'static>>::new()));

        // The `[header]` chip + the question text (secondary chip, primary text).
        let mut q_spans = Vec::new();
        let used = push_cols(
            &mut q_spans,
            &format!("[{}] ", question.header),
            secondary_style(theme),
            0,
            inner,
        );
        let _ = push_cols(
            &mut q_spans,
            &question.question,
            primary_style(theme),
            used,
            inner,
        );
        body.push(Line::from(q_spans));

        // The per-question rows: the recorded answer, the free-form hint, or the
        // interactive radio - one branch per state.
        if let Some(answer) = pending.answer(i) {
            // Answered: a success-green `✓ answer` line.
            let mut a_spans = Vec::new();
            let _ = push_cols(
                &mut a_spans,
                &format!("✓ {answer}"),
                success_style(theme),
                0,
                inner,
            );
            body.push(Line::from(a_spans));
        } else if pending.collecting_other() == Some(i) {
            // Collecting a free-form "Other" answer: the hint (the composer draws
            // below this box).
            let mut h_spans = Vec::new();
            let _ = push_cols(
                &mut h_spans,
                QUESTION_OTHER_HINT,
                secondary_style(theme),
                0,
                inner,
            );
            body.push(Line::from(h_spans));
        } else {
            // The interactive radio: the question's option labels PLUS the
            // auto-appended "Other" row. `active` reads the per-question
            // SelectionList; only the CURRENT question (cursor) is highlighted.
            let mut labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            labels.push(OTHER_OPTION_LABEL);
            let active = pending.active_row(i);
            body.extend(selection_rows(&labels, active, true, inner_u16, theme));
        }
    }

    // Frame the body in a rounded box, every row exactly `inner + 2` columns.
    let mut lines = vec![box_top(inner, border)];
    lines.extend(body.iter().map(|line| box_row(&line.spans, inner, border)));
    lines.push(box_bottom(inner, border));
    lines
}

/// The plan-modal title (ADR-0067, qwen `exitPlanMode.ts:176` confirmation
/// `title`, VERBATIM).
const PLAN_MODAL_TITLE: &str = "Would you like to proceed?";

/// The plan modal's four outcome rows (ADR-0067, qwen
/// `ToolConfirmationMessage.tsx:465-490`, IN THAT ROW ORDER). `pre_plan_mode` is
/// worded into the restore row's `({mode})` with the mode's qwen wire string
/// (`ApprovalMode::wire_str`), matching qwen's `t('...({{mode}})', {mode:
/// planProps.prePlanMode ?? 'default'})` interpolation (the enum VALUE string,
/// e.g. `default`/`auto-edit`, not the footer's `plan mode` label).
fn plan_outcome_labels(pre_plan_mode: ApprovalMode) -> [String; 4] {
    [
        format!("Yes, restore previous mode ({})", pre_plan_mode.wire_str()),
        "Yes, and auto-accept edits".to_string(),
        "Yes, and manually approve edits".to_string(),
        "No, keep planning (esc)".to_string(),
    ]
}

/// The plan-confirmation modal as a standalone bordered box (ADR-0067, qwen
/// `exit_plan_mode`): a rounded box with the title, the plan text rendered as
/// markdown (the same [`markdown_lines`] the transcript uses), then the four
/// outcome rows via a numbered radio. The active row (the modal's own
/// [`SelectionList`]) is highlighted. Every content row is `<= inner` columns and
/// boxed to exactly `inner + 2` (measure==draw, ADR-0029). Rendered bottom-most
/// in the pending body so the top-clip never eats it.
pub(super) fn plan_modal_lines(
    pending: &PendingPlan,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner = (width as usize).saturating_sub(2);
    let inner_u16 = inner as u16;
    let border = border_style(theme);

    let mut body: Vec<Line<'static>> = Vec::new();

    // Title row (qwen `planProps.title`).
    let mut title_spans = Vec::new();
    let _ = push_cols(
        &mut title_spans,
        PLAN_MODAL_TITLE,
        primary_style(theme).add_modifier(Modifier::BOLD),
        0,
        inner,
    );
    body.push(Line::from(title_spans));

    // A blank gap, then the plan text rendered as markdown (qwen renders the plan
    // via `MarkdownDisplay`); `box_row` truncates each rendered line to `inner`.
    body.push(Line::from(Vec::<Span<'static>>::new()));
    body.extend(markdown_lines(pending.plan(), theme));

    // A blank gap before the outcome radio (qwen `marginBottom:1`).
    body.push(Line::from(Vec::<Span<'static>>::new()));
    let labels = plan_outcome_labels(pending.pre_plan_mode());
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let active = pending.active_row();
    body.extend(selection_rows(&label_refs, active, true, inner_u16, theme));

    // Frame the body in a rounded box, every row exactly `inner + 2` columns.
    let mut lines = vec![box_top(inner, border)];
    lines.extend(body.iter().map(|line| box_row(&line.spans, inner, border)));
    lines.push(box_bottom(inner, border));
    lines
}
