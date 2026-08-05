use ratatui::style::{Color, Modifier, Style};

use crate::ui::markdown::MdStyle;
use crate::ui::theme::{self, Theme};
use crate::view_model::DiffSide;

pub(super) fn tui_color(color: theme::Color) -> Color {
    match color {
        theme::Color::Black => Color::Black,
        theme::Color::Red => Color::Red,
        theme::Color::Green => Color::Green,
        theme::Color::Yellow => Color::Yellow,
        theme::Color::Blue => Color::Blue,
        theme::Color::Magenta => Color::Magenta,
        theme::Color::Cyan => Color::Cyan,
        theme::Color::Gray => Color::Gray,
        theme::Color::DarkGray => Color::DarkGray,
        theme::Color::LightRed => Color::LightRed,
        theme::Color::LightGreen => Color::LightGreen,
        theme::Color::LightYellow => Color::LightYellow,
        theme::Color::LightBlue => Color::LightBlue,
        theme::Color::LightMagenta => Color::LightMagenta,
        theme::Color::LightCyan => Color::LightCyan,
        theme::Color::White => Color::White,
        theme::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The ONE mapping from a diff's [`DiffSide`] to its fallback foreground
/// (ADR-0008): added reads green, removed red, context the muted context slot.
/// This is the fg the marker glyph always wears (so add/remove reads without
/// truecolor) and the code text falls back to when no syntect fragment colors
/// it. The added/removed background TINT is a separate mapping ([`diff_tint`]).
pub(super) fn diff_side_fg(side: DiffSide, theme: &Theme) -> Color {
    match side {
        DiffSide::Added => tui_color(theme.added),
        DiffSide::Removed => tui_color(theme.removed),
        DiffSide::Context => tui_color(theme.context),
    }
}

/// The muted-italic style a diff's adapter CHROME wears (ADR-0008): the `@@ … @@`
/// hunk header and the `… N more lines` elision tail - neither is a code line,
/// so neither carries a marker, a tint, or syntect fg. One helper so both read
/// the same.
pub(super) fn diff_chrome_style(theme: &Theme) -> Style {
    Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC)
}

// ---------------------------------------------------------------------------
// qwen v0.16.0 colour ROLES → suspenders Theme slots (Phase 2, ADR-0046).
//
// qwen-code paints its committed chrome from a handful of semantic ROLES
// (`text.accent`, `text.secondary`, `status.success`, …). This port maps each
// role onto a suspenders Theme slot behind ONE helper apiece, so a theme
// reconcile is a single edit per role rather than a hunt through the render
// body. Phase 7 (ADR-0008/0053) carved the four qwen roles that used to borrow
// a neighbouring slot into real slots: `text.primary`→`foreground`,
// `text.accent`→`accent`, `status.success`→`success`,
// `status.warning`→`warning`. `text.secondary`/`ui.symbol`/`border.default`
// still share the neutral gray `muted` slot by design.
// ---------------------------------------------------------------------------

/// qwen `text.accent` (AccentPurple `#D2A6FF`): the user `>` caret + the
/// assistant `✦` marker. Reads the dedicated `accent` slot (Phase 7, ADR-0008).
pub(super) fn accent_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.accent))
}

/// qwen `text.secondary` (Gray): thought glyph/body, tool descriptions, retry,
/// hints. Maps to the `muted` slot.
pub(super) fn secondary_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
}

/// qwen `status.success` (AccentGreen `#AAD94C`): the success prefix + the
/// `✓`/`o` tool markers. Reads the dedicated `success` slot (Phase 7,
/// ADR-0008), a distinct role rather than the diff `added` green it once borrowed.
pub(super) fn success_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.success))
}

/// qwen `status.warning` (AccentYellow `#FFD700`): the `△` warning prefix + a
/// pending tool-group border. Reads the dedicated `warning` slot (Phase 7,
/// ADR-0008).
pub(super) fn warning_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.warning))
}

/// qwen `status.error` (AccentRed): the `✕`/`x` error prefix + marker. Maps to
/// the `error` slot.
pub(super) fn error_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.error))
}

/// qwen `text.primary` (Foreground `#bfbdb6`): body text, info bodies, tool
/// names. Reads the dedicated `foreground` slot (Phase 7, ADR-0008) - a real
/// pinned colour rather than the terminal default, so body text matches
/// QwenDark on any background.
pub(super) fn primary_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.foreground))
}

/// qwen `ui.symbol` (Gray): a shell tool's marker + a shell tool-group's border.
/// Maps to the `muted` slot (same read as `border.default`).
pub(super) fn symbol_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
}

/// qwen `border.default` (Gray): the resting tool-group box border. Maps to the
/// `muted` slot.
pub(super) fn border_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
}

/// The ONE mapping from a semantic markdown [`MdStyle`] to a ratatui [`Style`]
/// (ADR-0008's move, applied to assistant markdown): [`markdown::to_lines`]
/// speaks semantics; this is where they become the active Theme's colors.
pub fn md_style(style: MdStyle, theme: &Theme) -> Style {
    match style {
        MdStyle::Plain => Style::default(),
        MdStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        MdStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        MdStyle::BoldItalic => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        MdStyle::Code => Style::default().fg(tui_color(theme.code)),
        MdStyle::CodeBlock => Style::default()
            .fg(tui_color(theme.code_block))
            .bg(tui_color(theme.code_block_bg)),
        MdStyle::Heading => Style::default()
            .fg(tui_color(theme.heading))
            .add_modifier(Modifier::BOLD),
        MdStyle::Bullet => Style::default().fg(tui_color(theme.bullet)),
        MdStyle::Quote => Style::default()
            .fg(tui_color(theme.quote))
            .add_modifier(Modifier::ITALIC),
        MdStyle::Link => Style::default()
            .fg(tui_color(theme.link))
            .add_modifier(Modifier::UNDERLINED),
    }
}
