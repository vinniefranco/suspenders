//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`LineStyle`] →
//! color for a Block's lines, [`PressureLevel`] → color/emphasis for the status
//! bar. Plugins and the Transcript core never touch ratatui; they speak the
//! vocabulary and this module renders it. Everything here is pure presentation
//! of [`TranscriptItem`]s - no state, no IO. Only this module and [`crate::ui`]
//! `use ratatui` / `use crossterm` (ADR-0019 invariant).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::ui::transcript::{
    LineStyle, PressureLevel, Status, StyledLine, Transcript, TranscriptItem,
};

// ---------------------------------------------------------------------------
// The single semantic → color mapping (ADR-0008).
// ---------------------------------------------------------------------------

/// The ONE mapping from a semantic [`LineStyle`] to a ratatui [`Style`]
/// (ADR-0008). Plugins produce styles; this turns them into colors.
pub fn line_style(style: LineStyle) -> Style {
    match style {
        LineStyle::Added => Style::default().fg(Color::Green),
        LineStyle::Removed => Style::default().fg(Color::Red),
        LineStyle::Context => Style::default().fg(Color::DarkGray),
        LineStyle::Emphasis => Style::default().add_modifier(Modifier::BOLD),
        LineStyle::Muted => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        LineStyle::Default => Style::default(),
    }
}

/// The ONE mapping from the semantic [`PressureLevel`] (ADR-0008) to a status
/// bar style: `Ok` reads muted, `Elevated` warns, `Critical` alarms.
pub fn pressure_style(level: PressureLevel) -> Style {
    match level {
        PressureLevel::Critical => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
        PressureLevel::Elevated => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        PressureLevel::Ok => Style::default().fg(Color::DarkGray),
    }
}

// ---------------------------------------------------------------------------
// Render helpers.
// ---------------------------------------------------------------------------

/// Renders the whole frame: the transcript viewport, the status bar, the input
/// line, and - when an Approval is pending - the modal on top. `scroll` is the
/// viewport's top line offset the adapter tracks (0 = follow the tail).
pub fn render(frame: &mut Frame, t: &Transcript, base_url: &str, spinner: u64, scroll: u16) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transcript viewport
            Constraint::Length(1), // status bar
            Constraint::Length(1), // input line
        ])
        .split(area);

    render_viewport(frame, chunks[0], t, scroll);
    render_status_bar(frame, chunks[1], t, base_url, spinner);
    render_input_line(frame, chunks[2], t);

    if let Some(pending) = &t.pending_approval {
        render_approval_modal(frame, area, &pending.command);
    }
}

