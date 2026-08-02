//! Diff rendering + code-fence syntax highlighting + assistant markdown
//! (ADR-0008): the first-class `Diff` item's semantic tint / syntect fg split,
//! the shared syntect machinery that also colours markdown fences, and the
//! markdown-to-`Line` mapping. Split from the components god module by rendering
//! responsibility; shared text/column primitives (`push_cols`, `truncate_cols`,
//! `text_rows`) stay in the parent and arrive via `use super::*`.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;

use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::theme::{self, Theme};
use crate::view_model::{DiffHunk, DiffSide};

use super::style::{diff_chrome_style, diff_side_fg, md_style, secondary_style, tui_color};
use super::text::{push_cols, truncate_cols};

// ---------------------------------------------------------------------------
// Code-fence syntax highlighting (presentation, so it lives HERE - ADR-0008:
// markdown.rs carries only the semantic fact, the fence's language).
// ---------------------------------------------------------------------------

/// The bundled syntax definitions, lazy: headless runs that never render pay
/// nothing for the load. The syntect themes are NOT here - the theme module
/// owns that set ([`theme::syntax_theme_set`]), so the names its validation
/// accepts and the themes this highlighter draws from are one loaded copy.
pub(super) static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

pub(super) fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// One highlighted fragment: the `(r, g, b)` foreground and the text it colors.
pub(super) type CodeFragment = ((u8, u8, u8), String);

/// Highlights one code block with the named bundled syntect theme (the active
/// Theme's `syntax` slot): per input line, the [`CodeFragment`]s syntect
/// colors it with - pure data in/out, no ratatui types. `None` when `lang`
/// resolves to no bundled syntax (caller falls back to the plain
/// [`MdStyle::CodeBlock`] rendering). Parse state carries across the lines, so
/// multi-line constructs (block comments, raw strings) color correctly.
/// An unknown `syntax` name falls back to `base16-ocean.dark` - theme parsing
/// validates names (ADR-0038), so this is belt-and-suspenders, not a path.
pub(super) fn highlight_code(
    lines: &[&str],
    lang: &str,
    syntax_theme: &str,
) -> Option<Vec<Vec<CodeFragment>>> {
    let syntaxes = syntaxes();
    // `find_syntax_by_token` matches the syntax name ("rust", "python") AND
    // file extensions ("rs", "py"), case-insensitively - the widest net for
    // fence tags.
    let syntax = syntaxes.find_syntax_by_token(lang)?;
    let themes = &theme::syntax_theme_set().themes;
    let colors = themes
        .get(syntax_theme)
        .unwrap_or(&themes["base16-ocean.dark"]);
    let mut state = HighlightLines::new(syntax, colors);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // The newlines-variant SyntaxSet expects each line `\n`-terminated.
        let with_newline = format!("{line}\n");
        let ranges = state.highlight_line(&with_newline, syntaxes).ok()?;
        let mut fragments = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            let fg = style.foreground;
            fragments.push(((fg.r, fg.g, fg.b), text.to_string()));
        }
        out.push(fragments);
    }
    Some(out)
}

/// The inset prefix a bare code block indents under: two
/// columns, wearing the code background so the block reads as one solid inset
/// surface rather than a boxed one.
pub(super) const CODE_INSET: &str = "  ";

