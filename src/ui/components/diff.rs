use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::ui::theme::Theme;
use crate::view_model::{DiffHunk, DiffSide};

use super::markdown_render::{CodeFragment, highlight_code};
use super::style::{diff_chrome_style, diff_side_fg, secondary_style, tui_color};
use super::text::{push_cols, truncate_cols};

/// Normalizes a diff line's raw code text for display: tabs become two spaces
/// (consistent with [`text_rows`]); an empty line stays empty (the tint band
/// fills it visibly, so no space-padding trick is needed as it was for a plain
/// [`Line`]).
pub(super) fn normalize_diff_text(text: &str) -> String {
    text.replace('\t', "  ")
}

// ---------------------------------------------------------------------------
// Diff rendering (ADR-0008): the first-class `Diff` item's two color sources
// stay split - the SEMANTIC tag (added/removed/context) becomes a full-width
// background TINT from the Theme's slots, and the LEXICAL syntect foreground
// layers over it. The `+`/`-`/context marker glyph is added here, never baked
// into the core's text. The same syntect machinery highlights markdown fences.
// ---------------------------------------------------------------------------

/// The marker glyph a diff line's [`DiffSide`] draws (ADR-0008): the adapter
/// adds it, so the change still reads on a non-truecolor terminal and when the
/// tint is subtle. Two cells wide, so the code text aligns across the sides.
fn diff_marker(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Added => "+ ",
        DiffSide::Removed => "- ",
        DiffSide::Context => "  ",
    }
}

/// The background tint a diff line's [`DiffSide`] paints (ADR-0008): added and
/// removed read their Theme `*_bg` slots; context is untinted. The tint is the
/// SEMANTIC meaning; the syntect fg layers over it.
fn diff_tint(side: DiffSide, theme: &Theme) -> Option<Color> {
    match side {
        DiffSide::Added => Some(tui_color(theme.added_bg)),
        DiffSide::Removed => Some(tui_color(theme.removed_bg)),
        DiffSide::Context => None,
    }
}

/// Renders a first-class `Diff` item (ADR-0008) into ratatui lines: the title,
/// then each hunk's optional `@@ … @@` header (muted italic, no marker or tint)
/// and its tagged code lines as a full-width tint band with the marker glyph and
/// the syntect foreground, then the muted `… N more lines` tail from
/// [`diff_elided_tail`] (the caller appends it, so this stays integration-only).
///
/// Each produced [`Line`] is truncated to `content_width` so the viewport's
/// `Wrap` never re-breaks it - `wrapped_count` then equals the drawn rows
/// (measure==draw, ADR-0029). The tint is a FULL-WIDTH band: every code row is
/// padded to `content_width` with a bg-filled span, so the stripe reaches the
/// right edge like GitHub's.
pub(super) fn diff_lines(
    lang: Option<&str>,
    hunks: &[DiffHunk],
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Integration (IOSP): the gutter width + each hunk's rows come from the
    // operations below; here we only join the hunk blocks with the `═` separator.
    let width = content_width as usize;
    let gutter_width = diff_gutter_width(hunks);
    let separator = Line::styled("═".repeat(width), diff_chrome_style(theme));
    let blocks: Vec<Vec<Line<'static>>> = hunks
        .iter()
        .map(|hunk| hunk_code_lines(hunk, lang, gutter_width, width, theme))
        .collect();
    join_blocks(blocks, separator)
}

/// Joins row `blocks` with a `separator` row between each (never before the first
/// or after the last) - the flatten-with-separator the diff hunk rule needs
/// without a branch inside the fold (qwen `═` U+2550 hunk rule, DiffRenderer.tsx
/// :272). Pure.
fn join_blocks(blocks: Vec<Vec<Line<'static>>>, separator: Line<'static>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, block) in blocks.into_iter().enumerate() {
        if i > 0 {
            out.push(separator.clone());
        }
        out.extend(block);
    }
    out
}

/// The line-number gutter width (columns) a diff draws: the digit count of the
/// largest line number any hunk reaches, floored at 1 (qwen DiffRenderer.tsx
/// :213-218). Parsed from each hunk's `@@ -old,_ +new,_ @@` header and the line
/// count that follows, so no core change is needed (render-side `@@` parse).
fn diff_gutter_width(hunks: &[DiffHunk]) -> usize {
    let mut max = 1u32;
    for hunk in hunks {
        let (old_start, new_start) = parse_hunk_header(hunk.header.as_deref());
        let (mut old_n, mut new_n) = (old_start, new_start);
        for line in &hunk.lines {
            match line.side {
                DiffSide::Context => {
                    max = max.max(new_n);
                    old_n += 1;
                    new_n += 1;
                }
                DiffSide::Added => {
                    max = max.max(new_n);
                    new_n += 1;
                }
                DiffSide::Removed => {
                    max = max.max(old_n);
                    old_n += 1;
                }
            }
        }
    }
    max.to_string().len().max(1)
}

