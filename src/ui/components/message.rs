use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::theme::Theme;
use crate::view_model::TranscriptItem;

use super::header::{HeaderView, header_lines};
use super::markdown_render::markdown_lines;
use super::style::{accent_style, primary_style, secondary_style};
use super::text::{text_rows, wrap_words};
use super::tool_body::{marker_prefix_and_style, tool_inner_lines, tool_inner_width};

/// The grey style settled Thinking draws in (qwen `ThinkMessage`
/// `text.secondary`). No italic - qwen thoughts read as plain grey markdown.
fn thinking_style(theme: &Theme) -> Style {
    secondary_style(theme)
}

/// A settled Thinking item's lines (qwen `ThinkMessage`, ConversationMessages.tsx
/// :250): the grey `✦` U+2726 marker + grey markdown body, hung under the 2-col
/// prefix. qwen has NO per-thought collapse - a thought either shows in full or
/// is hidden entirely by compact mode (the show/hide decision is the caller's,
/// ADR-0052), so this always renders the full grey body.
pub(super) fn settled_thinking_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    prefixed_markdown_lines(
        "✦",
        thinking_style(theme),
        markdown_lines(text, theme)
            .into_iter()
            .map(|line| recolor_line(line, thinking_style(theme)))
            .collect(),
    )
}

/// Overrides every span's fg with `style`'s colour while keeping modifiers, so a
/// Thinking body reads uniformly grey (qwen colours the whole `ThinkMessage`
/// markdown `text.secondary`).
fn recolor_line(line: Line<'static>, style: Style) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(style)))
            .collect::<Vec<_>>(),
    )
}

/// The lines one Transcript item renders as. `Diff` is the first-class rich item
/// of the semantic display vocabulary (ADR-0008): a titled diff whose lines take
/// a semantic tint from their [`DiffSide`]'s Theme slots and a syntect foreground.
/// `compact` (Ctrl+O, qwen `compactMode`, the core's `Screen::compact_mode`) hides
/// settled `Thinking` items ENTIRELY and folds a tool RESULT body (a multi-line
/// `Diff`, or a `Todo` checklist) to its header row - keeping the transcript terse
/// (ADR-0052). `content_width` is the `content_area` width the lines draw in.
pub(super) fn message_lines(
    item: &TranscriptItem,
    compact: bool,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match item {
        // User prompt (qwen `UserMessage`, ConversationMessages.tsx:186): the
        // `>` U+003E caret + text both `text.accent`, hanging under a 2-col
        // prefix (`stringWidth(">")+1`). Multi-line input renders as many rows.
        TranscriptItem::User { text } => prefixed_text_lines(
            ">",
            accent_style(theme),
            text,
            accent_style(theme),
            content_width,
        ),
        // Assistant markdown (qwen `AssistantMessage`, ConversationMessages.tsx
        // :210): the `✦` U+2726 marker `text.accent` on row 0, the full markdown
        // body hanging under a 2-col prefix.
        TranscriptItem::Assistant { text } => {
            prefixed_markdown_lines("✦", accent_style(theme), markdown_lines(text, theme))
        }
        // Settled Thinking (qwen `ThinkMessage`, ConversationMessages.tsx:250):
        // the same `✦` U+2726 marker but `text.secondary` (grey) for BOTH glyph
        // and body. Compact mode HIDES it entirely (qwen `!compactMode`, ADR-0052:
        // show/hide, never a collapsed one-liner); otherwise the full grey body.
        TranscriptItem::Thinking { text } => {
            if compact {
                Vec::new()
            } else {
                settled_thinking_lines(text, theme)
            }
        }
        // Tool items render INSIDE the group box (qwen `ToolGroupMessage`); their
        // INNER content is built here at the box's inner width and wrapped with
        // borders at assembly by [`grouped_rows`]. Reached only via that path.
        // Under compact the RESULT body folds to the header row (qwen
        // `!compactMode || forceShowResult`).
        TranscriptItem::ToolCall { .. }
        | TranscriptItem::ToolResult { .. }
        | TranscriptItem::Diff { .. }
        | TranscriptItem::Todo { .. } => {
            tool_inner_lines(item, compact, tool_inner_width(content_width), theme)
        }
        // The startup banner (qwen `AppHeader` = `Header` + `Tips`): the ASCII
        // wordmark logo (accent) left, a single-border info panel right, and the
        // `Tips:` line below. Drawn at the FULL content width so the width gate
        // ([`header_lines`]) can decide whether the 83-col logo + gap + a minimum
        // info panel fits, hiding the logo when it does not.
        TranscriptItem::Header {
            title,
            version,
            model,
            cwd,
            tip,
        } => header_lines(
            &HeaderView {
                title,
                version,
                model,
                cwd,
                tip,
            },
            content_width,
            theme,
        ),
        // Info/notification (qwen `InfoMessage`, StatusMessages.tsx:64): the `●`
        // U+25CF prefix `text.primary`, body `text.primary`, hanging under a
        // 2-col prefix. A Marker tints its prefix + body by TONE alone.
        TranscriptItem::Info { text } => prefixed_text_lines(
            "●",
            primary_style(theme),
            text,
            primary_style(theme),
            content_width,
        ),
        // A harness Marker: the prefix glyph + tint chosen by the marker's
        // [`Tone`] (qwen StatusMessages set - Constrain reads the `△` warning
        // status, everything else the `●` info status). Tone alone decides,
        // never the text.
        TranscriptItem::Marker { .. } => {
            let (glyph, style) = marker_prefix_and_style(item, theme);
            prefixed_text_lines(glyph, style, marker_text(item), style, content_width)
        }
    }
}

