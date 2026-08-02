//! The startup Header banner (qwen `AppHeader` = `Header` + `Tips`): the ASCII
//! wordmark logo, the bordered info panel, and the `Tips:` line, arranged into
//! one of three width tiers. Split from the components god module by rendering
//! responsibility; shared box/text primitives arrive via `use super::*`.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::ui::theme::Theme;

use super::box_draw::frame_box;
use super::style::{accent_style, border_style, secondary_style};
use super::text::{push_cols, truncate_cols};

/// The ASCII wordmark logo (qwen `AsciiArt.ts` `shortAsciiLogo`), "suspenders"
/// in the ANSI-Shadow block font. All 6 rows are EXACTLY 83 display columns wide,
/// so the two-column width gate ([`header_lines`]) can size the layout against a
/// fixed logo width. Drawn in the theme accent colour.
pub(super) const HEADER_LOGO: &str = "\
███████╗██╗   ██╗███████╗██████╗ ███████╗███╗   ██╗██████╗ ███████╗██████╗ ███████╗
██╔════╝██║   ██║██╔════╝██╔══██╗██╔════╝████╗  ██║██╔══██╗██╔════╝██╔══██╗██╔════╝
███████╗██║   ██║███████╗██████╔╝█████╗  ██╔██╗ ██║██║  ██║█████╗  ██████╔╝███████╗
╚════██║██║   ██║╚════██║██╔═══╝ ██╔══╝  ██║╚██╗██║██║  ██║██╔══╝  ██╔══██╗╚════██║
███████║╚██████╔╝███████║██║     ███████╗██║ ╚████║██████╔╝███████╗██║  ██║███████║
╚══════╝ ╚═════╝ ╚══════╝╚═╝     ╚══════╝╚═╝  ╚═══╝╚═════╝ ╚══════╝╚═╝  ╚═╝╚══════╝";

/// The fixed display width of every [`HEADER_LOGO`] row (qwen `getAsciiArtWidth`).
pub(super) const HEADER_LOGO_WIDTH: usize = 83;

/// The gap columns between the logo and the info panel (qwen `logoGap`).
pub(super) const HEADER_LOGO_GAP: usize = 2;

/// The minimum readable working-directory path width (qwen `minPathLength`); with
/// the box chrome it sets the minimum info-panel width the logo must leave room
/// for before the two-column layout is used.
pub(super) const HEADER_MIN_PATH: usize = 40;

/// The info panel's inner content width in a two-column layout is capped here
/// (qwen `maxInfoPanelWidth = 60`, minus the box chrome), so a very wide terminal
/// does not stretch the panel across the whole screen beside the logo.
pub(super) const HEADER_MAX_PANEL_INNER: usize = 60 - HEADER_BOX_CHROME;

/// The box chrome width the info panel spends on borders + padding: `│ ` left and
/// ` │` right (qwen `borderWidth 2 + paddingX*2`).
pub(super) const HEADER_BOX_CHROME: usize = 4;

/// The borrowed startup Header facts the render path takes (qwen `AppHeader`
/// props): the brand title, crate version, scoped model id, working directory,
/// and the startup tip. A value object so [`header_lines`] takes one borrow.
pub(super) struct HeaderView<'a> {
    pub(super) title: &'a str,
    pub(super) version: &'a str,
    pub(super) model: &'a str,
    pub(super) cwd: &'a str,
    pub(super) tip: &'a str,
}

/// The widest the STACKED tier lets its info panel + tips grow (columns): a full
/// content width beyond this reads the cap, so the box and tips do not sprawl the
/// whole screen under a full-width logo banner. Chosen at qwen's `maxInfoPanelWidth`.
pub(super) const HEADER_STACKED_MAX_WIDTH: usize = 80;

/// Which of the three width tiers the startup [`TranscriptItem::Header`] draws in,
/// resolved from the content width `W` against the fixed 83-col logo. The gate is
/// the ONE place the tier boundaries live so the render and the tests agree:
///
/// * [`HeaderTier::SideBySide`] - `W >= 83 + gap(2) + min_panel(44) = 129`: the
///   logo left, the boxed panel right.
/// * [`HeaderTier::Stacked`] - `83 <= W < 129`: the full-width logo banner on top,
///   the boxed panel (capped at [`HEADER_STACKED_MAX_WIDTH`]) below it, left-aligned.
/// * [`HeaderTier::NoLogo`] - `W < 83`: the logo cannot fit, so the panel (+ tips)
///   render alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeaderTier {
    SideBySide,
    Stacked,
    NoLogo,
}

