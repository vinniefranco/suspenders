use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::lull;
use crate::ui::theme::Theme;

use super::pending::Anim;
use super::style::{secondary_style, tui_color};
use super::text::{text_rows, truncate_visual};

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Run is running.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many source rows of the live reasoning the rolling tail shows under the
/// `✦ Thinking` header (the short reasoning tail). Tunable.
const THINKING_TAIL_ROWS: usize = 3;

/// The milliseconds-per-second divisor used when converting `quiet_ticks` (each
/// tick is `TICK_MS` ms) into an elapsed-seconds figure for the lull timer.
const MILLIS_PER_SEC: u64 = 1_000;

/// The rolling reasoning tail shown while a Run streams: an animated
/// `✦ Thinking ⠋` header (the braille [`SPINNER`] advanced by the adapter's
/// tick - motion lives HERE at the reasoning header, not the status bar), then
/// the last [`THINKING_TAIL_ROWS`] VISUAL rows of the reasoning, indented two
/// columns under the header as a sub-block. Empty when nothing is streaming.
///
/// Bounded by VISUAL rows, not source rows: one long unwrapped reasoning line
/// soft-wraps to many rows, which would let the short reasoning tail grow
/// to fill the viewport. Each source row is truncated (with an `…` marker) to
/// the content width so it occupies exactly one visual row and the tail is a
/// hard `THINKING_TAIL_ROWS` cap - truncation, not re-wrapping, so this never
/// drifts from what the Paragraph paints (ADR-0029). `width` is the
/// `content_area` width the tail draws in.
///
/// Uncached on purpose: the tail's window is non-monotonic (older lines scroll
/// off as it grows), so the char-length key the settled streaming cache relies
/// on would not hold. A handful of `Line`s per frame is cheap.
pub(super) fn live_thinking_lines(
    thinking: &str,
    spinner: u64,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if thinking.is_empty() {
        return vec![];
    }
    let header_style = Style::default()
        .fg(tui_color(theme.thinking_header))
        .add_modifier(Modifier::ITALIC);
    let row_style = Style::default()
        .fg(tui_color(theme.thinking))
        .add_modifier(Modifier::ITALIC);
    let frame = SPINNER[(spinner as usize) % SPINNER.len()];
    let mut out = vec![Line::styled(format!("✦ Thinking {frame}"), header_style)];
    // The tail rows indent two columns, so their text budget is the content
    // width less that indent (never below 1).
    let row_width = (width as usize).saturating_sub(2).max(1);
    let rows = text_rows(thinking);
    let tail = &rows[rows.len().saturating_sub(THINKING_TAIL_ROWS)..];
    out.extend(
        tail.iter()
            .map(|row| Line::styled(format!("  {}", truncate_visual(row, row_width)), row_style)),
    );
    out
}

// The lull "waiting" row (`lull_visible`/`live_lull_lines`) was folded into
// [`spinner_line`] (ADR-0048): the LoadingIndicator shows whenever the Run is
// Running and keeps the lull scene as its phrase content, so the separate quiet-
// only row is gone. The lull clock + scenes ([`lull`]) still drive the phrase.

/// The `k` (thousand) grouping unit `format_token_count` divides by.
const TOKEN_K: u64 = 1_000;
/// The `m` (million) grouping unit: at/above it, `format_token_count` renders
/// `N.Nm` (qwen `value >= 1_000_000 -> (value/1_000_000).toFixed(1) + "m"`).
const TOKEN_M: u64 = 1_000_000;
/// The threshold at/above which `format_token_count` drops the decimal (`Nk`),
/// and below which it shows one decimal (`N.Nk`).
const TOKEN_K_DECIMAL_LIMIT: u64 = 10_000;
/// The hundredths divisor used to round a token count to one decimal `k`: `count
/// / 100` rounded, then `/ 10`, matches JS `(count/1000).toFixed(1)`.
const TOKEN_HUNDREDTHS: f64 = 100.0;
/// The tenths divisor completing the one-decimal `k` rounding.
const TOKEN_TENTHS: f64 = 10.0;