/// The transcript viewport: the message list, oldest first, plus any in-flight
/// streaming Thinking/text.
pub fn render_viewport(frame: &mut Frame, area: Rect, t: &Transcript, scroll: u16) {
    let mut lines: Vec<Line> = t.messages.iter().flat_map(message_lines).collect();

    // The live streaming snapshot renders below the settled items.
    let thinking = t.streaming_thinking();
    if !thinking.is_empty() {
        lines.push(Line::styled(
            format!("🧠 thinking… ({} chars)", thinking.chars().count()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let streaming_text = t.streaming_text();
    if !streaming_text.is_empty() {
        lines.extend(text_rows(&streaming_text).into_iter().map(Line::raw));
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// The lines one Transcript item renders as. `Block` is the semantic display
/// vocabulary (ADR-0008): a titled block whose lines take their color from
/// [`line_style`].
fn message_lines(item: &TranscriptItem) -> Vec<Line<'static>> {
    match item {
        // User prompts: the "› " gutter on the first row, continuation rows
        // aligned under it. Multi-line input renders as multiple rows.
        TranscriptItem::User { text } => {
            let gutter = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            text_rows(text)
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let prefix = if i == 0 { "› " } else { "  " };
                    Line::from(vec![Span::styled(prefix, gutter), Span::raw(row)])
                })
                .collect()
        }
        // Assistant text is rendered as plain text, split into rows on newline
        // (ratatui does not break a single Line on embedded '\n'); width-wrapping
        // is left to the viewport Paragraph's Wrap.
        TranscriptItem::Assistant { text } => text_rows(text)
            .into_iter()
            .map(Line::raw)
            .collect(),
        TranscriptItem::Thinking { text } => vec![Line::styled(
            format!("🧠 thought: {}", first_line(text)),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )],
        TranscriptItem::ToolCall { name, summary } => vec![Line::styled(
            format!("⚙ {}", join_summary(name, summary)),
            Style::default().fg(Color::Yellow),
        )],
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: false,
        } => vec![Line::styled(
            format!("⚙ {name} → {summary}"),
            Style::default().fg(Color::Yellow),
        )],
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: true,
        } => vec![Line::styled(
            format!("⚙ {name} ✗ {summary}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )],
        TranscriptItem::Block { title, lines } => {
            let mut out = vec![Line::styled(
                format!("⚙ {title}"),
                Style::default().fg(Color::Yellow),
            )];
            out.extend(lines.iter().map(block_line));
            out
        }
        TranscriptItem::Info { text } => {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            text_rows(text)
                .into_iter()
                .map(|row| Line::styled(row, style))
                .collect()
        }
    }
}

/// Splits multi-line text into one string per source row: ratatui does not break
/// a single `Line` on an embedded '\n', so multi-line messages must become
/// multiple `Line`s or they collapse into one blob. Tabs become two spaces; `\r`
/// is stripped; empty lines survive as blank rows. Width-wrapping is the
/// Paragraph's job (`Wrap`), so this only handles hard line breaks.
fn text_rows(text: &str) -> Vec<String> {
    text.replace('\r', "")
        .split('\n')
        .map(|row| row.replace('\t', "  "))
        .collect()
}

fn block_line(line: &StyledLine) -> Line<'static> {
    let text = if line.text.is_empty() {
        " ".to_string()
    } else {
        line.text.replace('\t', "  ")
    };
    Line::styled(text, line_style(line.style))
}

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Turn is running.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The bottom status bar: `suspenders`, the base_url, the agent status (an
/// animated spinner while running), and the live `~Ntok / budget` estimate
/// colored by the semantic [`PressureLevel`].
pub fn render_status_bar(frame: &mut Frame, area: Rect, t: &Transcript, base_url: &str, spinner: u64) {
    let status_style = match t.status {
        Status::Running => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Status::Idle => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    };

    // While running, an animated braille spinner precedes "running"; idle is
    // static. The frame counter comes from the adapter's animation tick.
    let status_text = match t.status {
        Status::Running => format!("{} running", SPINNER[(spinner as usize) % SPINNER.len()]),
        Status::Idle => "idle".to_string(),
    };

    let mut spans = vec![
        Span::styled(" suspenders ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(base_url.to_string(), Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(status_text, status_style),
    ];

    if let Some(label) = tokens_label(t.token_estimate, t.context_budget) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(label, pressure_style(t.pressure_level)));
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(30, 30, 40)));
    frame.render_widget(bar, area);
}

/// The input line: the composer prompt and its current value.
pub fn render_input_line(frame: &mut Frame, area: Rect, t: &Transcript) {
    let line = Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(t.input_value.clone()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The Approval modal for a run_command Tool Call: `y` approves, `n` denies,
/// `a` approves-always. Key handling lives in the Transcript core; this draws it.
pub fn render_approval_modal(frame: &mut Frame, area: Rect, command: &str) {
    let width = (command.chars().count() as u16 + 8).max(44).min(area.width.saturating_sub(4));
    let height = 8u16.min(area.height.saturating_sub(2));
    let modal = centered_rect(width, height, area);

    frame.render_widget(Clear, modal);
    let block = Block::default().title("Approval").borders(Borders::ALL);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let body = Paragraph::new(vec![
        Line::styled("Run command?", Style::default().add_modifier(Modifier::BOLD)),
        Line::styled(command.to_string(), Style::default().fg(Color::Yellow)),
        Line::raw(""),
        Line::from(vec![
            Span::styled("[y]es", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" / "),
            Span::styled("[n]o", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" / "),
            Span::styled("[a]lways", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
        ]),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(body, inner);
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// The `~Ntok / budget` label, or `None` when either number is missing.
fn tokens_label(estimate: Option<u64>, budget: Option<u64>) -> Option<String> {
    match (estimate, budget) {
        (Some(estimate), Some(budget)) => Some(format!("~{estimate}tok / {budget}")),
        _ => None,
    }
}

fn join_summary(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

/// A centered `width`×`height` rect inside `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}