/// The plain text an Info/Marker item carries (both are text rows, no markdown).
fn marker_text(item: &TranscriptItem) -> &str {
    match item {
        TranscriptItem::Info { text } | TranscriptItem::Marker { text, .. } => text,
        _ => "",
    }
}

/// The 2-column prefix width every single-glyph committed prefix hangs under
/// (qwen `getPrefixWidth = stringWidth(prefix) + 1`, ConversationMessages.tsx:90
/// / StatusMessages.tsx:44): one glyph column plus one clear column so the body
/// never touches the marker. All Phase-2 prefixes (`>`,`✦`,`●`) are width-1.
const PREFIX_WIDTH: usize = 2;

/// Lines for a prefixed PLAIN-TEXT item (qwen `PrefixedTextMessage`): the `glyph`
/// in `prefix_style` on row 0, then the wrapping text in `text_style` hung under
/// the [`PREFIX_WIDTH`] prefix column. Every produced [`Line`] is `<= content_width`
/// columns (the body wrapped to `content_width - PREFIX_WIDTH`, both prefix and
/// continuation padded to the prefix column), so the viewport's `Wrap` never
/// re-breaks it (measure==draw, ADR-0029).
fn prefixed_text_lines(
    glyph: &str,
    prefix_style: Style,
    text: &str,
    text_style: Style,
    content_width: u16,
) -> Vec<Line<'static>> {
    let inner = (content_width as usize).saturating_sub(PREFIX_WIDTH).max(1);
    let pad = " ".repeat(PREFIX_WIDTH);
    let mut out = Vec::new();
    let mut first = true;
    for source in text_rows(text) {
        for seg in wrap_words(&source, inner) {
            let lead = if first {
                Span::styled(format!("{glyph} "), prefix_style)
            } else {
                Span::raw(pad.clone())
            };
            out.push(Line::from(vec![lead, Span::styled(seg, text_style)]));
            first = false;
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(format!("{glyph} "), prefix_style)));
    }
    out
}

/// Lines for a prefixed MARKDOWN item (qwen `PrefixedMarkdownMessage`): the
/// `glyph` in `prefix_style` on the first body row, every row (row 0 and each
/// continuation) hung under the [`PREFIX_WIDTH`] prefix column. The markdown
/// `body` is already styled; this only prepends the marker/indent column. Because
/// the body was built at the reduced width by the cache, the prefixed lines stay
/// `<= content_width` (measure==draw, ADR-0029).
fn prefixed_markdown_lines(
    glyph: &str,
    prefix_style: Style,
    body: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let pad = " ".repeat(PREFIX_WIDTH);
    let mut first = true;
    body.into_iter()
        .map(|line| {
            let lead = if first {
                Span::styled(format!("{glyph} "), prefix_style)
            } else {
                Span::raw(pad.clone())
            };
            first = false;
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}
