//! The interactive Approval + question-modal rendering (ADR-0049/0057): the
//! numbered `›` selection radio, the verbatim approval options + question line,
//! the inline approval block appended inside a confirming tool group, and the
//! standalone question modal. Split from the components god module by rendering
//! responsibility; shared primitives arrive via `use super::*`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::ui::screen::{ConfirmKind, OTHER_OPTION_LABEL, PendingApproval, PendingQuestion};
use crate::ui::theme::Theme;

use super::box_draw::frame_box;
use super::popup::number_label;
use super::style::{border_style, primary_style, secondary_style, success_style};
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
        number_label(items.len()).width()
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
                let num = number_label(i + 1);
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
pub(super) const APPROVAL_OPTIONS: [&str; 3] = [
    "Yes, allow once",
    "Always allow in this project",
    "No, suggest changes (esc)",
];

/// The Approval question line (ADR-0049, qwen verbatim): `Exec` reads `Allow
/// execution of: '{command}'?`, `Info` reads `Do you want to proceed?`.
pub(super) fn approval_question(kind: ConfirmKind, command: &str) -> String {
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
    let question = approval_question(pending.kind, &pending.command);
    let mut spans = Vec::new();
    let _ = push_cols(&mut spans, &question, primary_style(theme), 0, width);
    rows.push(Line::from(spans));
    rows.extend(selection_rows(
        &APPROVAL_OPTIONS,
        pending.selection.active(),
        true,
        inner_width,
        theme,
    ));
    rows
}

/// The question-modal title (ADR-0057, qwen `askUserQuestion` confirmation
/// title, VERBATIM).
pub(super) const QUESTION_MODAL_TITLE: &str = "Please answer the following question(s):";

/// The free-form "Other" capture hint shown under a question while the composer
/// collects the answer (ADR-0057): tells the user to type below and submit.
pub(super) const QUESTION_OTHER_HINT: &str = "Type your answer below, then press Enter.";

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

    for (i, question) in pending.questions.iter().enumerate() {
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
        if let Some(Some(answer)) = pending.answers.get(i) {
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
        } else if pending.collecting_other == Some(i) {
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
            let active = pending.per_question.get(i).map(|s| s.active()).unwrap_or(0);
            body.extend(selection_rows(&labels, active, true, inner_u16, theme));
        }
    }

    // Frame the body in a rounded box, every row exactly `inner + 2` columns.
    frame_box(&body, inner, border)
}