/// Resolves the [`HeaderTier`] for a content width (the ONE gate, so render and
/// tests share the boundary math).
pub(super) fn header_tier(available: usize) -> HeaderTier {
    let min_panel = HEADER_MIN_PATH + HEADER_BOX_CHROME;
    if available >= HEADER_LOGO_WIDTH + HEADER_LOGO_GAP + min_panel {
        HeaderTier::SideBySide
    } else if available >= HEADER_LOGO_WIDTH {
        HeaderTier::Stacked
    } else {
        HeaderTier::NoLogo
    }
}

/// The lines the startup [`TranscriptItem::Header`] banner renders as (qwen
/// `AppHeader` = `Header` + `Tips`): the ASCII wordmark logo (accent), a single-
/// border info panel, and a `Tips:` line below - arranged by [`header_tier`] into
/// one of three width tiers (side-by-side / stacked / no-logo). The epic wordmark
/// shows on ANY terminal that can fit its 83 columns (tiers 1-2); only a truly
/// narrow terminal (< 83) hides it. Every produced [`Line`] is `<= content_width`
/// columns (the box rows funnelled through the same column-exact assembly as the
/// tool-group box), so the viewport's `Wrap` never re-breaks it (measure==draw,
/// ADR-0029).
pub(super) fn header_lines(view: &HeaderView<'_>, content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let available = content_width as usize;
    let tier = header_tier(available);

    // The info panel's inner content width, per tier:
    // - SideBySide: the space left of the logo + gap, capped at qwen's max.
    // - Stacked:    the width up to HEADER_STACKED_MAX_WIDTH, minus box chrome.
    // - NoLogo:     the full width minus box chrome.
    let panel_inner = match tier {
        HeaderTier::SideBySide => {
            (available - HEADER_LOGO_WIDTH - HEADER_LOGO_GAP - HEADER_BOX_CHROME)
                .min(HEADER_MAX_PANEL_INNER)
        }
        HeaderTier::Stacked => available
            .min(HEADER_STACKED_MAX_WIDTH)
            .saturating_sub(HEADER_BOX_CHROME),
        HeaderTier::NoLogo => available.saturating_sub(HEADER_BOX_CHROME),
    }
    .max(1);

    // The bordered info panel (qwen `Header` info column): the 4 content rows
    // wrapped in a single-line box - always drawn; the logo placement is the tier's.
    let panel = header_boxed_panel(view, panel_inner, theme);
    let mut out = match tier {
        HeaderTier::SideBySide => header_two_column(&panel, theme),
        HeaderTier::Stacked => header_stacked(panel, theme),
        HeaderTier::NoLogo => panel,
    };
    // The Tips line below the box (qwen `Tips`), in secondary, `<= content_width`.
    out.push(header_tips_line(view.tip, available, theme));
    out
}

/// The bordered info panel (qwen `Header`): the 4 content rows funnelled through
/// [`box_row`] to the exact `inner` width, framed with a single-line top/bottom
/// border - exactly `inner + 2` columns per row and 6 rows tall (1 top + 4 content
/// + 1 bottom), so it lines up beside the 6-row logo in the two-column layout.
pub(super) fn header_boxed_panel(view: &HeaderView<'_>, inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let border = border_style(theme);
    frame_box(&header_panel_rows(view, inner, theme), inner, border)
}