/// Parses the `(old_start, new_start)` 1-based line numbers from a `@@ -a,b +c,d
/// @@` unified-diff header (qwen `hunkHeaderRegex`, DiffRenderer.tsx:29). A `None`
/// header (a created file) starts both at 1.
pub(super) fn parse_hunk_header(header: Option<&str>) -> (u32, u32) {
    let Some(header) = header else {
        return (1, 1);
    };
    // `@@ -old[,n] +new[,n] @@` — take the first number after `-` and after `+`.
    let field = |marker: char| -> u32 {
        header
            .split(marker)
            .nth(1)
            .and_then(|rest| rest.split([',', ' ']).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(1)
    };
    (field('-'), field('+'))
}

/// The muted `... last N lines hidden ...` tail a display-capped diff ends with,
/// or nothing when the cap elided nothing (`elided == 0`). Worded to match qwen's
/// overflow banner (DiffRenderer `MaxSizedBox` → `... N lines hidden ...`).
pub(super) fn diff_elided_tail(
    elided: usize,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if elided == 0 {
        return Vec::new();
    }
    vec![Line::styled(
        truncate_cols(
            &format!("... last {elided} lines hidden ..."),
            content_width as usize,
        ),
        diff_chrome_style(theme),
    )]
}

/// One hunk's code lines, syntect-highlighted two-pass so multi-line constructs
/// (a block comment, a raw string) color coherently across ALL their lines
/// (ADR-0008 recorded decision). The AFTER-image (context + added, in order) is
/// highlighted as ONE slice so syntect parse state carries; the BEFORE-image
/// (context + removed, in order) as another. A context line draws from the after
/// pass and advances both cursors; an added line draws from after; a removed
/// line from before - so a created file (one all-added hunk = the whole file)
/// colors its `/** … */` JSDoc as a comment across every line, not just line 1.
///
/// KNOWN LIMITATION (inherent to any before/after two-pass scheme): a multi-line
/// construct a single hunk STRADDLES via a removed opener and an added closer
/// (e.g. `/*` removed, `*/` added) can't color coherently - the two lines live
/// in different images. The common cases (whole created files, comments that
/// survive an edit as context) are coherent; a straddling rewrite is not.
fn hunk_code_lines(
    hunk: &DiffHunk,
    lang: Option<&str>,
    gutter_width: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The 1-based line numbers each row draws in the gutter, parsed from the
    // hunk header (render-side, no core change - qwen DiffRenderer.tsx:279-301).
    let numbers = hunk_line_numbers(hunk);
    // Normalize each line's text ONCE, in file order, then strip the common
    // leading indentation shared by every displayable line (qwen DiffRenderer.tsx
    // :225-241) so a deeply-nested edit still reads.
    let raw: Vec<String> = hunk
        .lines
        .iter()
        .map(|l| normalize_diff_text(&l.text))
        .collect();
    let strip = common_indent(&raw);
    let texts: Vec<String> = raw
        .iter()
        .map(|t| t.chars().skip(strip).collect())
        .collect();

    // The two images, in file order: added/context feed the after pass, and
    // removed/context the before pass, so syntect parse state carries per side.
    let image = |keep: fn(DiffSide) -> bool| -> Vec<&str> {
        hunk.lines
            .iter()
            .zip(&texts)
            .filter(|(l, _)| keep(l.side))
            .map(|(_, t)| t.as_str())
            .collect()
    };
    // Highlight each image as one slice (parse state carries) when a language
    // resolves; `None` (unknown/absent language) falls back to no fg fragments.
    let highlight =
        |refs: Vec<&str>| lang.and_then(|lang| highlight_code(&refs, lang, &theme.syntax));
    let after_fg = highlight(image(|s| matches!(s, DiffSide::Added | DiffSide::Context)));
    let before_fg = highlight(image(|s| {
        matches!(s, DiffSide::Removed | DiffSide::Context)
    }));

    let mut out = Vec::with_capacity(hunk.lines.len());
    let mut after_i = 0;
    let mut before_i = 0;
    for (line, text) in hunk.lines.iter().zip(&texts) {
        // Each line draws its fragments from the image it belongs to; a context
        // line draws from the after pass and advances BOTH cursors so the two
        // passes stay aligned to file order. Exhaustive over the three sides.
        let fragments = match line.side {
            DiffSide::Removed => {
                let fg = before_fg.as_ref().and_then(|f| f.get(before_i)).cloned();
                before_i += 1;
                fg
            }
            DiffSide::Added => {
                let fg = after_fg.as_ref().and_then(|f| f.get(after_i)).cloned();
                after_i += 1;
                fg
            }
            DiffSide::Context => {
                let fg = after_fg.as_ref().and_then(|f| f.get(after_i)).cloned();
                after_i += 1;
                before_i += 1;
                fg
            }
        };
        let gutter = diff_gutter_cell(numbers[out.len()], gutter_width);
        out.push(diff_code_row(
            line.side, &gutter, text, fragments, width, theme,
        ));
    }
    out
}

/// The per-row 1-based line numbers a hunk draws in its gutter, in display order:
/// a Context/Added row shows its NEW line number, a Removed row its OLD one (qwen
/// DiffRenderer.tsx:279-301). Parsed from the hunk header start numbers.
pub(super) fn hunk_line_numbers(hunk: &DiffHunk) -> Vec<u32> {
    let (mut old_n, mut new_n) = parse_hunk_header(hunk.header.as_deref());
    hunk.lines
        .iter()
        .map(|line| match line.side {
            DiffSide::Context => {
                let n = new_n;
                old_n += 1;
                new_n += 1;
                n
            }
            DiffSide::Added => {
                let n = new_n;
                new_n += 1;
                n
            }
            DiffSide::Removed => {
                let n = old_n;
                old_n += 1;
                n
            }
        })
        .collect()
}

/// The common leading-space count shared by every non-blank line (qwen strips it
/// per hunk so a deeply-indented edit still reads at the box edge). A hunk of
/// only blank lines strips nothing.
fn common_indent(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| l.chars().any(|c| !c.is_whitespace()))
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

/// One diff line-number gutter cell: the number right-aligned in `gutter_width`
/// columns plus a trailing space, the muted-italic diff chrome. The single place
/// the gutter's alignment lives.
fn diff_gutter_cell(number: u32, gutter_width: usize) -> String {
    format!("{number:>gutter_width$} ")
}

/// One diff code row as a full-width tint band: the untinted [`DIFF_INDENT`]
/// gutter, then the marker glyph (semantic fg - added green, removed red, so the
/// change reads without truecolor) and the code (syntect fg when highlighted,
/// else the semantic fg), all over the side's background tint, padded to `width`
/// so the band reaches the right edge. Widths are DISPLAY COLUMNS (a wide CJK or
/// emoji glyph counts 2), so the row occupies exactly `width` columns and the
/// viewport's `Wrap` never re-breaks it - measure==draw, and the tint band never
/// shatters across rows (ADR-0029).
fn diff_code_row(
    side: DiffSide,
    gutter: &str,
    text: &str,
    fragments: Option<Vec<CodeFragment>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let tint = diff_tint(side, theme);
    let semantic = Style::default().fg(diff_side_fg(side, theme));
    let band = |mut s: Style| {
        if let Some(bg) = tint {
            s = s.bg(bg);
        }
        s
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    // The line-number gutter (qwen DiffRenderer): the number tinted `text.secondary`
    // over the side's diff background, so the band starts at the gutter's left edge.
    let mut used = push_cols(&mut spans, gutter, band(secondary_style(theme)), 0, width);

    // The marker glyph carries the SEMANTIC fg over the tint.
    used = push_cols(&mut spans, diff_marker(side), band(semantic), used, width);

    // The code: syntect fg fragments over the tint, or the semantic fg when no
    // language highlighted this line.
    match fragments {
        Some(frags) if !frags.is_empty() => {
            for ((r, g, b), frag) in frags {
                used = push_cols(
                    &mut spans,
                    &frag,
                    band(Style::default().fg(Color::Rgb(r, g, b))),
                    used,
                    width,
                );
            }
        }
        _ => {
            used = push_cols(&mut spans, text, band(semantic), used, width);
        }
    }

    // Pad the band to the right edge so the tint reads full-width.
    if let Some(bg) = tint
        && used < width
    {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}