/// A compact token count (qwen `formatTokenCount`, statusLinePresets.ts:217): the
/// bare number under 1000, `N.Nk` (one decimal, rounded) from 1000 to 9999, `Nk`
/// (floored) from 10000 to 999999, and `N.Nm` (one decimal, rounded) at 1000000
/// and above (qwen `2_400_000 -> "2.4m"`). Used by the spinner's `↑ 1.2k tokens`
/// figure.
pub(super) fn format_token_count(count: u64) -> String {
    if count < TOKEN_K {
        return count.to_string();
    }
    if count < TOKEN_K_DECIMAL_LIMIT {
        // One decimal, ROUNDED (qwen's `.toFixed(1)` rounds 9999 -> "10.0k").
        let tenths = (count as f64 / TOKEN_HUNDREDTHS).round() / TOKEN_TENTHS;
        return format!("{tenths:.1}k");
    }
    if count < TOKEN_M {
        return format!("{}k", count / TOKEN_K);
    }
    // One decimal, ROUNDED (qwen `(value / 1_000_000).toFixed(1)`).
    let tenths = (count as f64 / (TOKEN_M as f64 / TOKEN_TENTHS)).round() / TOKEN_TENTHS;
    format!("{tenths:.1}m")
}

/// The in-flight facts the spinner line renders WITH (a Parameter Object so
/// [`spinner_line`] stays within the SRP param ceiling): the optional thought
/// `subject` (Phase-6 seam - wins over the lull phrase when `Some`, matching qwen
/// `thought?.subject || currentLoadingPhrase`), the optional live token `count`
/// (Phase-6 seam - shipped `None` to avoid per-frame jitter), and whether the
/// stream is `receiving` (streaming text non-empty - picks the `↑`/`↓` arrow).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SpinnerState<'a> {
    pub(super) subject: Option<&'a str>,
    pub(super) tokens: Option<u64>,
    pub(super) receiving: bool,
}

/// The running spinner line (qwen `LoadingIndicator.tsx`, ADR-0041/0048): a
/// braille [`SPINNER`] frame, the phrase (the current lull scene content - a
/// deliberate divergence from qwen's `usePhraseCycler`, kept for the whimsy; the
/// [`SpinnerState::subject`] wins when `Some`), then the cancel group
/// `(<elapsed> [· <arrow> <tokens> tokens] · esc to cancel)` in secondary.
/// paddingLeft 2. Every produced row is truncated to `width` so it stays one
/// visual row (measure==draw, ADR-0029). Empty when the lull is still settling
/// (no phrase yet).
pub(super) fn spinner_line(
    anim: Anim,
    state: SpinnerState<'_>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The phrase is the current lull scene; while the lull settles there is no
    // scene yet, so the spinner line waits too (the lull row's settle window).
    let Some(phrase) = state
        .subject
        .or_else(|| lull::frame(anim.quiet_ticks, anim.lull_seq))
    else {
        return vec![];
    };
    let glyph = SPINNER[(anim.spinner as usize) % SPINNER.len()];
    let secs = anim.quiet_ticks.saturating_mul(crate::ui::TICK_MS) / MILLIS_PER_SEC;
    let elapsed = lull::format_elapsed(secs);
    let arrow = if state.receiving { "↓" } else { "↑" };
    let tokens_part = state
        .tokens
        .map(|n| format!(" · {arrow} {} tokens", format_token_count(n)))
        .unwrap_or_default();
    let cancel = format!("({elapsed}{tokens_part} · esc to cancel)");

    let style = Style::default()
        .fg(tui_color(theme.lull))
        .add_modifier(Modifier::ITALIC);
    let secondary = secondary_style(theme);
    // paddingLeft 2, then `<glyph> <phrase>  <cancel>` - built span-by-span so the
    // cancel group reads secondary while the phrase reads the lull colour, then
    // truncated as a whole to one visual row.
    let text = format!("  {glyph} {phrase}  {cancel}");
    // The phrase+glyph fit first; if the whole line overflows, truncate it (the
    // rare narrow case) - the common case is well within width.
    if text.chars().count() <= width as usize {
        return vec![Line::from(vec![
            Span::styled(format!("  {glyph} {phrase}  "), style),
            Span::styled(cancel, secondary),
        ])];
    }
    vec![Line::styled(truncate_visual(&text, width as usize), style)]
}
