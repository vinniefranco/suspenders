//! The --resume Session Picker modal (a centered bordered list).
//!
//! Split from the components god module; shared primitives arrive via
//! `use super::*`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::picker::Picker;
use crate::ui::theme::Theme;

use super::style::tui_color;
use super::text::centered_rect;

/// Computes the bounding rect for the Session Picker modal: derives the needed
/// content width from the entries and footer, clamps both dimensions to the
/// terminal, and returns a centered `Rect`. Pure - no frame access.
pub(super) fn picker_rect(picker: &Picker, area: Rect) -> Rect {
    const FOOTER: &str = "↑/↓ select · Enter resume · Esc fresh session · q quit";

    let content_width = picker
        .entries
        .iter()
        .map(|e| e.stamp.chars().count() + 2 + e.label.chars().count())
        .chain(std::iter::once(FOOTER.chars().count()))
        .max()
        .unwrap_or(0) as u16;
    let width = (content_width + PICKER_MIN_WIDTH_EXTRA)
        .max(MODAL_MIN_WIDTH)
        .min(area.width.saturating_sub(2));
    let height =
        (picker.entries.len() as u16 + PICKER_HEIGHT_OVERHEAD).min(area.height.saturating_sub(2));
    centered_rect(width, height, area)
}

/// The `--resume` Session Picker: a centered bordered list, one row per
/// Session (`stamp  label`), the cursor row reversed+bold, and a dim key-hint
/// footer. Key handling lives in the pure [`Picker`] core; this only draws.
pub fn render_picker(frame: &mut Frame, picker: &Picker, theme: &Theme) {
    const FOOTER: &str = "↑/↓ select · Enter resume · Esc fresh session · q quit";

    let area = frame.area();
    let modal = picker_rect(picker, area);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" resume a session ")
        .borders(Borders::ALL);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let mut lines: Vec<Line> = picker
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let style = if i == picker.cursor {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(format!("{}  {}", entry.stamp, entry.label), style)
        })
        .collect();
    lines.push(Line::styled(
        FOOTER,
        Style::default().fg(tui_color(theme.muted)),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The minimum guaranteed Session Picker width in columns.
pub(super) const MODAL_MIN_WIDTH: u16 = 44;

/// The minimum content width (columns) of the Session Picker popup, including its
/// horizontal padding (+4 for the two border columns plus two inner padding cols).
pub(super) const PICKER_MIN_WIDTH_EXTRA: u16 = 4;

/// The header/footer row overhead added to entry count to size the Picker height
/// (borders top+bottom plus the key-hint footer row).
pub(super) const PICKER_HEIGHT_OVERHEAD: u16 = 3;