/// The four info-panel content rows (qwen `Header` info column), each already
/// clipped to `inner` columns: the bold accent title + secondary version, a blank
/// spacer, the scoped model id with a ` (/model to change)` hint when it fits, and
/// the tilde-shortened working directory. Borderless spans - [`header_two_column`]
/// or the one-column path wraps them in the box.
pub(super) fn header_panel_rows(view: &HeaderView<'_>, inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    // Title line: `>_ suspenders` bold accent, then ` (v<version>)` secondary.
    let title_line = {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        used = push_cols(
            &mut spans,
            &format!(">_ {}", view.title),
            accent_style(theme).add_modifier(Modifier::BOLD),
            used,
            inner,
        );
        push_cols(
            &mut spans,
            &format!(" (v{})", view.version),
            secondary_style(theme),
            used,
            inner,
        );
        Line::from(spans)
    };

    // Model line: the scoped id, plus ` (/model to change)` when the whole line
    // still fits the inner width (qwen `showModelHint`).
    let model_line = {
        let hint = " (/model to change)";
        let mut spans: Vec<Span<'static>> = Vec::new();
        let used = push_cols(&mut spans, view.model, secondary_style(theme), 0, inner);
        // The hint rides along only when the whole line still fits (qwen
        // `showModelHint`); otherwise the model id shows alone.
        if view.model.width() + hint.width() <= inner {
            push_cols(&mut spans, hint, secondary_style(theme), used, inner);
        }
        Line::from(spans)
    };

    // Directory line: tilde-abbreviated then column-clipped to the inner width.
    let dir_line = {
        let path = tildeify_path(view.cwd);
        Line::from(Span::styled(
            truncate_cols(&path, inner),
            secondary_style(theme),
        ))
    };

    vec![title_line, Line::default(), model_line, dir_line]
}

/// The logo + boxed info panel side by side (qwen two-column `Header`): the 6
/// accent logo rows on the left, a [`HEADER_LOGO_GAP`]-col gap, then the pre-built
/// 6-row bordered `panel` box (they line up 1:1). Every row is exactly
/// `HEADER_LOGO_WIDTH + gap + inner + 2` columns (measure==draw, ADR-0029).
pub(super) fn header_two_column(panel: &[Line<'static>], theme: &Theme) -> Vec<Line<'static>> {
    let gap = " ".repeat(HEADER_LOGO_GAP);
    // Zip the 6 logo rows against the 6 box rows into one row each. When the box
    // has fewer rows than the logo (never today - it is always 6), the extra logo
    // rows draw the logo alone; when it has more, the extra box rows draw alone.
    HEADER_LOGO
        .lines()
        .zip(panel)
        .map(|(logo, boxed)| {
            let mut spans = vec![
                Span::styled(logo.to_string(), accent_style(theme)),
                Span::raw(gap.clone()),
            ];
            spans.extend(boxed.spans.clone());
            Line::from(spans)
        })
        .collect()
}

/// The logo STACKED above the boxed info panel (the middle tier): the 6 accent
/// logo rows as a full-width TOP banner (each exactly [`HEADER_LOGO_WIDTH`] cols),
/// then the pre-built bordered `panel` box below it. Left-aligned to the content
/// gutter (no centering), so it lines up with the composer. Every logo row is 83
/// columns and every box row is `inner + 2` - both `<= content_width` in this tier
/// by construction (measure==draw, ADR-0029).
pub(super) fn header_stacked(panel: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = HEADER_LOGO
        .lines()
        .map(|logo| Line::from(Span::styled(logo.to_string(), accent_style(theme))))
        .collect();
    out.extend(panel);
    out
}

/// The `Tips: <tip>` line below the box (qwen `Tips`), in secondary, clipped to
/// `width` columns so it never soft-wraps (measure==draw, ADR-0029).
pub(super) fn header_tips_line(tip: &str, width: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        truncate_cols(&format!("Tips: {tip}"), width),
        secondary_style(theme),
    ))
}

/// Abbreviates a leading `$HOME` in `path` to `~` (qwen `tildeifyPath`); other
/// paths pass through unchanged. Reads the home directory from the environment at
/// this one edge, then delegates to the pure [`tildeify_with_home`] rewrite - so
/// the string logic is testable without touching process env (ADR-0019).
pub(super) fn tildeify_path(path: &str) -> String {
    let home = std::env::var_os("HOME").and_then(|h| h.into_string().ok());
    tildeify_with_home(path, home.as_deref())
}

/// The pure `~`-abbreviation of `path` against a known `home` (qwen `tildeifyPath`):
/// an exact-match home becomes `~`, a home-prefixed path keeps its `~`-rooted tail,
/// everything else (including no/empty home) passes through unchanged. Pure text,
/// no IO - the env read lives in [`tildeify_path`].
pub(super) fn tildeify_with_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path == home => "~".to_string(),
        Some(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