/// Renders assistant markdown into ratatui lines: one `Line` per [`MdLine`],
/// each span styled by the single [`md_style`] mapping; an empty MdLine (block
/// separation) becomes a blank row. Consecutive code lines sharing a non-empty
/// `code_lang` render as one bare, inset code block (a blank row above/below,
/// each row inset under [`CODE_INSET`], no box or gutter): [`highlight_code`]
/// gives syntect fg over OUR code background; blocks with no/unknown language
/// fall back to the plain CodeBlock style, still inset.
pub(super) fn markdown_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let md_lines = markdown::to_lines(text);
    let mut out = Vec::with_capacity(md_lines.len());
    let mut i = 0;
    while i < md_lines.len() {
        // Prose (`code_lang == None`) takes the per-line plain path; ANY fenced
        // code - including a bare ``` fence (`Some("")`, which local models emit
        // constantly) - enters the inset code-block branch below. An empty lang
        // simply won't resolve a syntax, so it falls to the plain-but-inset
        // fallback inside the branch, framed like every other code block.
        let lang = match md_lines[i].code_lang.as_deref() {
            Some(lang) => lang.to_string(),
            None => {
                out.push(plain_md_line(&md_lines[i], theme));
                i += 1;
                continue;
            }
        };
        let mut end = i;
        while end < md_lines.len() && md_lines[end].code_lang.as_deref() == Some(lang.as_str()) {
            end += 1;
        }
        let block = &md_lines[i..end];
        let texts: Vec<String> = block.iter().map(md_line_text).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        // Bare, inset code block: a blank row above and
        // below frames the block, and each code row insets under
        // [`CODE_INSET`]; no box, no line-number gutter - the syntect fg over
        // our code bg carries it. The inset prefix wears the code bg so the
        // block reads as one solid surface.
        let code_bg = tui_color(theme.code_block_bg);
        let inset = || Span::styled(CODE_INSET, Style::default().bg(code_bg));
        out.push(Line::default());
        match highlight_code(&refs, &lang, &theme.syntax) {
            Some(highlighted) => {
                for (fragments, text) in highlighted.into_iter().zip(&texts) {
                    if fragments.is_empty() {
                        // Blank (or all-whitespace) code line: keep the same
                        // bg treatment the plain path gives it, still inset.
                        out.push(Line::from(vec![
                            inset(),
                            Span::styled(text.clone(), md_style(MdStyle::CodeBlock, theme)),
                        ]));
                    } else {
                        let mut spans = vec![inset()];
                        spans.extend(fragments.into_iter().map(|((r, g, b), text)| {
                            Span::styled(text, Style::default().fg(Color::Rgb(r, g, b)).bg(code_bg))
                        }));
                        out.push(Line::from(spans));
                    }
                }
            }
            // Unknown language: the plain CodeBlock rendering, still inset.
            None => out.extend(block.iter().map(|line| {
                let mut spans = vec![inset()];
                spans.extend(
                    line.spans
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), md_style(span.style, theme))),
                );
                Line::from(spans)
            })),
        }
        out.push(Line::default());
        i = end;
    }
    out
}

/// One [`MdLine`] rendered the plain way: each span through the single
/// [`md_style`] mapping.
pub(super) fn plain_md_line(line: &MdLine, theme: &Theme) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), md_style(span.style, theme)))
            .collect::<Vec<_>>(),
    )
}

/// One MdLine's concatenated text (code lines carry a single span, but this
/// stays correct regardless).
pub(super) fn md_line_text(line: &MdLine) -> String {
    line.spans.iter().map(|s| s.text.as_str()).collect()
}

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
pub(super) fn diff_marker(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Added => "+ ",
        DiffSide::Removed => "- ",
        DiffSide::Context => "  ",
    }
}

/// The background tint a diff line's [`DiffSide`] paints (ADR-0008): added and
/// removed read their Theme `*_bg` slots; context is untinted. The tint is the
/// SEMANTIC meaning; the syntect fg layers over it.
pub(super) fn diff_tint(side: DiffSide, theme: &Theme) -> Option<Color> {
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
pub(super) fn join_blocks(
    blocks: Vec<Vec<Line<'static>>>,
    separator: Line<'static>,
) -> Vec<Line<'static>> {
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
pub(super) fn diff_gutter_width(hunks: &[DiffHunk]) -> usize {
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
pub(super) fn hunk_code_lines(
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
            DiffCell {
                side: line.side,
                gutter: &gutter,
                text,
                fragments,
            },
            width,
            theme,
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
pub(super) fn common_indent(lines: &[String]) -> usize {
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
pub(super) fn diff_gutter_cell(number: u32, gutter_width: usize) -> String {
    format!("{number:>gutter_width$} ")
}

/// One diff code row's content (Parameter Object): the semantic [`DiffSide`], the
/// already-formatted line-number `gutter` cell, the indent-stripped code `text`,
/// and the optional syntect `fragments` that colour it. Bundled so
/// [`diff_code_row`] takes the cell content as one value beside the `width`/`theme`
/// presentation inputs (the four travel together from [`hunk_code_lines`]).
pub(super) struct DiffCell<'a> {
    side: DiffSide,
    gutter: &'a str,
    text: &'a str,
    fragments: Option<Vec<CodeFragment>>,
}

/// One diff code row as a full-width tint band: the untinted [`DIFF_INDENT`]
/// gutter, then the marker glyph (semantic fg - added green, removed red, so the
/// change reads without truecolor) and the code (syntect fg when highlighted,
/// else the semantic fg), all over the side's background tint, padded to `width`
/// so the band reaches the right edge. Widths are DISPLAY COLUMNS (a wide CJK or
/// emoji glyph counts 2), so the row occupies exactly `width` columns and the
/// viewport's `Wrap` never re-breaks it - measure==draw, and the tint band never
/// shatters across rows (ADR-0029).
pub(super) fn diff_code_row(cell: DiffCell<'_>, width: usize, theme: &Theme) -> Line<'static> {
    let DiffCell {
        side,
        gutter,
        text,
        fragments,
    } = cell;
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
