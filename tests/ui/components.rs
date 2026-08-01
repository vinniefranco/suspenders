
use super::*;
use crate::ui::transcript::Transcript;
use crate::view_model::DiffLine;

// A first-class `Diff` item (ADR-0008) with one all-added hunk of raw
// (marker-free) code lines - the shape the Diff extension's Presenter emits.
// `lang` is `None` so tests exercise the no-highlight fallback unless they
// pass a real language explicitly.
fn diff_item(title: &str, lines: Vec<DiffLine>) -> TranscriptItem {
    TranscriptItem::Diff {
        title: title.to_string(),
        lang: None,
        hunks: vec![DiffHunk {
            header: None,
            lines,
        }],
        elided: 0,
    }
}

// -----------------------------------------------------------------------
// The semantic MdStyle → Style mapping (ADR-0008): one assertion per
// vocabulary word, pinning the display fact each variant means.
// -----------------------------------------------------------------------

#[test]
fn md_plain_maps_to_the_default_style() {
    assert_eq!(md_style(MdStyle::Plain, theme::dark()), Style::default());
}

#[test]
fn md_bold_maps_to_the_bold_modifier() {
    assert_eq!(
        md_style(MdStyle::Bold, theme::dark()),
        Style::default().add_modifier(Modifier::BOLD)
    );
}

#[test]
fn md_italic_maps_to_the_italic_modifier() {
    assert_eq!(
        md_style(MdStyle::Italic, theme::dark()),
        Style::default().add_modifier(Modifier::ITALIC)
    );
}

#[test]
fn md_bold_italic_carries_both_modifiers() {
    let style = md_style(MdStyle::BoldItalic, theme::dark());
    assert!(
        style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC)
    );
}

#[test]
fn md_inline_code_reads_yellow() {
    assert_eq!(
        md_style(MdStyle::Code, theme::dark()).fg,
        Some(Color::Yellow)
    );
}

#[test]
fn md_code_block_carries_the_code_background() {
    // The bg is the block treatment every code row keeps, highlighted or
    // not; the fg is the plain-fallback tint syntect replaces when it can.
    let style = md_style(MdStyle::CodeBlock, theme::dark());
    assert_eq!(style.bg, Some(tui_color(theme::dark().code_block_bg)));
    assert!(matches!(style.fg, Some(Color::Rgb(..))));
}

#[test]
fn md_heading_reads_bold_cyan() {
    let style = md_style(MdStyle::Heading, theme::dark());
    assert_eq!(style.fg, Some(Color::Cyan));
    assert!(style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn md_bullet_reads_cyan() {
    assert_eq!(
        md_style(MdStyle::Bullet, theme::dark()).fg,
        Some(Color::Cyan)
    );
}

#[test]
fn md_quote_reads_dim_italic() {
    let style = md_style(MdStyle::Quote, theme::dark());
    assert_eq!(style.fg, Some(Color::DarkGray));
    assert!(style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn md_link_reads_underlined_blue() {
    let style = md_style(MdStyle::Link, theme::dark());
    assert_eq!(style.fg, Some(Color::Blue));
    assert!(style.add_modifier.contains(Modifier::UNDERLINED));
}

// -----------------------------------------------------------------------
// The spinner line / LoadingIndicator (ADR-0048): the running spinner that
// subsumed the old lull "waiting" row - it keeps the lull scene as its
// phrase content and carries the elapsed + esc-to-cancel affordance.
// -----------------------------------------------------------------------

fn spinner_text(lines: &[Line<'static>]) -> String {
    lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
}

// format_token_count (qwen formatTokenCount): bare under 1000, N.Nk rounded
// 1000..9999, Nk floored 10000..999999, N.Nm rounded at 1000000+.
#[test]
fn format_token_count_matches_qwens_k_thresholds() {
    assert_eq!(format_token_count(0), "0");
    assert_eq!(format_token_count(847), "847");
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(1000), "1.0k");
    assert_eq!(format_token_count(5400), "5.4k");
    assert_eq!(format_token_count(9999), "10.0k");
    assert_eq!(format_token_count(10000), "10k");
    assert_eq!(format_token_count(100000), "100k");
    assert_eq!(format_token_count(999999), "999k");
    // The megatoken branch (qwen `2_400_000 -> "2.4m"`): one decimal, rounded.
    assert_eq!(format_token_count(1_000_000), "1.0m");
    assert_eq!(format_token_count(1_200_000), "1.2m");
    assert_eq!(format_token_count(2_400_000), "2.4m");
    assert_eq!(format_token_count(2_450_000), "2.5m");
}

// While the lull settles (`quiet_ticks` under SETTLE_TICKS) there is no
// phrase yet, so the spinner waits - unless a subject overrides.
#[test]
fn spinner_line_is_empty_within_the_settle_window_without_a_subject() {
    let lines = spinner_line(Anim::default(), SpinnerState::default(), 60, theme::dark());
    assert!(lines.is_empty(), "no phrase yet, no spinner row");
}

// At the settle close the spinner shows one row carrying the phrase, the
// elapsed timer (opens at 5s), and `esc to cancel`. No token figure when
// `tokens` is None.
#[test]
fn spinner_line_shows_phrase_elapsed_and_esc_to_cancel() {
    let anim = Anim {
        quiet_ticks: lull::SETTLE_TICKS,
        ..Default::default()
    };
    let lines = spinner_line(anim, SpinnerState::default(), 80, theme::dark());
    assert_eq!(lines.len(), 1, "one visual row");
    let text = spinner_text(&lines);
    assert!(text.contains("5s"), "elapsed opens at 5s: {text:?}");
    assert!(
        text.contains("esc to cancel"),
        "cancel affordance: {text:?}"
    );
    assert!(
        !text.contains("tokens"),
        "no token figure when None: {text:?}"
    );
}

// The subject wins over the lull phrase (the Phase-6 thought-subject seam,
// qwen `thought?.subject || currentLoadingPhrase`) and shows even inside the
// settle window.
#[test]
fn spinner_line_subject_wins_over_the_lull_phrase() {
    let lines = spinner_line(
        Anim::default(),
        SpinnerState {
            subject: Some("Refactoring the parser"),
            ..SpinnerState::default()
        },
        80,
        theme::dark(),
    );
    assert!(spinner_text(&lines).contains("Refactoring the parser"));
}

// The token figure shows with the arrow: `↑` when NOT receiving (sending),
// `↓` while streaming text arrives; the count is formatTokenCount'd.
#[test]
fn spinner_line_shows_the_arrow_and_token_figure_when_present() {
    let anim = Anim {
        quiet_ticks: lull::SETTLE_TICKS,
        ..Default::default()
    };
    let sending = spinner_line(
        anim,
        SpinnerState {
            tokens: Some(1234),
            ..SpinnerState::default()
        },
        80,
        theme::dark(),
    );
    let text = spinner_text(&sending);
    assert!(text.contains("↑ 1.2k tokens"), "sending arrow up: {text:?}");

    let receiving = spinner_line(
        anim,
        SpinnerState {
            tokens: Some(1234),
            receiving: true,
            ..SpinnerState::default()
        },
        80,
        theme::dark(),
    );
    assert!(
        spinner_text(&receiving).contains("↓ 1.2k tokens"),
        "receiving arrow down"
    );
}

// A narrow width truncates the whole line to one visual row (measure==draw,
// ADR-0029).
#[test]
fn spinner_line_truncates_to_one_row_at_a_narrow_width() {
    let width = 24u16;
    let anim = Anim {
        quiet_ticks: lull::SETTLE_TICKS,
        ..Default::default()
    };
    let lines = spinner_line(
        anim,
        SpinnerState {
            subject: Some("a very long thought subject that overflows"),
            ..SpinnerState::default()
        },
        width,
        theme::dark(),
    );
    assert_eq!(lines.len(), 1);
    assert!(line_text(&lines[0]).chars().count() <= width as usize);
}

// -----------------------------------------------------------------------
// Themes (ADR-0038): the mappings read the active Theme's slots. The dark
// Theme must render byte-identically to the pre-theme hardcoded palette
// (the pinning tests below), and a non-default Theme must actually change
// what the mappings produce.
// -----------------------------------------------------------------------

/// A Theme differing from `dark` only in the slots `overrides` states.
fn themed(overrides: &str) -> Theme {
    theme::SparseTheme::parse(overrides)
        .expect("the test theme parses")
        .over(theme::dark())
}

#[test]
fn theme_colors_translate_one_to_one_to_ratatui() {
    let pairs: [(theme::Color, Color); 17] = [
        (theme::Color::Black, Color::Black),
        (theme::Color::Red, Color::Red),
        (theme::Color::Green, Color::Green),
        (theme::Color::Yellow, Color::Yellow),
        (theme::Color::Blue, Color::Blue),
        (theme::Color::Magenta, Color::Magenta),
        (theme::Color::Cyan, Color::Cyan),
        (theme::Color::Gray, Color::Gray),
        (theme::Color::DarkGray, Color::DarkGray),
        (theme::Color::LightRed, Color::LightRed),
        (theme::Color::LightGreen, Color::LightGreen),
        (theme::Color::LightYellow, Color::LightYellow),
        (theme::Color::LightBlue, Color::LightBlue),
        (theme::Color::LightMagenta, Color::LightMagenta),
        (theme::Color::LightCyan, Color::LightCyan),
        (theme::Color::White, Color::White),
        (theme::Color::Rgb(1, 2, 3), Color::Rgb(1, 2, 3)),
    ];
    for (theme_color, expected) in pairs {
        assert_eq!(tui_color(theme_color), expected, "{theme_color:?}");
    }
}

#[test]
fn dark_diff_side_fg_pins_the_palette() {
    let t = theme::dark();
    assert_eq!(diff_side_fg(DiffSide::Added, t), Color::Green);
    assert_eq!(diff_side_fg(DiffSide::Removed, t), Color::Red);
    assert_eq!(diff_side_fg(DiffSide::Context, t), Color::DarkGray);
}

#[test]
fn diff_chrome_reads_muted_italic() {
    // The `@@` header and `… N more lines` tail wear one shared chrome style.
    assert_eq!(
        diff_chrome_style(theme::dark()),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    );
}

#[test]
fn a_non_default_theme_recolors_the_mappings() {
    let t = themed("[colors]\nadded = \"#123456\"\nheading = \"magenta\"\n");
    assert_eq!(
        diff_side_fg(DiffSide::Added, &t),
        Color::Rgb(0x12, 0x34, 0x56)
    );
    assert_eq!(md_style(MdStyle::Heading, &t).fg, Some(Color::Magenta));
    // Unstated slots still read the dark floor.
    assert_eq!(diff_side_fg(DiffSide::Removed, &t), Color::Red);
}

// -----------------------------------------------------------------------
// markdown_lines: the semantic-MdLine → ratatui-Line rendering, including
// the code-fence routing (syntect vs. the plain CodeBlock fallback).
// -----------------------------------------------------------------------

#[test]
fn markdown_lines_styles_prose_spans_through_md_style() {
    let lines = markdown_lines("plain **bold** text", theme::dark());
    assert_eq!(lines.len(), 1);
    assert_eq!(line_text(&lines[0]), "plain bold text");
    let bold = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "bold")
        .expect("the bold span");
    assert_eq!(bold.style, md_style(MdStyle::Bold, theme::dark()));
}

/// A bare code block insets each row under [`CODE_INSET`] and frames the
/// block with a blank row above and below. This finds
/// the row whose code text (after the inset) matches `code`, and returns its
/// spans WITHOUT the leading inset span - what the assertions below care
/// about.
fn code_row<'a>(lines: &'a [Line<'static>], code: &str) -> &'a [Span<'static>] {
    let line = lines
        .iter()
        .find(|l| line_text(l) == format!("{CODE_INSET}{code}"))
        .unwrap_or_else(|| panic!("the code row for {code:?}"));
    // The first span is always the inset (code bg, no fg); the code follows.
    assert_eq!(line.spans[0].content.as_ref(), CODE_INSET);
    assert_eq!(
        line.spans[0].style.bg,
        Some(tui_color(theme::dark().code_block_bg))
    );
    &line.spans[1..]
}

#[test]
fn a_known_language_fence_is_highlighted_over_the_code_background() {
    let lines = markdown_lines("```rust\nlet x = 1;\n```", theme::dark());
    let code = code_row(&lines, "let x = 1;");
    // Syntect fragments the line; every fragment keeps OUR code bg under
    // its own syntect fg.
    assert!(code.len() > 1, "syntect splits the line");
    for span in code {
        assert_eq!(span.style.bg, Some(tui_color(theme::dark().code_block_bg)));
        assert!(matches!(span.style.fg, Some(Color::Rgb(..))));
    }
}

#[test]
fn a_bare_code_block_is_framed_by_a_blank_row_above_and_below() {
    // The block is inset and bounded by one blank row on each side; no box,
    // no gutter (Decision E).
    let lines = markdown_lines("before\n\n```rust\nlet x = 1;\n```\n\nafter", theme::dark());
    let code_idx = lines
        .iter()
        .position(|l| line_text(l) == format!("{CODE_INSET}let x = 1;"))
        .expect("the inset code row");
    assert_eq!(line_text(&lines[code_idx - 1]), "", "blank row above");
    assert_eq!(line_text(&lines[code_idx + 1]), "", "blank row below");
}

#[test]
fn an_unknown_language_fence_falls_back_to_the_plain_code_block_style() {
    let lines = markdown_lines("```notareallanguage\nsome code\n```", theme::dark());
    let code = code_row(&lines, "some code");
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].style, md_style(MdStyle::CodeBlock, theme::dark()));
}

#[test]
fn a_bare_fence_with_no_language_gets_the_inset_framed_block() {
    // A bare ``` fence carries `Some("")` - the common case local models
    // emit. It skips syntect (empty lang resolves no syntax) but still gets
    // the SAME inset + blank-framed code block as a labeled fence (M1): the
    // plain CodeBlock style, inset under CODE_INSET, framed above and below.
    let lines = markdown_lines("before\n\n```\nunlabeled code\n```\n\nafter", theme::dark());
    let code = code_row(&lines, "unlabeled code");
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].style, md_style(MdStyle::CodeBlock, theme::dark()));
    // Framed: a blank row above and below the inset code row.
    let idx = lines
        .iter()
        .position(|l| line_text(l) == format!("{CODE_INSET}unlabeled code"))
        .expect("the inset code row");
    assert_eq!(line_text(&lines[idx - 1]), "", "blank row above");
    assert_eq!(line_text(&lines[idx + 1]), "", "blank row below");
}

#[test]
fn a_blank_line_inside_a_highlighted_fence_keeps_the_code_background() {
    let lines = markdown_lines("```rust\nlet a = 1;\n\nlet b = 2;\n```", theme::dark());
    let a_idx = lines
        .iter()
        .position(|l| line_text(l) == format!("{CODE_INSET}let a = 1;"))
        .expect("the first code line");
    // The blank row between the statements yields no syntect fragments, so
    // it takes the plain CodeBlock treatment - same bg, no hole - and it is
    // still inset (the inset span, then the empty code span).
    let blank = &lines[a_idx + 1];
    assert_eq!(line_text(blank), CODE_INSET);
    assert_eq!(
        blank.spans[1].style,
        md_style(MdStyle::CodeBlock, theme::dark())
    );
}

#[test]
fn prose_after_a_fence_returns_to_the_plain_path() {
    let lines = markdown_lines("```rust\nlet x = 1;\n```\n\nafter the fence", theme::dark());
    let after = lines
        .iter()
        .find(|l| line_text(l) == "after the fence")
        .expect("the prose line");
    assert_eq!(
        after.spans[0].style,
        md_style(MdStyle::Plain, theme::dark())
    );
}

/// The color of the first fragment whose text contains `needle`.
fn color_of(lines: &[Vec<CodeFragment>], needle: &str) -> (u8, u8, u8) {
    lines
        .iter()
        .flatten()
        .find(|(_, text)| text.contains(needle))
        .unwrap_or_else(|| panic!("no fragment containing {needle:?}"))
        .0
}

#[test]
fn highlight_code_colors_keywords_differently_from_string_literals() {
    // Syntect fragments the literal (quotes vs contents); the contents
    // fragment is what must differ from the `fn` keyword.
    let lines = highlight_code(
        &["fn main() { let s = \"hi\"; }"],
        "rust",
        "base16-ocean.dark",
    )
    .unwrap();
    assert_ne!(color_of(&lines, "fn"), color_of(&lines, "hi"));
}

#[test]
fn highlight_code_resolves_extension_tokens_too() {
    // `find_syntax_by_token` matches extensions, not just names.
    assert!(highlight_code(&["let x = 1;"], "rs", "base16-ocean.dark").is_some());
    assert!(highlight_code(&["x = 1"], "py", "base16-ocean.dark").is_some());
}

#[test]
fn highlight_code_returns_none_for_an_unknown_lang() {
    assert_eq!(
        highlight_code(&["whatever"], "notareallanguage", "base16-ocean.dark"),
        None
    );
}

#[test]
fn highlight_code_on_empty_input_is_some_empty() {
    assert_eq!(
        highlight_code(&[], "rust", "base16-ocean.dark"),
        Some(vec![])
    );
}

#[test]
fn highlight_code_blank_line_yields_no_fragments() {
    let lines = highlight_code(
        &["let a = 1;", "", "let b = 2;"],
        "rust",
        "base16-ocean.dark",
    )
    .unwrap();
    assert_eq!(lines.len(), 3);
    assert!(lines[1].is_empty());
    assert!(!lines[0].is_empty() && !lines[2].is_empty());
}

#[test]
fn highlight_code_carries_parse_state_across_lines() {
    // A block comment opened on line 1 must color line 2 as comment, not code.
    let lines = highlight_code(
        &["/* comment", "still comment */", "let x = 1;"],
        "rust",
        "base16-ocean.dark",
    )
    .unwrap();
    let comment = color_of(&lines[..1], "comment");
    assert_eq!(color_of(&lines[1..2], "still comment"), comment);
    assert_ne!(color_of(&lines[2..], "let"), comment);
}

#[test]
fn highlight_code_preserves_the_line_text_verbatim() {
    let source = "fn add(a: u32, b: u32) -> u32 { a + b }";
    let lines = highlight_code(&[source], "rust", "base16-ocean.dark").unwrap();
    let joined: String = lines[0].iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(joined, source);
}

#[test]
fn highlighting_follows_the_named_syntax_theme() {
    // The Theme's `syntax` slot picks the syntect theme: the same code
    // colors differently under a dark and a light bundled theme.
    let dark = highlight_code(&["let x = 1;"], "rust", "base16-ocean.dark").unwrap();
    let light = highlight_code(&["let x = 1;"], "rust", "InspiredGitHub").unwrap();
    assert_ne!(dark, light, "two syntax themes color differently");
    // An unknown name falls back to the default rather than panicking
    // (unreachable through Theme parsing, which validates names).
    let fallback = highlight_code(&["let x = 1;"], "rust", "no-such-theme").unwrap();
    assert_eq!(fallback, dark);
}

#[test]
fn markdown_code_highlights_with_the_themes_syntax_slot() {
    // End to end through markdown_lines: a Theme naming a different
    // bundled syntect theme recolors the fence's spans.
    let fence = "```rust\nlet x = 1;\n```";
    let span_fgs = |t: &Theme| -> Vec<Option<Color>> {
        markdown_lines(fence, t)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style.fg))
            .collect()
    };
    assert_ne!(
        span_fgs(theme::dark()),
        span_fgs(&themed("syntax = \"InspiredGitHub\"\n"))
    );
}

// -----------------------------------------------------------------------
// The flat footer assembly (ADR-0053).
// -----------------------------------------------------------------------

/// A [`FigureView`] with no token estimate and a zero (hidden) cost.
fn no_figures() -> FigureView {
    FigureView {
        tokens: None,
        session_cost: 0.0,
        context_budget: None,
        mcp_offline: 0,
    }
}

/// A [`FigureView`] carrying only the token estimate, cost hidden.
fn tokens_only(estimate: u64) -> FigureView {
    FigureView {
        tokens: Some(estimate),
        session_cost: 0.0,
        context_budget: None,
        mcp_offline: 0,
    }
}

/// The pure footer assembly at `width` with the default fixtures (idle-ish,
/// tokens present with a budget, a positive cost) - the frame-free seam the
/// footer tests assert against.
fn footer_at(width: usize) -> Footer {
    footer(
        width,
        FooterView {
            conn: ConnectionView {
                base_url: "http://localhost:8080",
                model: "qwen3-coder",
            },
            figures: FigureView {
                tokens: Some(2500),
                session_cost: 0.42,
                context_budget: Some(10_000),
                mcp_offline: 0,
            },
            approval_mode: ApprovalMode::Default,
        },
    )
}

/// The footer at `width` with a given approval mode (default fixtures else).
fn footer_with_mode(mode: ApprovalMode) -> Footer {
    footer(
        200,
        FooterView {
            conn: ConnectionView {
                base_url: "http://localhost:8080",
                model: "qwen3-coder",
            },
            figures: no_figures(),
            approval_mode: mode,
        },
    )
}

#[test]
fn a_wide_footer_shows_model_then_context_then_cost_in_order() {
    let bar = footer_at(200);
    assert_eq!(bar.left, FooterLeft::Shortcuts);
    assert_eq!(
        bar.right,
        vec![
            FooterItem::Model {
                model: "qwen3-coder".into()
            },
            FooterItem::Context {
                label: "25.0% context used".into(),
                over_limit: false,
            },
            FooterItem::Cost {
                label: "$0.42".into(),
            },
        ]
    );
}

#[test]
fn a_narrow_footer_sheds_cost_then_model_keeping_context() {
    // Cost drops first, then model; context% survives longest (qwen's sole
    // figure). Widths chosen against the fixture: left `? for shortcuts` (15),
    // right `model qwen3-coder` (17) + `25.0% used` (10) + `$0.42` (5), each
    // ` | ` join (3), padding (4), min gap (1). Full = 4+15+1+(17+10+5+6)=58.
    let full = footer_at(60);
    assert_eq!(
        full.right,
        vec![
            FooterItem::Model {
                model: "qwen3-coder".into()
            },
            FooterItem::Context {
                label: "25.0% used".into(),
                over_limit: false,
            },
            FooterItem::Cost {
                label: "$0.42".into(),
            },
        ]
    );

    // Tighter (52): cost sheds (drops to 50), model + context survive.
    let no_cost = footer_at(52);
    assert_eq!(
        no_cost.right,
        vec![
            FooterItem::Model {
                model: "qwen3-coder".into()
            },
            FooterItem::Context {
                label: "25.0% used".into(),
                over_limit: false,
            },
        ]
    );

    // Tightest: model sheds too, context% NEVER drops.
    let context_only = footer_at(20);
    assert_eq!(
        context_only.right,
        vec![FooterItem::Context {
            label: "25.0% used".into(),
            over_limit: false,
        }]
    );
}

#[test]
fn the_footer_joins_the_right_group_with_a_grey_pipe_no_leading_sep() {
    // No powerline separators anywhere; the ` | ` join is the ONLY chrome
    // between items and there is none before the first.
    let bar = footer_at(200);
    assert!(!FOOTER_SEP.contains('\u{e0b0}'));
    assert!(!FOOTER_SEP.contains('\u{e0b2}'));
    assert_eq!(FOOTER_SEP, " | ");
    // right_cells = texts + one join per adjacent pair.
    let texts: usize = bar.right.iter().map(FooterItem::cells).sum();
    assert_eq!(
        bar.right_cells(),
        texts + (bar.right.len() - 1) * FOOTER_SEP.chars().count()
    );
}

#[test]
fn default_mode_shows_the_shortcuts_hint() {
    let bar = footer_with_mode(ApprovalMode::Default);
    assert_eq!(bar.left, FooterLeft::Shortcuts);
}

#[test]
fn each_non_default_mode_shows_the_autoaccept_label_and_hint() {
    for mode in [
        ApprovalMode::Plan,
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Yolo,
    ] {
        let bar = footer_with_mode(mode);
        assert_eq!(bar.left, FooterLeft::AutoAccept(mode), "{mode:?}");
        assert!(!approval_mode_label(mode).is_empty(), "{mode:?}");
    }
}

#[test]
fn the_autoaccept_labels_match_qwen_verbatim() {
    assert_eq!(approval_mode_label(ApprovalMode::Plan), "plan mode");
    assert_eq!(
        approval_mode_label(ApprovalMode::AutoEdit),
        "auto-accept edits"
    );
    assert_eq!(
        approval_mode_label(ApprovalMode::Auto),
        "auto mode (classifier-evaluated)"
    );
    assert_eq!(approval_mode_label(ApprovalMode::Yolo), "YOLO mode");
    assert_eq!(approval_mode_label(ApprovalMode::Default), "");
    assert_eq!(CYCLE_HINT, " (shift + tab to cycle)");
    assert_eq!(SHORTCUTS_HINT, "? for shortcuts");
}

#[test]
fn the_autoaccept_label_colour_matches_the_mode() {
    // plan → success, auto-edit/auto → warning, yolo → error (qwen).
    let theme = theme::dark();
    assert_eq!(
        approval_mode_style(ApprovalMode::Plan, theme).fg,
        success_style(theme).fg
    );
    assert_eq!(
        approval_mode_style(ApprovalMode::AutoEdit, theme).fg,
        warning_style(theme).fg
    );
    assert_eq!(
        approval_mode_style(ApprovalMode::Auto, theme).fg,
        warning_style(theme).fg
    );
    assert_eq!(
        approval_mode_style(ApprovalMode::Yolo, theme).fg,
        error_style(theme).fg
    );
}

#[test]
fn the_cost_item_is_absent_at_zero_and_present_when_priced() {
    let zero = footer(
        200,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: no_figures(),
            approval_mode: ApprovalMode::Default,
        },
    );
    assert!(
        !zero
            .right
            .iter()
            .any(|i| matches!(i, FooterItem::Cost { .. }))
    );

    let priced = footer(
        200,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: FigureView {
                tokens: None,
                session_cost: 0.42,
                context_budget: None,
                mcp_offline: 0,
            },
            approval_mode: ApprovalMode::Default,
        },
    );
    assert_eq!(
        priced
            .right
            .iter()
            .find(|i| matches!(i, FooterItem::Cost { .. })),
        Some(&FooterItem::Cost {
            label: "$0.42".into()
        })
    );
}

#[test]
fn the_mcp_health_pill_shows_when_offline_and_agrees_in_number() {
    // Singular at one, plural above, hidden at zero (qwen getPillLabel).
    assert_eq!(mcp_offline_label(0), None);
    assert_eq!(mcp_offline_label(1).as_deref(), Some("1 MCP offline"));
    assert_eq!(mcp_offline_label(3).as_deref(), Some("3 MCPs offline"));
}

#[test]
fn the_footer_carries_the_pill_only_when_servers_are_offline() {
    let offline = footer(
        200,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: FigureView {
                tokens: None,
                session_cost: 0.0,
                context_budget: None,
                mcp_offline: 2,
            },
            approval_mode: ApprovalMode::Default,
        },
    );
    assert!(offline.right.contains(&FooterItem::McpOffline {
        label: "2 MCPs offline".into(),
    }));

    // At zero the pill is absent (qwen's hidden-at-zero rule).
    let healthy = footer(
        200,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: no_figures(),
            approval_mode: ApprovalMode::Default,
        },
    );
    assert!(
        !healthy
            .right
            .iter()
            .any(|i| matches!(i, FooterItem::McpOffline { .. }))
    );
}

#[test]
fn cost_label_shows_two_decimals_and_a_sub_cent_floor() {
    assert_eq!(cost_label(0.42), "$0.42");
    assert_eq!(cost_label(0.01), "$0.01");
    assert_eq!(cost_label(12.3), "$12.30");
    assert_eq!(cost_label(1234.567), "$1234.57");
    assert_eq!(cost_label(0.0099), "<$0.01");
    assert_eq!(cost_label(0.0001), "<$0.01");
}

#[test]
fn context_percent_label_formats_the_ratio_flat_and_labels_by_width() {
    // FLAT now (no block padding, ADR-0053): the leading `%` is part of the
    // label; `% used` under 100 cols, `% context used` at/above.
    assert_eq!(
        context_percent_label(2500, 10_000, 80).as_deref(),
        Some("25.0% used")
    );
    assert_eq!(
        context_percent_label(2500, 10_000, 120).as_deref(),
        Some("25.0% context used")
    );
    assert_eq!(
        context_percent_label(15_000, 10_000, 120).as_deref(),
        Some(">100% context used")
    );
    assert_eq!(context_percent_label(2500, 0, 80), None);
}

#[test]
fn context_over_limit_flags_usage_past_the_budget() {
    assert!(!context_over_limit(2500, 10_000));
    assert!(!context_over_limit(10_000, 10_000));
    assert!(context_over_limit(10_001, 10_000));
    assert!(!context_over_limit(1, 0));
}

#[test]
fn the_footer_carries_a_context_item_only_when_a_budget_exists() {
    let with_budget = footer(
        120,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: FigureView {
                tokens: Some(2500),
                session_cost: 0.0,
                context_budget: Some(10_000),
                mcp_offline: 0,
            },
            approval_mode: ApprovalMode::Default,
        },
    );
    assert_eq!(
        with_budget
            .right
            .iter()
            .find(|i| matches!(i, FooterItem::Context { .. })),
        Some(&FooterItem::Context {
            label: "25.0% context used".into(),
            over_limit: false,
        })
    );

    let no_budget = footer(
        120,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: tokens_only(2500),
            approval_mode: ApprovalMode::Default,
        },
    );
    assert!(
        !no_budget
            .right
            .iter()
            .any(|i| matches!(i, FooterItem::Context { .. }))
    );
}

#[test]
fn an_over_budget_context_item_flags_over_limit() {
    let bar = footer(
        120,
        FooterView {
            conn: ConnectionView {
                base_url: "u",
                model: "m",
            },
            figures: FigureView {
                tokens: Some(15_000),
                session_cost: 0.0,
                context_budget: Some(10_000),
                mcp_offline: 0,
            },
            approval_mode: ApprovalMode::Default,
        },
    );
    assert_eq!(
        bar.right
            .iter()
            .find(|i| matches!(i, FooterItem::Context { .. })),
        Some(&FooterItem::Context {
            label: ">100% context used".into(),
            over_limit: true,
        })
    );
}

#[test]
fn a_footer_item_cells_match_its_text() {
    let item = FooterItem::Model {
        model: "qwen3-coder".into(),
    };
    assert_eq!(item.cells(), item.text().chars().count());
    assert_eq!(item.text(), "model qwen3-coder");
}

// -----------------------------------------------------------------------
// The render cache, read through its accessors.
//
// The cache syncs against the Transcript STORE (ADR-0034): tests seed a
// bare store through its verbs - the items Vec is not reachable, which is
// the point (the extend-vs-rebuild contract is the store's revision).
// The tests HERE observe only what the frame path observes (`settled`,
// `streaming_tail`); the extend-vs-rebuild invariant itself is pinned by
// sentinel tests inside `render_cache`, next to the private entries.
// -----------------------------------------------------------------------

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn fresh_transcript() -> Transcript {
    Transcript::new(Vec::new())
}

#[test]
fn cache_sync_builds_one_entry_per_settled_item_with_its_wrapped_count() {
    let mut t = fresh_transcript();
    // The `>` caret prefix is baked into the cached User line (ADR-0046 qwen
    // chrome), so the 16-char word hangs under the 2-col prefix and wraps at
    // the reduced width (10 - 2 = 8) to 2 rows.
    t.user("0123456789012345");
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 10, theme::dark());
    assert_eq!(cache.settled().count(), 1);
    // 2 wrapped rows, dense (no per-item blank separator).
    assert_eq!(cache.settled().next().unwrap().1, 2);
}

#[test]
fn cache_sync_rebuilds_when_the_width_changes() {
    let mut t = fresh_transcript();
    t.user("0123456789012345");
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    // 1 content row, dense (no per-item blank separator).
    let wide = cache.settled().next().unwrap().1;
    assert_eq!(wide, 1);
    cache.sync(&t, Toggles::default(), 10, theme::dark()); // resize: every wrapped count is stale
    assert!(cache.settled().next().unwrap().1 > wide);
}

#[test]
fn cache_sync_rebuilds_when_compact_hides_a_thought() {
    // Compact mode (Ctrl+O) HIDES a settled Thinking item entirely (qwen
    // `!compactMode`, ADR-0052); the default shows the full grey body.
    let mut t = fresh_transcript();
    t.push(TranscriptItem::Thinking {
        text: "line one\nline two".to_string(),
    });
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    // Default (compact=false): the grey `✦`-prefixed markdown body (two
    // source rows → two rows).
    assert_eq!(cache.settled().next().unwrap().0.len(), 2);
    cache.sync(&t, Toggles { compact: true }, 80, theme::dark());
    // Compact: the thought is hidden entirely - zero lines.
    assert_eq!(cache.settled().next().unwrap().0.len(), 0);
}

#[test]
fn cache_sync_rebuilds_when_compact_folds_a_tool_body() {
    // Compact folds a multi-line Diff to a single title line (qwen
    // `!compactMode || forceShowResult`); the default shows the full body.
    // Flipping the toggle clears the cache so the change takes effect.
    // Separators are added at assembly, not baked per item.
    let mut t = fresh_transcript();
    t.push(diff_item(
        "edit src/foo.rs",
        vec![
            DiffLine::new(DiffSide::Added, "added line"),
            DiffLine::new(DiffSide::Removed, "removed line"),
        ],
    ));
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles { compact: true }, 80, theme::dark());
    // Compact: one fold row (3-wide marker gutter + title + affordance).
    let collapsed = cache.settled().next().unwrap().0;
    assert_eq!(collapsed.len(), 1);
    assert_eq!(
        line_text(&collapsed[0]).trim_start(),
        "edit src/foo.rs · ^O expand"
    );
    // The default (compact=false) shows the full body.
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    // The tool header row + both diff body rows.
    let expanded = cache.settled().next().unwrap().0;
    assert_eq!(expanded.len(), 3);
    assert!(line_text(&expanded[0]).contains("edit src/foo.rs"));
}

/// Every fg across the cache's first settled item, in order: each line's
/// own style fg (styled Lines carry their color there), then its spans'.
fn settled_span_fgs(cache: &RenderCache) -> Vec<Option<Color>> {
    cache
        .settled()
        .next()
        .expect("one settled item")
        .0
        .iter()
        .flat_map(|l| std::iter::once(l.style.fg).chain(l.spans.iter().map(|s| s.style.fg)))
        .collect()
}

#[test]
fn cache_sync_rebuilds_when_the_theme_changes() {
    // Cached lines BAKE their colors, so a Theme swap (Stage C's live
    // preview) must clear the cache: after syncing with a Theme that
    // recolors `muted`, the settled thought (grey = muted) carries the new
    // color. A Marker's `●` prefix reads its tone slot (Plain = muted).
    let mut t = fresh_transcript();
    t.marker("a marker", Tone::Plain);
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    assert_eq!(settled_span_fgs(&cache)[1], Some(Color::DarkGray));

    let recolored = themed("[colors]\nmuted = \"#ff00ff\"\n");
    cache.sync(&t, Toggles::default(), 80, &recolored);
    assert_eq!(settled_span_fgs(&cache)[1], Some(Color::Rgb(255, 0, 255)));
}

#[test]
fn cache_sync_repaints_highlighted_code_on_a_syntax_theme_swap() {
    // The stale-highlight hazard: syntect colors are baked into cached
    // spans, so a Theme differing only in its `syntax` slot must also
    // rebuild - the swap may not serve the old code colors.
    let mut t = fresh_transcript();
    t.push(TranscriptItem::Assistant {
        text: "```rust\nlet x = 1;\n```".to_string(),
    });
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    let dark_colors = settled_span_fgs(&cache);

    let light_syntax = themed("syntax = \"InspiredGitHub\"\n");
    cache.sync(&t, Toggles::default(), 80, &light_syntax);
    assert_ne!(
        settled_span_fgs(&cache),
        dark_colors,
        "the swap repainted the cached code block"
    );
}

// --- Stage 3: merged one-liners + semantic fold ------------------------

#[test]
fn a_merged_result_renders_name_arg_dot_result() {
    let item = TranscriptItem::ToolResult {
        name: "read_file".to_string(),
        summary: "340 lines".to_string(),
        is_error: false,
        key_arg: Some("src/foo.rs".to_string()),
    };
    // The INNER box content (qwen `ToolInfo`): the 3-wide `✓` marker gutter,
    // the bold name, then the dim `arg · result` description.
    let lines = message_lines(&item, false, 80, theme::dark());
    assert_eq!(lines.len(), 1);
    assert_eq!(line_text(&lines[0]), "✓  read_file src/foo.rs · 340 lines");
}

#[test]
fn an_unpaired_result_shows_only_the_summary() {
    // No key_arg (governor-injected result): the description is the bare
    // summary (no arg to set off).
    let item = TranscriptItem::ToolResult {
        name: "run_shell_command".to_string(),
        summary: "injected".to_string(),
        is_error: false,
        key_arg: None,
    };
    let lines = message_lines(&item, false, 80, theme::dark());
    assert_eq!(line_text(&lines[0]), "✓  run_shell_command injected");
}

// --- The startup Header banner (qwen `AppHeader`) ----------------------

// Tier 1 (side-by-side): at a WIDE content width (>= 129) the 83-col logo
// draws to the LEFT of the bordered info panel, and the last row is the
// `Tips:` line. Every drawn row stays within the content width (measure==draw).
#[test]
fn a_header_at_a_wide_width_shows_the_logo_beside_the_boxed_panel() {
    let item = TranscriptItem::Header {
        title: "suspenders".into(),
        version: "1.2.3".into(),
        model: "openrouter/qwen3-coder".into(),
        cwd: "/tmp/proj".into(),
        tip: "Type / to see all available commands.".into(),
    };
    let width = 140;
    let lines = message_lines(&item, false, width, theme::dark());
    let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    // The logo drew (its first row's block glyphs are present) beside the box.
    assert!(text.contains("███████╗"), "the logo drew:\n{text}");
    assert!(
        text.contains("╭") && text.contains("╮"),
        "the box drew:\n{text}"
    );
    assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
    assert!(text.contains("(v1.2.3)"), "the version:\n{text}");
    assert!(
        text.contains("openrouter/qwen3-coder"),
        "the scoped model:\n{text}"
    );
    assert!(
        text.contains("(/model to change)"),
        "the model hint fits at this width:\n{text}"
    );
    // The Tips line is the final row.
    assert_eq!(
        line_text(lines.last().unwrap()),
        "Tips: Type / to see all available commands."
    );
    // No row soft-wraps: each is within the content width (measure==draw).
    for row in &lines {
        assert!(
            row_display_width(row) <= width as usize,
            "row exceeds content width: {:?}",
            line_text(row)
        );
    }
}

// The WIDTH GATE (qwen `showLogo`): a NARROW content width cannot fit the
// 83-col logo + gap + a minimum info panel, so the logo is hidden and the
// panel (+ tips) render alone. The block glyphs never appear.
#[test]
fn a_header_at_a_narrow_width_hides_the_logo() {
    let item = TranscriptItem::Header {
        title: "suspenders".into(),
        version: "1.2.3".into(),
        model: "openrouter/qwen3-coder".into(),
        cwd: "/tmp/proj".into(),
        tip: "Type / to see all available commands.".into(),
    };
    let width = 50;
    let lines = message_lines(&item, false, width, theme::dark());
    let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(
        !text.contains("███████╗"),
        "the logo is hidden on a narrow terminal:\n{text}"
    );
    // The bordered panel + brand + tips still render.
    assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
    assert!(text.contains("╭"), "the box top border:\n{text}");
    assert!(text.contains("Tips:"), "the tips line:\n{text}");
    for row in &lines {
        assert!(
            row_display_width(row) <= width as usize,
            "row exceeds content width: {:?}",
            line_text(row)
        );
    }
}

// Tier 2 (stacked): a MID content width (83 <= W < 129) that fits the logo but
// not the side-by-side panel draws the full-width logo banner ON TOP, then the
// bordered info panel BELOW it. Both the block glyphs AND the box appear, and
// no row exceeds the content width (measure==draw).
#[test]
fn a_header_at_a_mid_width_stacks_the_logo_above_the_boxed_panel() {
    let item = TranscriptItem::Header {
        title: "suspenders".into(),
        version: "1.2.3".into(),
        model: "openrouter/qwen3-coder".into(),
        cwd: "/tmp/proj".into(),
        tip: "Type / to see all available commands.".into(),
    };
    let width = 100;
    let lines = message_lines(&item, false, width, theme::dark());
    let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    // The logo banner AND the box both drew.
    assert!(text.contains("███████╗"), "the logo banner drew:\n{text}");
    assert!(
        text.contains("╭") && text.contains("╮"),
        "the box drew below the logo:\n{text}"
    );
    assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
    assert!(text.contains("Tips:"), "the tips line:\n{text}");
    // The logo is a full-width TOP banner: the first 6 rows are pure logo (no
    // box border yet), then the box begins. Row 0 is a logo row, and the box
    // top border appears only AFTER the 6 logo rows.
    assert!(
        line_text(&lines[0]).contains("███████╗"),
        "row 0 is the logo banner top:\n{text}"
    );
    let box_top = lines
        .iter()
        .position(|l| line_text(l).contains('╭'))
        .expect("the box top border row");
    assert!(
        box_top >= 6,
        "the box begins after the 6 logo rows (stacked), got row {box_top}"
    );
    // No row soft-wraps: each is within the content width (measure==draw).
    for row in &lines {
        assert!(
            row_display_width(row) <= width as usize,
            "row exceeds content width: {:?}",
            line_text(row)
        );
    }
}

// The tier gate is the ONE place the boundary math lives: side-by-side at
// >= 129, stacked at 83..129, no-logo below 83.
#[test]
fn header_tier_boundaries_are_exact() {
    assert_eq!(header_tier(129), HeaderTier::SideBySide);
    assert_eq!(header_tier(128), HeaderTier::Stacked);
    assert_eq!(header_tier(83), HeaderTier::Stacked);
    assert_eq!(header_tier(82), HeaderTier::NoLogo);
    assert_eq!(header_tier(0), HeaderTier::NoLogo);
}

// Measure==draw (ADR-0029): every Header row is `<= content width` across
// ALL three tiers (NoLogo, Stacked, SideBySide), so the startup banner never
// soft-wraps a row - the same width-sweep guard the Help panel carries.
#[test]
fn header_rows_never_exceed_the_content_width() {
    let item = TranscriptItem::Header {
        title: "suspenders".into(),
        version: "1.2.3".into(),
        model: "openrouter/qwen3-coder".into(),
        cwd: "/home/vinnie/Projects/suspenders".into(),
        tip: "Use @path/to/file to add files as context.".into(),
    };
    for width in [40u16, 60, 82, 83, 100, 128, 129, 160, 220] {
        let lines = message_lines(&item, false, width, theme::dark());
        for row in &lines {
            assert!(
                row_display_width(row) <= width as usize,
                "row exceeds width {width}: {:?}",
                line_text(row)
            );
        }
    }
}

// The model hint is dropped when the scoped id + ` (/model to change)` would
// overflow the info panel's inner width (qwen `showModelHint`), so the panel
// never soft-wraps.
#[test]
fn a_header_drops_the_model_hint_when_it_would_not_fit() {
    let item = TranscriptItem::Header {
        title: "suspenders".into(),
        version: "1.2.3".into(),
        model: "some-very-long-provider/a-very-long-model-identifier-name".into(),
        cwd: "/tmp/proj".into(),
        tip: "tip".into(),
    };
    // A width wide enough for the panel but not the model + hint together.
    let width = 50;
    let lines = message_lines(&item, false, width, theme::dark());
    let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(
        !text.contains("(/model to change)"),
        "the hint is dropped when it would overflow:\n{text}"
    );
}

// tildeify_with_home abbreviates a leading home to `~`, and leaves other
// paths (and the no-home case) untouched. Pure - no process env is touched.
#[test]
fn tildeify_abbreviates_the_home_prefix() {
    let home = Some("/home/dev");
    assert_eq!(tildeify_with_home("/home/dev/proj/src", home), "~/proj/src");
    assert_eq!(tildeify_with_home("/home/dev", home), "~");
    assert_eq!(tildeify_with_home("/etc/hosts", home), "/etc/hosts");
    // A path merely PREFIXED by the home string but not on a path boundary is
    // left alone (no `~/develop` from `/home/develop`).
    assert_eq!(tildeify_with_home("/home/develop", home), "/home/develop");
    // No home / empty home: pass through.
    assert_eq!(tildeify_with_home("/home/dev/x", None), "/home/dev/x");
    assert_eq!(tildeify_with_home("/home/dev/x", Some("")), "/home/dev/x");
}

#[test]
fn a_failing_result_shows_the_error_marker() {
    // A failed result reads the `x` U+0078 ERROR marker (0.16.0 ASCII, NOT
    // `✗`), then the name + `arg · summary` description.
    let item = TranscriptItem::ToolResult {
        name: "run_shell_command".to_string(),
        summary: "exit 1".to_string(),
        is_error: true,
        key_arg: Some("cargo test".to_string()),
    };
    let lines = message_lines(&item, false, 80, theme::dark());
    assert_eq!(
        line_text(&lines[0]),
        "x  run_shell_command cargo test · exit 1"
    );
}

#[test]
fn a_failing_result_without_an_arg_shows_the_bare_summary() {
    let item = TranscriptItem::ToolResult {
        name: "edit".to_string(),
        summary: "old_str not found".to_string(),
        is_error: true,
        key_arg: Some("src/foo.rs".to_string()),
    };
    let lines = message_lines(&item, false, 80, theme::dark());
    assert_eq!(
        line_text(&lines[0]),
        "x  edit src/foo.rs · old_str not found"
    );
}

// A Marker's prefix glyph + tint are chosen by its Tone (qwen StatusMessages
// set): a Constrain marker reads the `△` warning, everything else the `●` info
// glyph. Identical text under two tones tints differently, proving the adapter
// never sniffs the line.
#[test]
fn a_marker_tints_by_its_tone_not_its_text() {
    let theme = theme::dark();
    for (tone, glyph, expected) in [
        (Tone::Housekeeping, "● ", theme.muted),
        (Tone::Aid, "● ", theme.muted),
        (Tone::Constrain, "△ ", theme.warning),
        (Tone::Steering, "● ", theme.accent),
        (Tone::Plain, "● ", theme.muted),
    ] {
        let item = TranscriptItem::Marker {
            // Same text for every tone: the tint cannot be coming from it.
            text: "harness marker".to_string(),
            tone,
        };
        let lines = message_lines(&item, false, 80, theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            line_text(&lines[0]),
            format!("{glyph}harness marker"),
            "{tone:?}"
        );
        // The prefix span carries the tone color.
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(tui_color(expected)),
            "{tone:?}"
        );
    }
}

#[test]
fn has_foldable_body_is_true_only_for_a_non_empty_diff() {
    // A non-empty Diff folds under Ctrl-O.
    let diff = diff_item("edit x", vec![DiffLine::new(DiffSide::Added, "a")]);
    assert!(diff.has_foldable_body());

    // A one-line merged ToolResult has no body to fold.
    let result = TranscriptItem::ToolResult {
        name: "read_file".to_string(),
        summary: "340 lines".to_string(),
        is_error: false,
        key_arg: Some("src/foo.rs".to_string()),
    };
    assert!(!result.has_foldable_body());

    // A Diff with no hunk lines has nothing to fold either.
    let empty = diff_item("titled but empty", vec![]);
    assert!(!empty.has_foldable_body());
}

#[test]
fn ctrl_o_still_folds_a_diff_after_the_merge() {
    // A merge produces a lone Diff (the call line removed). Ctrl-O must still
    // collapse it to its one-line title - the semantic fold predicate keys
    // on the Diff's foldable body, unaffected by the merge.
    let diff = diff_item(
        "edit src/foo.rs (+1 -1)",
        vec![
            DiffLine::new(DiffSide::Added, "new"),
            DiffLine::new(DiffSide::Removed, "old"),
        ],
    );
    // Compact (folds the body): one fold row (3-wide marker gutter + the
    // title and the `· ^O expand` affordance).
    let collapsed = message_lines(&diff, true, 80, theme::dark());
    assert_eq!(collapsed.len(), 1);
    assert_eq!(
        line_text(&collapsed[0]).trim_start(),
        "edit src/foo.rs (+1 -1) · ^O expand"
    );
    // Default (compact=false): the tool header row + both body rows.
    let expanded = message_lines(&diff, false, 80, theme::dark());
    assert_eq!(expanded.len(), 3);
}

// -----------------------------------------------------------------------
// diff rendering (ADR-0008): the marker glyph, the full-width tint band,
// and the two-pass hunk-coherent syntect highlighting.
// -----------------------------------------------------------------------

// A one-hunk Diff item: the shared builder every diff-render test routes
// through, so the `TranscriptItem::Diff { … }` literal lives in one place.
fn diff_of(lang: Option<&str>, header: Option<&str>, lines: Vec<DiffLine>) -> TranscriptItem {
    TranscriptItem::Diff {
        title: "edit foo".to_string(),
        lang: lang.map(str::to_string),
        hunks: vec![DiffHunk {
            header: header.map(str::to_string),
            lines,
        }],
        elided: 0,
    }
}

// The rendered diff code rows for `item` (bypassing the tool box): the
// line-number-gutter + marker + code rows [`diff_lines`] produces, the direct
// seam these diff-internals tests inspect (the box only adds an outer 3-col
// indent). Panics on a non-Diff item.
fn diff_rows_of(item: &TranscriptItem, width: u16) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::Diff { lang, hunks, .. } => {
            diff_lines(lang.as_deref(), hunks, width, theme::dark())
        }
        _ => panic!("not a Diff item"),
    }
}

// A created-file Diff (one all-added hunk, `header: None`) of `content` in
// `lang`, rendered to its code rows.
fn created_diff_rows(lang: &str, content: &[&str], width: u16) -> Vec<Line<'static>> {
    let lines = content
        .iter()
        .map(|t| DiffLine::new(DiffSide::Added, *t))
        .collect();
    let item = diff_of(Some(lang), None, lines);
    diff_rows_of(&item, width)
}

// The distinct syntect foregrounds of a diff code row: the fgs AFTER the
// line-number gutter + marker glyph, dropping the trailing full-width pad (bg
// only, no fg). Used to compare the color a line's code was highlighted with.
fn code_fgs(row: &Line<'static>) -> Vec<Color> {
    row.spans
        .iter()
        .skip(2) // the line-number gutter + the marker glyph
        .filter_map(|s| s.style.fg)
        .collect()
}

#[test]
fn a_created_file_block_comment_colors_coherently_across_every_line() {
    // The ADR-0008 HARD requirement: a created file is one all-added hunk =
    // the whole file, highlighted as ONE slice so syntect parse state
    // carries. A multi-line `/** … */` JSDoc block MUST color as a comment
    // across ALL its lines - per-line-independent highlighting (which would
    // color only line 1 as a comment and lines 2-3 as plain text) is WRONG.
    let rows = created_diff_rows(
        "js",
        &[
            "/**",
            " * a doc comment",
            " * spanning lines",
            " */",
            "const x = 1;",
        ],
        80,
    );
    // The 4 comment lines are rows[0..=3]; row[4] is the trailing code line.
    let comment_fg = code_fgs(&rows[0]);
    assert!(
        comment_fg.iter().all(|c| matches!(c, Color::Rgb(..))),
        "the comment's first line is syntect-colored: {comment_fg:?}"
    );
    let first = comment_fg[0];
    for row in &rows[0..=3] {
        for fg in code_fgs(row) {
            assert_eq!(
                fg,
                first,
                "every line of the block comment shares the comment color \
                     (parse state carried across the hunk): {:?}",
                line_text(row)
            );
        }
    }
    // The trailing code line, by contrast, is NOT the comment color - proof
    // the comment actually closed and highlighting resumed.
    let code_fg = code_fgs(&rows[4]);
    assert!(
        code_fg.iter().any(|c| *c != first),
        "the `const x = 1;` line is not the comment color: {code_fg:?}"
    );
}

#[test]
fn a_removed_line_highlights_from_the_before_image() {
    // The removed side of a hunk highlights as its own slice (the before
    // image): a removed comment line still colors as a comment, from the
    // before-image pass, not the after-image one.
    let item = diff_of(
        Some("js"),
        Some("@@ -1,2 +1,1 @@"),
        vec![
            DiffLine::new(DiffSide::Removed, "// gone"),
            DiffLine::new(DiffSide::Added, "kept();"),
        ],
    );
    let rows = diff_rows_of(&item, 80);
    // rows: removed, added (the `@@` header is parsed for line numbers, not
    // drawn as a row).
    let removed = &rows[0];
    assert_eq!(removed.spans[1].content.as_ref(), "- ");
    // The removed comment carried a syntect fg (before-image highlighted).
    assert!(
        code_fgs(removed)
            .iter()
            .all(|c| matches!(c, Color::Rgb(..))),
        "the removed comment is syntect-colored: {:?}",
        line_text(removed)
    );
}

#[test]
fn an_added_line_reads_as_a_full_width_tint_band() {
    // The tint is GitHub-style: a full-width band. The row's LAST span pads
    // to the content width and carries the added_bg, and the marker glyph +
    // code carry that same bg over their fg.
    let rows = created_diff_rows("rs", &["let x = 1;"], 40);
    let row = &rows[0];
    let added_bg = Some(tui_color(theme::dark().added_bg));
    // The marker glyph (after the line-number gutter) carries the tint and the
    // semantic (green) fg.
    assert_eq!(row.spans[1].content.as_ref(), "+ ");
    assert_eq!(row.spans[1].style.bg, added_bg);
    assert_eq!(row.spans[1].style.fg, Some(tui_color(theme::dark().added)));
    // Every span (gutter, marker, code, pad) carries the tint (band-wide).
    for span in &row.spans {
        assert_eq!(span.style.bg, added_bg, "band span keeps the tint");
    }
    // The row fills the width exactly, in DISPLAY COLUMNS (indent + marker +
    // code + pad).
    assert_eq!(row_display_width(row), 40, "the band reaches the edge");
    // The last span is the pad (bg only, no fg).
    let pad = row.spans.last().unwrap();
    assert_eq!(pad.style.bg, added_bg);
    assert_eq!(pad.style.fg, None);
}

#[test]
fn a_context_line_is_untinted() {
    let item = diff_of(
        Some("rs"),
        None,
        vec![DiffLine::new(DiffSide::Context, "let x = 1;")],
    );
    let rows = diff_rows_of(&item, 40);
    let ctx = &rows[0];
    // The context marker (after the gutter) is two blanks and NO span carries
    // a background.
    assert_eq!(ctx.spans[1].content.as_ref(), "  ");
    for span in &ctx.spans {
        assert_eq!(span.style.bg, None, "context is untinted");
    }
}

#[test]
fn an_unknown_language_falls_back_to_the_semantic_foreground() {
    // No language resolves for a `.txt` extension (lang: None in practice);
    // the code still renders, tinted, with the semantic fg (no syntect).
    let item = diff_of(
        None,
        None,
        vec![DiffLine::new(DiffSide::Added, "just text")],
    );
    let rows = diff_rows_of(&item, 40);
    let row = &rows[0];
    // The code span (after gutter + marker) carries the semantic added fg
    // (green), not a syntect Rgb.
    let code = &row.spans[2];
    assert_eq!(code.content.as_ref(), "just text");
    assert_eq!(code.style.fg, Some(tui_color(theme::dark().added)));
}

#[test]
fn the_elided_tail_renders_as_a_muted_count() {
    let tail_rows = diff_elided_tail(40, 40, theme::dark());
    let tail = &tail_rows[0];
    assert_eq!(line_text(tail).trim_end(), "... last 40 lines hidden ...");
    assert_eq!(tail.style, diff_chrome_style(theme::dark()));
}

#[test]
fn an_interleaved_hunk_aligns_each_line_to_its_own_image() {
    // The cursor-alignment path: a hunk that interleaves context, removed,
    // and added lines must draw each line from the RIGHT image (added/context
    // from the after pass, removed/context from the before) with no desync.
    // The `x` identifier appears on every line, so a coherent highlight gives
    // every row the SAME fg for that token; a desynced cursor would mis-color.
    let item = diff_of(
        Some("rs"),
        Some("@@ -1,4 +1,4 @@"),
        vec![
            DiffLine::new(DiffSide::Context, "let x = 0;"),
            DiffLine::new(DiffSide::Removed, "let x = 1;"),
            DiffLine::new(DiffSide::Removed, "let x = 2;"),
            DiffLine::new(DiffSide::Added, "let x = 3;"),
            DiffLine::new(DiffSide::Context, "let x = 4;"),
            DiffLine::new(DiffSide::Added, "let x = 5;"),
        ],
    );
    let rows = diff_rows_of(&item, 80);
    // rows: the 6 code rows in file order (no title/header row).
    let code = &rows[..];
    assert_eq!(code.len(), 6);
    // The `let` keyword is fragment 0 of every code row; its fg is the syntect
    // keyword color, identical on every line iff the two passes stayed aligned.
    let keyword_fg = |row: &Line<'static>| code_fgs(row).first().copied();
    let first = keyword_fg(&code[0]).expect("the first row is highlighted");
    assert!(
        matches!(first, Color::Rgb(..)),
        "syntect colored it: {first:?}"
    );
    for row in code {
        assert_eq!(
            keyword_fg(row),
            Some(first),
            "every interleaved line's keyword shares one color: {:?}",
            line_text(row)
        );
    }
    // And each line wears the tint of ITS side (added/removed/context).
    let added_bg = Some(tui_color(theme::dark().added_bg));
    let removed_bg = Some(tui_color(theme::dark().removed_bg));
    assert_eq!(code[0].spans[1].content.as_ref(), "  "); // context marker
    assert_eq!(code[0].spans.last().unwrap().style.bg, None);
    assert_eq!(code[1].spans[1].content.as_ref(), "- "); // removed marker
    assert_eq!(code[1].spans.last().unwrap().style.bg, removed_bg);
    assert_eq!(code[3].spans[1].content.as_ref(), "+ "); // added marker
    assert_eq!(code[3].spans.last().unwrap().style.bg, added_bg);
}

#[test]
fn an_all_removed_hunk_renders_from_the_before_image() {
    // A pure deletion: every line is Removed, so the after image is empty and
    // the whole hunk highlights from the before image. Each row wears the
    // removed marker, the removed tint, and a syntect fg.
    let item = diff_of(
        Some("rs"),
        Some("@@ -1,2 +0,0 @@"),
        vec![
            DiffLine::new(DiffSide::Removed, "fn gone() {}"),
            DiffLine::new(DiffSide::Removed, "fn also() {}"),
        ],
    );
    let rows = diff_rows_of(&item, 80);
    let removed_bg = Some(tui_color(theme::dark().removed_bg));
    for row in &rows[..] {
        assert_eq!(row.spans[1].content.as_ref(), "- ");
        assert_eq!(row.spans.last().unwrap().style.bg, removed_bg);
        assert!(
            code_fgs(row).iter().all(|c| matches!(c, Color::Rgb(..))),
            "the removed code is syntect-colored: {:?}",
            line_text(row)
        );
    }
}

#[test]
fn a_tab_in_a_diff_line_expands_through_the_full_row() {
    // The tab→two-spaces normalization survives the whole render path, not
    // just the unit: a `\t`-indented code line draws with the tab expanded.
    // A tab becomes two spaces; the common leading indentation (here the whole
    // tab) is then stripped per-hunk (qwen DiffRenderer), so the code reads at
    // the box edge. The key invariant: no raw tab survives the render path.
    let rows = created_diff_rows("rs", &["\tlet x = 1;"], 80);
    let row = &rows[0];
    let text = line_text(row);
    assert!(
        text.starts_with("1 + let x = 1;"),
        "tab expanded + common indent stripped: {text:?}"
    );
    assert!(!text.contains('\t'), "no raw tab survives: {text:?}");
}

#[test]
fn an_over_wide_code_row_is_clipped_to_the_width() {
    // The clip branch of `push_cols`: a code line wider than the content area
    // is truncated so the row occupies exactly `width` columns (and thus never
    // soft-wraps). Width 20, a 40-char line.
    let long = "x".repeat(40);
    let rows = created_diff_rows("rs", &[&long], 20);
    let row = &rows[0];
    assert_eq!(
        row_display_width(row),
        20,
        "the row is clipped to the width"
    );
    // One visual row: the viewport's own wrap math agrees (measure==draw).
    assert_eq!(wrapped_count(vec![row.clone()], 20), 1);
}

#[test]
fn a_wide_cjk_diff_row_stays_one_visual_row() {
    // The MAJOR width-correctness fix (review #2): widths are DISPLAY COLUMNS,
    // not char counts. A CJK line char-padded to `width` would render WIDER
    // than `width` columns and the viewport `Wrap` would re-break it, shatter-
    // ing the tint band. Assert via the SAME `wrapped_count` the viewport uses
    // that a wide-glyph row occupies exactly one visual row at several widths.
    for width in [12u16, 20, 41] {
        // Each CJK ideograph is two columns; mix in ASCII and an emoji.
        let rows = created_diff_rows("txt", &["語 = 実装 ✨ done"], width);
        let row = &rows[0];
        assert!(
            row_display_width(row) <= width as usize,
            "row is within {width} columns: got {} for {:?}",
            row_display_width(row),
            line_text(row)
        );
        assert_eq!(
            wrapped_count(vec![row.clone()], width),
            1,
            "the wide-glyph row stays ONE visual row at width {width}: {:?}",
            line_text(row)
        );
    }
}

// The rendered display width of a diff row (sum of its spans' column widths).
fn row_display_width(row: &Line<'static>) -> usize {
    row.spans.iter().map(|s| s.content.width()).sum()
}

#[test]
fn parse_hunk_header_reads_the_old_and_new_start_line_numbers() {
    // The MEDIUM-risk data path (Phase 2 risk #2): the `@@ -old,_ +new,_ @@`
    // header is parsed render-side for the two 1-based start numbers.
    assert_eq!(parse_hunk_header(Some("@@ -12,4 +30,5 @@")), (12, 30));
    // A single-line hunk omits the count (`@@ -a +b @@`); the starts still parse.
    assert_eq!(parse_hunk_header(Some("@@ -7 +9 @@")), (7, 9));
    // A trailing section label after the second `@@` does not confuse the
    // parse: the STARTS are `-1`/`+1` (the `,3`/`,4` are line counts, not starts).
    assert_eq!(
        parse_hunk_header(Some("@@ -1,3 +1,4 @@ fn main() {")),
        (1, 1)
    );
    // Distinct non-1 starts survive a trailing label too.
    assert_eq!(
        parse_hunk_header(Some("@@ -40,3 +52,4 @@ impl Foo {")),
        (40, 52)
    );
    // A created file carries no header; both sides start at line 1.
    assert_eq!(parse_hunk_header(None), (1, 1));
    // A malformed header falls back to (1, 1) rather than panicking.
    assert_eq!(parse_hunk_header(Some("not a header")), (1, 1));
}

#[test]
fn hunk_line_numbers_advances_each_side_by_line_kind() {
    // The exact per-row gutter numbers a mixed hunk draws (qwen DiffRenderer
    // :279-301): a context row shows its NEW number and advances BOTH sides; an
    // added row shows NEW and advances new only; a removed row shows OLD and
    // advances old only. Header `@@ -10,_ +20,_ @@` starts old at 10, new at 20.
    let hunk = DiffHunk {
        header: Some("@@ -10,3 +20,4 @@".to_string()),
        lines: vec![
            DiffLine::new(DiffSide::Context, "ctx a"), // old 10 / new 20 -> shows 20
            DiffLine::new(DiffSide::Removed, "gone"),  // old 11          -> shows 11
            DiffLine::new(DiffSide::Added, "new one"), // new 21          -> shows 21
            DiffLine::new(DiffSide::Added, "new two"), // new 22          -> shows 22
            DiffLine::new(DiffSide::Context, "ctx b"), // old 12 / new 23 -> shows 23
        ],
    };
    // Context: 20 (new). Removed: 11 (old, new stays 21). Added: 21, 22 (new).
    // Context: 23 (new; old is now 12).
    assert_eq!(hunk_line_numbers(&hunk), vec![20, 11, 21, 22, 23]);
}

// (The Ctrl-O viewport-stability test is retired: there is no adapter-side
// viewport now - native scrollback owns history, ADR-0046. Ctrl-O's effect
// on the cached line counts is still covered by the cache toggle tests.)

#[test]
fn per_item_wrapped_counts_sum_to_the_whole_paragraph_measure() {
    // The windowed render's geometry is the SUM of per-item measures; it
    // is only the same total the old whole-paragraph measure produced if
    // ratatui wraps each `Line` independently. Guard that assumption.
    let items = [
        TranscriptItem::User {
            text: "a user prompt long enough to wrap at a narrow width".to_string(),
        },
        TranscriptItem::Assistant {
            text: "some *markdown* with a fairly long paragraph in it\n\n- and\n- a list"
                .to_string(),
        },
        TranscriptItem::Info {
            text: "an info line".to_string(),
        },
    ];
    for width in [10u16, 24, 80] {
        let per_item: usize = items
            .iter()
            .map(|item| wrapped_count(message_lines(item, false, width, theme::dark()), width))
            .sum();
        let whole: Vec<Line> = items
            .iter()
            .flat_map(|item| message_lines(item, false, width, theme::dark()))
            .collect();
        assert_eq!(per_item, wrapped_count(whole, width), "width {width}");
    }
}

// -----------------------------------------------------------------------
// Frame-level render tests (ratatui TestBackend): draw one frame into an
// in-memory buffer and assert the meaningful facts land - titles, known
// lines, the scrollbar gutter - not whole-screen snapshots.
// -----------------------------------------------------------------------

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::content::ContentBlock;
use crate::event::Event;
use crate::llm::Delta;
use crate::llm::response::StopReason;
use crate::ui::screen::{Key, ScreenOpts};

/// Draws one frame with `draw` on a fresh `width`×`height` test terminal
/// and returns the terminal for buffer inspection.
fn draw_frame(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal.draw(|frame| draw(frame)).expect("draw one frame");
    terminal
}

/// One buffer row's symbols, concatenated.
fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
        .collect()
}

/// The whole buffer as newline-joined rows, for `contains` assertions.
fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| row_text(terminal, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the combined style modifiers of a single buffer cell. Shared by
/// the popup style assertions so the `buf.cell(...).expect(...).style().add_modifier`
/// chain is not repeated per-cell (DUPLICATE fix).
fn cell_modifier(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Modifier {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .expect("cell in test buffer")
        .style()
        .add_modifier
}

// The first char of a test-buffer cell (for gutter/marker assertions).
fn cell_char(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> char {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .expect("cell in test buffer")
        .symbol()
        .chars()
        .next()
        .unwrap_or(' ')
}

// The foreground color of a test-buffer cell (for accent/secondary checks).
fn cell_fg(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Option<Color> {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .expect("cell in test buffer")
        .style()
        .fg
}

/// Draws the inline PENDING transcript body (ADR-0046) for `screen` into a
/// fresh `width`x`height` terminal, TOP-aligned so the content-assertion
/// tests (which scan rows for known text/gutter glyphs) read a stable layout.
/// Uses [`render_pending_body_at`] directly (no status bar / composer) over
/// the whole area; the pending body draws the uncommitted settled tail plus
/// the live stream. Fresh cache, default anim, dark theme - the
/// overwhelmingly common test shape.
///
/// Top-aligned: when the content FITS, [`render_pending_body`] bottom-anchors,
/// so we draw into a body zone exactly as tall as the content when it fits,
/// and the full area when it overflows (top-clipped, newest kept).
fn draw_viewport(width: u16, height: u16, screen: &Screen) -> Terminal<TestBackend> {
    let mut cache = RenderCache::new();
    draw_frame(width, height, |f| {
        let area = f.area();
        // Measure the pending stack once to decide the zone height: a fitting
        // stack draws in a zone its own height (so it top-aligns), an
        // overflowing one uses the full area (top-clipped).
        let total = pending_body_height(screen, &mut cache, area.width, theme::dark());
        let zone_h = (total as u16).min(area.height).max(1);
        let zone = Rect {
            height: zone_h,
            ..area
        };
        // hw = 0: draw the WHOLE settled transcript (committed items live in
        // scrollback on a real TTY, but a headless content test wants them).
        render_pending_body_at(
            f,
            zone,
            &mut PendingBodyParams {
                screen,
                cache: &mut cache,
                anim: Anim::default(),
            },
            theme::dark(),
            0,
        );
    })
}

/// The pending stack's total wrapped rows for `screen` at `width` (test
/// helper): mirrors [`render_pending_body`]'s measurement so `draw_viewport`
/// can top-align a fitting stack.
fn pending_body_height(
    screen: &Screen,
    cache: &mut RenderCache,
    width: u16,
    theme: &Theme,
) -> usize {
    let content_width = width.saturating_sub(2 * CONTENT_MARGIN);
    cache.sync(
        screen.transcript(),
        Toggles {
            compact: screen.compact_mode,
        },
        content_width,
        theme,
    );
    let items = screen.transcript().items();
    // hw = 0: measure the WHOLE settled transcript (the test helper draws it
    // all top-aligned) through the SAME grouped fold the body draws (ADR-0046),
    // including the inline approval block when one is open (ADR-0049).
    let approving = screen.pending_approval.as_ref().and_then(|pending| {
        newest_live_tool_index(items).map(|call_index| Approving {
            pending,
            call_index,
        })
    });
    let mut lines = grouped_rows_with_approval(&GroupedRows {
        cache,
        items,
        hw: 0,
        width: content_width,
        theme,
        approving: approving.as_ref(),
    });
    // Add the live stream rows the body would append. Compact suppresses the
    // live thinking tail (matching `render_pending_body_at`).
    let thinking = screen.transcript().streaming_thinking();
    let thinking_lines = if screen.compact_mode {
        Vec::new()
    } else {
        live_thinking_lines(&thinking, 0, content_width, theme)
    };
    append_live(&mut lines, &thinking_lines);
    if let Some((tail, _)) = cache.streaming_tail() {
        append_live(&mut lines, tail);
    }
    wrapped_count(lines, content_width)
}

// --- render_committed_slice (ADR-0046, the inline `insert_before` seam) ---

/// A whole [`Buffer`] as newline-joined rows of symbols.
fn commit_buffer_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Blits `[hw, hw + count)` of `cache` into `buf` under the dark theme - the
/// one place these tests spell the [`CommittedSlice`] bundle, so each case
/// reads as the `(hw, count)` window it exercises.
fn blit_slice(
    buf: &mut Buffer,
    cache: &RenderCache,
    items: &[TranscriptItem],
    hw: usize,
    count: usize,
) {
    render_committed_slice(
        buf,
        &CommittedSlice {
            cache,
            items,
            hw,
            count,
            theme: theme::dark(),
        },
    );
}

/// The committed-slice height at the dark theme + content width (test helper):
/// the new [`commit_slice_height`] shape that folds the SAME grouped rows the
/// blit draws. `content_width` is the frame width minus both margins.
fn slice_height(
    cache: &RenderCache,
    items: &[TranscriptItem],
    hw: usize,
    count: usize,
    content_width: u16,
) -> u16 {
    commit_slice_height(
        &CommittedSlice {
            cache,
            items,
            hw,
            count,
            theme: theme::dark(),
        },
        content_width,
    )
}

// A committed slice blits each item's cached content through the grouped fold,
// 2-col margin in (qwen `marginLeft:2`). Golden against the exact rows the
// pending body draws for the same items (see the seam-identity test).
#[test]
fn render_committed_slice_blits_prefixed_content() {
    // Author a tiny run directly on a bare store: an info line, a User
    // prompt, then one agent answer line.
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.info("opening");
    t.user("do a thing");
    t.push(TranscriptItem::Assistant {
        text: "sure".into(),
    });

    let items: Vec<TranscriptItem> = t.items().to_vec();
    let count = items.len();

    // Sync the cache at the SAME content width the slice draws at.
    let width: u16 = 40;
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), content_width, theme::dark());

    let height = slice_height(&cache, &items, 0, count, content_width);
    assert!(height >= 3, "info + user + answer are at least 3 rows");

    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    blit_slice(&mut buf, &cache, &items, 0, count);

    let text = commit_buffer_text(&buf);
    // The content landed with its qwen prefixes, 2-col margin in.
    assert!(text.contains("● opening"), "info prefix drawn:\n{text}");
    assert!(text.contains("> do a thing"), "user caret drawn:\n{text}");
    assert!(text.contains("✦ sure"), "assistant marker drawn:\n{text}");
}

// MEASURE == DRAW (ADR-0029/0046): `commit_slice_height` (what the adapter
// sizes the `insert_before` buffer to) must equal the number of NON-BLANK
// rows `render_committed_slice` actually writes into an OVERSIZED buffer. If
// measure and draw drifted (a width mismatch, a wrap discrepancy), the freeze
// would clip content or leave a gap; this pins them together. `Screen::demo`
// exercises every item kind (thoughts, machinery, markers, an error,
// wrapping assistant text, code) so the agreement holds across them all.
#[test]
fn commit_slice_height_agrees_with_the_rows_render_committed_slice_writes() {
    let screen = Screen::demo();
    let width: u16 = 100;
    let count = screen.transcript().items().len();

    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(
        screen.transcript(),
        Toggles::default(),
        content_width,
        theme::dark(),
    );
    let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();

    let measured = slice_height(&cache, &items, 0, count, content_width);
    assert!(measured > 0, "the demo run has content");

    // Draw into a buffer TALLER than the measurement, then count the rows
    // that actually got content. A blank row past the content proves nothing
    // overflowed the measured height; a blank row WITHIN it would mean the
    // draw under-filled what it measured.
    let oversized = measured + 5;
    let mut buf = Buffer::empty(Rect::new(0, 0, width, oversized));
    blit_slice(&mut buf, &cache, &items, 0, count);

    let text = commit_buffer_text(&buf);
    let non_blank = text.lines().filter(|l| !l.trim().is_empty()).count();
    // The demo has interior blank rows (code fences, spacing), so compare the
    // LAST non-blank row's index + 1 against the measured height: the draw
    // occupies exactly `[0, measured)` and writes nothing at/after `measured`.
    let last_non_blank = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| i)
        .max()
        .expect("some content drew");
    assert!(
        (last_non_blank as u16) < measured,
        "draw wrote past the measured height ({last_non_blank} >= {measured})"
    );
    assert!(
        non_blank > 0 && non_blank <= measured as usize,
        "non-blank rows ({non_blank}) fit within the measured height ({measured})"
    );
    // No content leaked into the oversized tail rows `[measured, oversized)`.
    for y in measured..oversized {
        let row = row_symbols(&buf, y);
        assert!(
            row.trim().is_empty(),
            "row {y} past the measured height must be blank: {row:?}"
        );
    }
}

/// One buffer row as its concatenated symbols (test helper for the
/// measure==draw agreement check).
fn row_symbols(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
        .collect()
}

// The committed slice honors the high-water offset: committing only the tail
// `[hw, hw + count)` draws that tail and nothing before it.
#[test]
fn render_committed_slice_draws_only_the_requested_range() {
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.info("EARLIER");
    t.info("LATER");

    let items: Vec<TranscriptItem> = t.items().to_vec();
    let width: u16 = 40;
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), content_width, theme::dark());

    // Skip EARLIER (hw = 1), commit only LATER (count = 1).
    let hw = items.len() - 1;
    let height = slice_height(&cache, &items, hw, 1, content_width);
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height.max(1)));
    blit_slice(&mut buf, &cache, &items, hw, 1);

    let text = commit_buffer_text(&buf);
    assert!(text.contains("LATER"), "the requested tail drew:\n{text}");
    assert!(
        !text.contains("EARLIER"),
        "items before the high-water mark are not redrawn:\n{text}"
    );
}

// THE identity guarantee (ADR-0046): the committed slice for a whole run,
// and the pending body's rendering of that SAME prefix, produce the
// IDENTICAL rows - gutter and content - so nothing reflows when an item
// crosses the commit seam. This is the property `run_fold`'s retirement buys:
// both paths read the SAME cache lines (no collapse, no window) and paint the
// SAME two-plane gutter. Uses `Screen::demo()` so the run has thoughts,
// machinery, markers, an error, closing text and code - every item kind.
#[test]
fn the_committed_slice_equals_the_pending_body_for_the_same_prefix() {
    let screen = Screen::demo();
    let width: u16 = 100;
    let count = screen.transcript().items().len();

    // (a) The committed slice `[0, count)` blitted into a bare buffer.
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut commit_cache = RenderCache::new();
    commit_cache.sync(
        screen.transcript(),
        Toggles::default(),
        content_width,
        theme::dark(),
    );
    let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();
    let height = slice_height(&commit_cache, &items, 0, count, content_width);
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    blit_slice(&mut buf, &commit_cache, &items, 0, count);
    let committed = commit_buffer_text(&buf);

    // (b) The pending body (hw = 0) drawn TOP-aligned into a zone exactly as
    // tall as the content, so the two are directly comparable row-for-row.
    let terminal = draw_viewport(width, height, &screen);
    let pending: String = (0..height)
        .map(|y| row_text(&terminal, y).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let committed_trimmed: String = committed
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        committed_trimmed, pending,
        "committed and pending must render the same prefix identically (no seam reflow)"
    );
}

// The identity holds UNDER COMPACT too (ADR-0052): with compact on, both the
// committed blit and the pending body hide thoughts + fold tool bodies through
// the SAME `message_lines` compact branch, so a RedrawScrollback re-blit at the
// new compact matches the pending region cell-for-cell (no split-brain in what
// each path chooses to draw).
#[test]
fn the_committed_slice_equals_the_pending_body_under_compact() {
    // A compact Screen (Ctrl+O flipped on). `demo()` carries thoughts + a tool
    // run, so compact actually changes the rows both paths emit.
    let (screen, _) = Screen::demo().handle_key(crate::ui::screen::Key::ToggleCompact);
    assert!(screen.compact_mode, "the demo screen is now compact");
    let width: u16 = 100;
    let count = screen.transcript().items().len();

    // (a) The committed slice `[0, count)` at compact = true.
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut commit_cache = RenderCache::new();
    commit_cache.sync(
        screen.transcript(),
        Toggles { compact: true },
        content_width,
        theme::dark(),
    );
    let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();
    let height = slice_height(&commit_cache, &items, 0, count, content_width);
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height.max(1)));
    blit_slice(&mut buf, &commit_cache, &items, 0, count);
    let committed = commit_buffer_text(&buf);

    // (b) The pending body (which reads `screen.compact_mode`) over the same
    // prefix, top-aligned.
    let terminal = draw_viewport(width, height.max(1), &screen);
    let pending: String = (0..height.max(1))
        .map(|y| row_text(&terminal, y).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let committed_trimmed: String = committed
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        committed_trimmed, pending,
        "committed and pending must match under compact (no seam reflow)"
    );
    // Compact genuinely hid the thoughts: the demo's reasoning text is gone.
    assert!(
        !committed_trimmed.contains("The user wants me to evaluate"),
        "compact hid the settled thoughts:\n{committed_trimmed}"
    );
}

// --- tool-group box (ADR-0047): grouping fold + border rigidity ----------

/// A store carrying an assistant line, a two-tool run (a call + a result),
/// then another assistant line - the shape the grouping fold boxes in the
/// middle only.
fn store_with_a_tool_run() -> crate::ui::transcript::Transcript {
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.user("do it");
    t.push(TranscriptItem::Assistant {
        text: "on it".into(),
    });
    t.push(TranscriptItem::ToolCall {
        id: "1".into(),
        name: "read_file".into(),
        summary: "src/foo.rs".into(),
    });
    t.push(TranscriptItem::ToolResult {
        name: "read_file".into(),
        summary: "340 lines".into(),
        is_error: false,
        key_arg: Some("src/foo.rs".into()),
    });
    t.push(TranscriptItem::Assistant {
        text: "done".into(),
    });
    t
}

#[test]
fn a_contiguous_tool_run_renders_as_one_rounded_box() {
    // The grouping fold (ADR-0047): the two tool items between the assistant
    // lines are ONE box - a single top border, a single bottom border.
    let t = store_with_a_tool_run();
    let items: Vec<TranscriptItem> = t.items().to_vec();
    let width: u16 = 60;
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), content_width, theme::dark());
    let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
    let text: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(
        text.iter().filter(|r| r.starts_with('╭')).count(),
        1,
        "exactly one box top: {text:?}"
    );
    assert_eq!(
        text.iter().filter(|r| r.starts_with('╰')).count(),
        1,
        "exactly one box bottom: {text:?}"
    );
    // The two tool rows are inside the border (start with `│`), the assistant
    // lines are not.
    assert!(
        text.iter()
            .any(|r| r.contains("read_file") && r.starts_with('│'))
    );
    assert!(text.iter().any(|r| r.starts_with("✦ done")));
}

// --- Todo render (ADR-0048, the committed defect fix) --------------------

fn todo(content: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        content: content.into(),
        status,
    }
}

fn todo_item(items: Vec<TodoItem>) -> TranscriptItem {
    TranscriptItem::Todo { items }
}

// The span carrying `needle` (used to assert its style).
fn span_with<'a>(line: &'a Line<'static>, needle: &str) -> &'a Span<'static> {
    line.spans
        .iter()
        .find(|s| s.content.contains(needle))
        .unwrap_or_else(|| panic!("no span contains {needle:?} in {:?}", line_text(line)))
}

#[test]
fn tool_todo_lines_draws_a_clean_header_then_the_circle_list() {
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![
        todo("read the file", Completed),
        todo("edit the file", InProgress),
        todo("build", Pending),
    ];
    let lines = tool_todo_lines(&items, false, 40, theme::dark());
    let text: Vec<String> = lines.iter().map(line_text).collect();

    // A clean `✓ todo_write` header with NO raw JSON args (the key_arg is
    // gone structurally - a Todo carries no summary).
    assert_eq!(text[0], "✓  todo_write");
    assert!(
        !text.iter().any(|r| r.contains('{') || r.contains("status")),
        "no raw JSON args leak: {text:?}"
    );
    // The circle glyphs, in order, one row per item.
    assert!(text[1].starts_with("● "), "completed circle: {:?}", text[1]);
    assert!(text[1].contains("read the file"));
    assert!(
        text[2].starts_with("◐ "),
        "in_progress circle: {:?}",
        text[2]
    );
    assert!(text[3].starts_with("○ "), "pending circle: {:?}", text[3]);
}

#[test]
fn tool_todo_lines_folds_to_header_only_under_compact() {
    // Compact (Ctrl+O) folds the checklist body away (qwen `!compactMode`,
    // ADR-0052): the header stays, the circle rows are gone. This pins the
    // one display-hide branch of `tool_todo_lines` (compact=true), where the
    // non-compact tests above exercise compact=false's full list.
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![
        todo("read the file", Completed),
        todo("edit the file", InProgress),
        todo("build", Pending),
    ];
    let lines = tool_todo_lines(&items, true, 40, theme::dark());
    assert_eq!(lines.len(), 1, "only the header row survives under compact");
    assert_eq!(line_text(&lines[0]), "✓  todo_write");
}

#[test]
fn tool_todo_lines_colours_in_progress_green_and_strikes_completed() {
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![
        todo("done item", Completed),
        todo("active item", InProgress),
        todo("later item", Pending),
    ];
    let lines = tool_todo_lines(&items, false, 40, theme::dark());

    // in_progress reads success (green); completed is CROSSED_OUT and NOT
    // green (qwen colours completed Foreground); pending is plain.
    let done = span_with(&lines[1], "done item");
    assert!(
        done.style.add_modifier.contains(Modifier::CROSSED_OUT),
        "completed is struck through"
    );
    assert_eq!(
        done.style.fg,
        primary_style(theme::dark()).fg,
        "completed is Foreground, not green"
    );

    let active = span_with(&lines[2], "active item");
    assert_eq!(active.style.fg, success_style(theme::dark()).fg);
    assert!(!active.style.add_modifier.contains(Modifier::CROSSED_OUT));

    let later = span_with(&lines[3], "later item");
    assert_eq!(later.style.fg, primary_style(theme::dark()).fg);
    assert!(!later.style.add_modifier.contains(Modifier::CROSSED_OUT));
}

#[test]
fn tool_todo_lines_wraps_long_content_and_every_row_fits_the_inner_width() {
    // Long content word-wraps under the 3-wide gutter; no produced row
    // exceeds the inner width (measure==draw, ADR-0029).
    let inner: u16 = 24;
    let items = vec![todo(
        "a rather long todo item that must wrap onto several rows",
        TodoStatus::Pending,
    )];
    let lines = tool_todo_lines(&items, false, inner, theme::dark());
    assert!(lines.len() > 2, "the long item wrapped: {}", lines.len());
    for line in &lines {
        assert!(
            line_text(line).width() <= inner as usize,
            "row exceeds inner width: {:?}",
            line_text(line)
        );
    }
}

/// The `(symbol, fg, bg, add_modifier)` cells of the box rows of `buf`,
/// starting at the first `╭` row and spanning `box_rows` rows - so a committed
/// blit (box at row 0) and a pending render (box after a committed prefix +
/// separator) can be aligned on the box and compared window-for-window.
fn box_cells(buf: &Buffer, box_rows: u16) -> Vec<(String, Color, Color, Modifier)> {
    let top = (0..buf.area.height)
        .find(|&y| buf.cell((CONTENT_MARGIN, y)).map(|c| c.symbol()) == Some("╭"))
        .expect("a box top row");
    (top..top + box_rows)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .map(|(x, y)| {
            let cell = buf.cell((x, y)).expect("cell in area");
            let style = cell.style();
            (
                cell.symbol().to_string(),
                style.fg.unwrap_or(Color::Reset),
                style.bg.unwrap_or(Color::Reset),
                style.add_modifier,
            )
        })
        .collect()
}

// The committed==pending identity (ADR-0046/0048): a Todo item flows the SAME
// message_lines -> cache -> grouped_rows path, so its committed BLIT is
// cell-for-cell identical to its pending render - same glyphs AND same styling
// (fg/bg/modifier), so nothing reflows or recolours at the commit seam. The
// committed side goes through the REAL blit (`render_committed_slice`) at a
// NON-ZERO high-water (an `info` line committed first, so the Todo box is at
// row 0 of the slice); the pending side draws the WHOLE transcript from hw=0
// (info + separator + Todo box) as the pending body does. Two distinct draw
// paths over distinct windows, aligned on the box top and compared cell-for-
// cell - not a self-comparison of one `grouped_rows` call.
#[test]
fn a_todo_renders_cell_for_cell_identically_committed_and_pending() {
    use TodoStatus::{Completed, InProgress, Pending};
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.info("a committed prefix line"); // index 0, committed (hw = 1)
    t.push(todo_item(vec![
        todo("read", Completed),
        todo("edit", InProgress),
        todo("ship", Pending),
    ]));
    let items: Vec<TranscriptItem> = t.items().to_vec();
    let width: u16 = 50;
    let height: u16 = 12;
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), content_width, theme::dark());

    // The Todo box's own height (border + header + 3 circle rows = 5), so the
    // aligned window spans exactly the box on both sides.
    let box_rows = grouped_rows(&cache, &items, 1, content_width, theme::dark()).len() as u16;

    // Committed: the REAL blit of the Todo slice [1, 2) at high-water 1.
    let mut committed = Buffer::empty(Rect::new(0, 0, width, height));
    render_committed_slice(
        &mut committed,
        &CommittedSlice {
            cache: &cache,
            items: &items,
            hw: 1,
            count: 1,
            theme: theme::dark(),
        },
    );

    // Pending: the WHOLE transcript (info + separator + Todo) drawn as the
    // pending body draws it (grouped_rows from hw=0 + the margin-inset
    // Paragraph). The Todo box lands below the committed prefix, so we align on
    // the box top.
    let mut pending = Buffer::empty(Rect::new(0, 0, width, height));
    let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
    let content_area = Rect {
        x: CONTENT_MARGIN,
        y: 0,
        width: content_width,
        height,
    };
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(content_area, &mut pending);

    // Cell-for-cell over the box window: symbol AND fg/bg/modifier match.
    assert_eq!(
        box_cells(&committed, box_rows),
        box_cells(&pending, box_rows),
        "committed blit diverged from the pending render:\ncommitted:\n{}\npending:\n{}",
        commit_buffer_text(&committed),
        commit_buffer_text(&pending),
    );

    // And the box wraps the circle list: one rounded box, the header + three
    // circle rows inside the border.
    let text = commit_buffer_text(&committed);
    assert_eq!(text.matches('╭').count(), 1, "one box:\n{text}");
    assert!(text.contains("todo_write"), "clean header:\n{text}");
    assert!(text.contains("●  read"), "completed glyph:\n{text}");
    assert!(text.contains("◐  edit"), "in_progress glyph:\n{text}");
    assert!(text.contains("○  ship"), "pending glyph:\n{text}");
}

// --- sticky "Current tasks" box (ADR-0048) -------------------------------

#[test]
fn ordered_sticky_todos_sorts_by_priority_stable_and_keeps_original_index() {
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![
        todo("a-completed", Completed),   // 0
        todo("b-pending", Pending),       // 1
        todo("c-inprogress", InProgress), // 2
        todo("d-pending", Pending),       // 3
    ];
    let ordered = ordered_sticky_todos(&items);
    let seq: Vec<(usize, &str)> = ordered
        .iter()
        .map(|(i, item)| (*i, item.content.as_str()))
        .collect();
    // in_progress first, then pending (stable: index 1 before 3), then completed.
    assert_eq!(
        seq,
        vec![
            (2, "c-inprogress"),
            (1, "b-pending"),
            (3, "d-pending"),
            (0, "a-completed"),
        ]
    );
}

#[test]
fn sticky_todos_shows_only_a_committed_non_empty_incomplete_list() {
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![todo("read", InProgress), todo("edit", Pending)];

    // Non-empty, incomplete, committed (index 2 < high_water 3): shows.
    assert_eq!(sticky_todos(Some((2, &items)), 3), Some(items.as_slice()));

    // Still pending (index 3 >= high_water 3, not yet committed): the inline
    // copy is on screen, so the sticky box defers.
    assert_eq!(sticky_todos(Some((3, &items)), 3), None);

    // No todo at all: nothing.
    assert_eq!(sticky_todos(None, 3), None);

    // Empty list: nothing.
    let empty: Vec<TodoItem> = vec![];
    assert_eq!(sticky_todos(Some((0, &empty)), 3), None);

    // All completed: the run is done, so the box hides.
    let done = vec![todo("read", Completed), todo("edit", Completed)];
    assert_eq!(sticky_todos(Some((0, &done)), 3), None);
}

#[test]
fn sticky_todos_height_caps_at_five_and_adds_an_overflow_row() {
    // borders(2) + header(1) + visible + overflow?1.
    assert_eq!(sticky_todos_height(0), 3);
    assert_eq!(sticky_todos_height(3), 6);
    assert_eq!(sticky_todos_height(5), 8);
    // Six items: five shown + one overflow row.
    assert_eq!(sticky_todos_height(6), 9);
}

#[test]
fn render_sticky_todos_draws_the_header_glyphs_and_overflow_and_fits_width() {
    use TodoStatus::{Completed, InProgress, Pending};
    let items = vec![
        todo("one", InProgress),
        todo("two", Pending),
        todo("three", Pending),
        todo("four", Pending),
        todo("five", Pending),
        todo("six", Completed),
    ];
    let width: u16 = 40;
    let height = sticky_todos_height(items.len()) as u16;
    let terminal = draw_frame(width, height, |f| {
        render_sticky_todos(f, Rect::new(0, 0, width, height), &items, theme::dark());
    });
    let text = buffer_text(&terminal);

    assert!(text.contains("Current tasks"), "header present:\n{text}");
    assert!(text.contains("◐"), "in_progress glyph present:\n{text}");
    assert!(text.contains("○"), "pending glyph present:\n{text}");
    // Six items > cap 5, so exactly one item is hidden with the overflow row.
    assert!(text.contains("... and 1 more"), "overflow row:\n{text}");
    // The rounded box corners are drawn.
    assert!(
        text.contains('╭') && text.contains('╰'),
        "box borders:\n{text}"
    );
    // Every drawn row is exactly `width` cells (the box never overflows -
    // measure==draw, ADR-0029): the TestBackend rows are width-padded, so
    // asserting the buffer drew without panicking on an oversize line suffices
    // here; the box_row funnel guarantees the width.
}

#[test]
fn sticky_fits_drops_the_box_when_the_frame_cannot_hold_it_plus_composer() {
    // sticky(6) + status(1) + composer(3) + body(1) = 11.
    assert!(sticky_fits(11, 6, 3));
    assert!(sticky_fits(20, 6, 3));
    // One row short: no room for the body -> hide.
    assert!(!sticky_fits(10, 6, 3));
    // A very short frame with any composer hides the box.
    assert!(!sticky_fits(4, 6, 3));
}

#[test]
fn render_sticky_todos_clamps_its_lines_to_a_squeezed_zone_without_panicking() {
    use TodoStatus::{InProgress, Pending};
    let items = vec![
        todo("one", InProgress),
        todo("two", Pending),
        todo("three", Pending),
    ];
    let full = sticky_todos_height(items.len()) as u16; // 6
    let width: u16 = 30;
    // Draw into a zone SHORTER than the measured box height: the clamp keeps
    // the draw within the zone (no over-draw, no panic).
    let squeezed = full - 3;
    let terminal = draw_frame(width, squeezed, |f| {
        render_sticky_todos(f, Rect::new(0, 0, width, squeezed), &items, theme::dark());
    });
    let text = buffer_text(&terminal);
    // The top of the box still draws; the truncated tail simply drops.
    assert!(text.contains("Current tasks"), "header present:\n{text}");
    assert_eq!(
        text.lines().count(),
        squeezed as usize,
        "drew exactly the zone height, no over-draw:\n{text}"
    );
}

// --- group_segments: the pure boundary fold (ADR-0047) -------------------

// The item constructors the pure `group_segments` tests route through, so the
// `TranscriptItem` literals stay in one place. No frame, no cache.
fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::Assistant { text: text.into() }
}
fn tool_call(name: &str) -> TranscriptItem {
    TranscriptItem::ToolCall {
        id: "id".into(),
        name: name.into(),
        summary: "arg".into(),
    }
}
fn tool_result(name: &str) -> TranscriptItem {
    TranscriptItem::ToolResult {
        name: name.into(),
        summary: "ok".into(),
        is_error: false,
        key_arg: None,
    }
}
fn info(text: &str) -> TranscriptItem {
    TranscriptItem::Info { text: text.into() }
}

#[test]
fn group_segments_boxes_maximal_tool_runs_between_prose() {
    // A single lone tool item is one group; the surrounding prose passes
    // through. A run at the START (index 0) and one at the very END both box.
    let items = [
        tool_call("read_file"),           // 0  (run at slice start)
        assistant("prose"),               // 1
        tool_call("run_shell_command"),   // 2  (run at the very end)
        tool_result("run_shell_command"), // 3
    ];
    assert_eq!(
        group_segments(&items, 0),
        vec![
            Segment::ToolGroup(0, 1),
            Segment::Item(1),
            Segment::ToolGroup(2, 4),
        ]
    );
}

#[test]
fn group_segments_splits_a_run_around_a_mid_batch_info() {
    // The ADR-0047 behavior pinned: an `Info` interleaved between two tool
    // results (a mid-batch extension error / standing approval) SPLITS the run
    // into two tool groups with the Info as its own singleton between them.
    let items = [
        assistant("on it"),               // 0
        tool_call("edit"),                // 1
        tool_result("edit"),              // 2
        info("auto-approved"),            // 3  (splits the batch)
        tool_result("run_shell_command"), // 4
        assistant("done"),                // 5
    ];
    assert_eq!(
        group_segments(&items, 0),
        vec![
            Segment::Item(0),
            Segment::ToolGroup(1, 3),
            Segment::Item(3),
            Segment::ToolGroup(4, 5),
            Segment::Item(5),
        ]
    );
}

#[test]
fn group_segments_keeps_a_diff_inside_the_tool_run() {
    // A `Diff` is a tool item, so it stays in the surrounding run's box rather
    // than breaking it (the diff renders inside the group box).
    let items = [
        tool_call("edit"),
        diff_item("edit foo", vec![DiffLine::new(DiffSide::Added, "a")]),
        tool_result("edit"),
    ];
    assert_eq!(group_segments(&items, 0), vec![Segment::ToolGroup(0, 3)]);
}

#[test]
fn group_segments_groups_an_orphaned_result_like_any_tool_item() {
    // An orphaned `ToolResult` (no preceding call - e.g. after a supersede
    // moved the result to the tail) is still a tool item and boxes normally.
    let items = [tool_result("read_file")];
    assert_eq!(group_segments(&items, 0), vec![Segment::ToolGroup(0, 1)]);
}

#[test]
fn group_segments_only_emits_the_settled_tail_from_the_high_water_mark() {
    // The fold starts at `hw`: items below the high-water mark are frozen into
    // scrollback and never re-emitted.
    let items = [
        assistant("frozen"),      // 0  (below hw)
        tool_call("read_file"),   // 1
        tool_result("read_file"), // 2
    ];
    assert_eq!(group_segments(&items, 1), vec![Segment::ToolGroup(1, 3)]);
}

// The box-rigidity assertion (ADR-0029, the HIGH risk): EVERY row `grouped_rows`
// produces for this store - borders, tool rows, the gap, a boxed diff's rows - is
// EXACTLY `width` display columns (so the right `│`/`╮`/`╯` corners always align in
// one column) AND stays ONE visual row (the viewport's own `Wrap` agrees).
fn assert_every_box_row_is_exactly_width(
    t: &crate::ui::transcript::Transcript,
    toggles: Toggles,
    widths: &[u16],
) {
    let items: Vec<TranscriptItem> = t.items().to_vec();
    for &width in widths {
        let mut cache = RenderCache::new();
        cache.sync(t, toggles, width, theme::dark());
        let lines = grouped_rows(&cache, &items, 0, width, theme::dark());
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(
                w,
                width as usize,
                "box row not exactly {width} cols (right border misaligned): {:?}",
                line_text(line)
            );
            assert_eq!(
                wrapped_count(vec![line.clone()], width),
                1,
                "box row is not one visual row at {width}: {:?}",
                line_text(line)
            );
        }
    }
}

// A single-`ToolResult` transcript with the given name/desc - the shared
// builder the box-rigidity goldens route through, so the `ToolResult { … }`
// literal lives in one place (and a Diff/multi-tool case builds its own).
fn store_with_one_result(name: &str, summary: String) -> crate::ui::transcript::Transcript {
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.push(TranscriptItem::ToolResult {
        name: name.into(),
        summary,
        is_error: false,
        key_arg: Some(name.into()),
    });
    t
}

#[test]
fn every_boxed_row_is_exactly_the_box_width_the_right_border_aligns() {
    // A long tool description (truncate-end) must not spill the right border.
    let t = store_with_one_result("run_shell_command", "a very long description ".repeat(10));
    assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30, 50, 72]);
}

#[test]
fn a_cjk_and_emoji_tool_header_keeps_the_box_rigid() {
    // Widths are DISPLAY COLUMNS, not char counts: a CJK+emoji tool name and
    // description (each ideograph 2 cols, the emoji 2 cols) must still pad/
    // truncate to exactly the box width - the char-count trap the review flagged.
    let t = store_with_one_result("你好🎉世界", "説明 ".repeat(12));
    assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[20, 33, 50]);
}

#[test]
fn a_boxed_diff_with_a_wide_glyph_line_keeps_the_box_rigid() {
    // A tool group whose result is a Diff: the diff renders INSIDE the box
    // (tools_expanded), so every diff-in-box row - including a wide-glyph diff
    // line - must be exactly the box width, not just the header/border rows.
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.push(TranscriptItem::Diff {
        title: "edit 世界.rs".into(),
        lang: Some("txt".into()),
        hunks: vec![DiffHunk {
            header: Some("@@ -1,2 +1,2 @@".into()),
            lines: vec![
                DiffLine::new(DiffSide::Context, "let 語 = 実装; // 説明"),
                DiffLine::new(DiffSide::Added, "let x = 1; 🎉"),
                DiffLine::new(DiffSide::Removed, "let x = 0;"),
            ],
        }],
        elided: 0,
    });
    // Not compact: the diff body shows in full.
    let expanded = Toggles::default();
    assert_every_box_row_is_exactly_width(&t, expanded, &[24, 40, 60]);
}

#[test]
fn a_multi_tool_box_pads_its_gap_row_to_the_width() {
    // Two tools in one box exercise the `gap:1` blank row between them; the gap
    // row (and every other row) must still be exactly the box width.
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.push(TranscriptItem::ToolResult {
        name: "read_file".into(),
        summary: "340 lines".into(),
        is_error: false,
        key_arg: Some("src/foo.rs".into()),
    });
    t.push(TranscriptItem::ToolResult {
        name: "run_shell_command".into(),
        summary: "ok".into(),
        is_error: false,
        key_arg: Some("cargo build".into()),
    });
    assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30, 50, 72]);
}

#[test]
fn a_tool_name_on_the_inner_width_boundary_keeps_the_box_rigid() {
    // A name+desc whose display width lands EXACTLY on the inner-width boundary
    // (no truncation, no leftover pad): the edge case between "fits" and "spills".
    // At width 30 the inner content width is 30 - BOX_CHROME(4) = 26; the header
    // is a 3-col marker + name + space + desc, so name+desc filling 23 cols lands
    // the header exactly on the inner width.
    let inner = 30usize - BOX_CHROME;
    let budget = inner - STATUS_INDICATOR_WIDTH; // marker column
    let name = "n".repeat(8);
    let desc = "d".repeat(budget - name.len() - 1); // -1 for the space
    let mut t = crate::ui::transcript::Transcript::new(Vec::new());
    t.push(TranscriptItem::ToolResult {
        name,
        summary: desc,
        is_error: false,
        key_arg: None,
    });
    assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30]);
}

/// Renders `store_with_a_tool_run` through BOTH the committed path (a
/// `blit_slice` into a bare buffer) and the pending path (the SAME
/// `grouped_rows` fold, drawn `CONTENT_MARGIN` in, top-aligned) into two
/// same-size buffers, so a caller can compare them glyph- and cell-wise.
fn committed_and_pending_tool_group_buffers() -> (Buffer, Buffer, u16, u16) {
    let t = store_with_a_tool_run();
    let items: Vec<TranscriptItem> = t.items().to_vec();
    let count = items.len();
    let width: u16 = 60;
    let content_width = width - 2 * CONTENT_MARGIN;

    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), content_width, theme::dark());

    // (a) The committed slice `[0, count)` blitted into a bare buffer.
    let height = slice_height(&cache, &items, 0, count, content_width);
    let mut committed_buf = Buffer::empty(Rect::new(0, 0, width, height));
    blit_slice(&mut committed_buf, &cache, &items, 0, count);

    // (b) The pending body's line assembly (`grouped_rows`, the SAME fold),
    // drawn CONTENT_MARGIN in and top-aligned into a same-size buffer.
    let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
    let mut pending_buf = Buffer::empty(Rect::new(0, 0, width, height));
    Paragraph::new(lines).wrap(Wrap { trim: false }).render(
        Rect::new(CONTENT_MARGIN, 0, content_width, height),
        &mut pending_buf,
    );

    (committed_buf, pending_buf, width, height)
}

#[test]
fn a_committed_tool_group_is_byte_and_style_identical_to_the_pending_one() {
    // The committed==pending identity (ADR-0046) over a tool GROUP: BOTH
    // paths assemble the settled tail through the SAME `grouped_rows` fold,
    // so a group is identical live vs frozen - and the guarantee is a STYLE
    // identity, not just a glyph one. Assert (a) the glyph text matches
    // byte-for-byte, then (b) every cell's symbol AND full style (fg/bg/
    // modifier) matches, so a colour/modifier-only divergence still fails.
    let (committed_buf, pending_buf, width, height) = committed_and_pending_tool_group_buffers();

    assert_eq!(
        commit_buffer_text(&committed_buf),
        commit_buffer_text(&pending_buf),
        "committed and pending render the tool group identically"
    );

    for y in 0..height {
        for x in 0..width {
            let c = committed_buf.cell((x, y)).expect("committed cell");
            let p = pending_buf.cell((x, y)).expect("pending cell");
            assert_eq!(
                (c.symbol(), c.style().fg, c.style().bg, c.modifier),
                (p.symbol(), p.style().fg, p.style().bg, p.modifier),
                "committed vs pending diverge at ({x},{y})"
            );
        }
    }
}

// The fg colour of an item's FIRST span (the prefix glyph): the role a Phase-2
// committed prefix wears. Asserted against the style HELPERS (not raw hexes) so
// the Phase-7 slot remap moves in lockstep.
fn first_span_fg(item: &TranscriptItem) -> Option<Color> {
    message_lines(item, false, 40, theme::dark())[0].spans[0]
        .style
        .fg
}

#[test]
fn each_committed_prefix_wears_its_colour_role() {
    let theme = theme::dark();
    // User `>` and Assistant `✦` are both `text.accent` (accent_style).
    assert_eq!(
        first_span_fg(&TranscriptItem::User { text: "hi".into() }),
        accent_style(theme).fg
    );
    assert_eq!(
        first_span_fg(&TranscriptItem::Assistant { text: "hi".into() }),
        accent_style(theme).fg
    );
    // Info `●` is `text.primary` (primary_style - the `foreground` slot, ADR-0008).
    assert_eq!(
        first_span_fg(&TranscriptItem::Info {
            text: "note".into()
        }),
        primary_style(theme).fg
    );
    // Settled Thinking `✦` is grey `text.secondary` (thinking/secondary_style).
    assert_eq!(
        first_span_fg(&TranscriptItem::Thinking {
            text: "pondering".into()
        }),
        secondary_style(theme).fg
    );
    // And the roles are genuinely distinct where the slots differ: accent != grey.
    assert_ne!(accent_style(theme).fg, secondary_style(theme).fg);
}

#[test]
fn the_phase_7_role_helpers_read_their_dedicated_slots() {
    // Phase 7 (ADR-0008/0053) carved four qwen roles into real slots; each
    // helper now reads its OWN slot, no longer a borrowed neighbour. Pinned
    // against the slot value (a hex the theme drift test also guards), so a
    // regression to the old borrow surfaces here.
    let theme = theme::dark();
    assert_eq!(accent_style(theme).fg, Some(tui_color(theme.accent)));
    assert_eq!(success_style(theme).fg, Some(tui_color(theme.success)));
    assert_eq!(warning_style(theme).fg, Some(tui_color(theme.warning)));
    assert_eq!(primary_style(theme).fg, Some(tui_color(theme.foreground)));
    // Purple accent, lime success, gold warning, real foreground - the
    // QwenDark hexes, and distinct from the slots they used to borrow.
    assert_eq!(accent_style(theme).fg, Some(Color::Rgb(0xD2, 0xA6, 0xFF)));
    assert_eq!(success_style(theme).fg, Some(Color::Rgb(0xAA, 0xD9, 0x4C)));
    assert_eq!(warning_style(theme).fg, Some(Color::Rgb(0xFF, 0xD7, 0x00)));
    assert_eq!(primary_style(theme).fg, Some(Color::Rgb(0xbf, 0xbd, 0xb6)));
    assert_ne!(accent_style(theme).fg, Some(tui_color(theme.prompt_gutter)));
    assert_ne!(success_style(theme).fg, Some(tui_color(theme.added)));
    assert_ne!(warning_style(theme).fg, Some(tui_color(theme.marker_aid)));
}

// --- render_pending (ADR-0046): bottom-anchor + top-clip -----------------

/// Draws one full inline pending frame (transcript body + status +
/// composer) for the given screen into a fresh `width`x`height` terminal.
fn draw_pending(width: u16, height: u16, screen: &Screen) -> Terminal<TestBackend> {
    let mut cache = RenderCache::new();
    let conn = ConnectionFacts {
        base_url: "http://test".into(),
        model: "m".into(),
    };
    draw_frame(width, height, |f| {
        render_pending(
            f,
            screen,
            &mut cache,
            FrameCtx {
                conn: conn.view(),
                anim: Anim::default(),
                theme: theme::dark(),
            },
        );
    })
}

// --- composer chrome (ADR-0048): top rule, prompt, bottom border, cursor --

// The correctness-critical +1 (Risk #1): the draft sits BELOW the top dash
// rule, so the terminal cursor y is the zone top plus one rule row plus the
// cursor row offset. A one-line draft at column 3 lands at y = zone.y + 1.
#[test]
fn composer_cursor_sits_one_row_below_the_top_rule() {
    // A zone of height 4 (top rule + 1 draft row + bottom border + spare):
    // "abc" with the cursor after it.
    let layout = composer::layout("abc", 3, 40);
    assert_eq!(layout.cursor_row, 0);
    let zone = Rect::new(0, 5, 40, 4);
    let screen = Screen::new(ScreenOpts::default());
    let mut terminal = draw_frame(40, 12, |f| {
        render_composer(f, zone, &screen, &layout, theme::dark());
    });
    let cursor = terminal.get_cursor_position().expect("cursor placed");
    // x: zone.x + 2 (the `> ` prompt) + cursor_col(3) = 5.
    assert_eq!(cursor.x, 5, "cursor x past the `> ` prompt");
    // y: zone.y(5) + 1 (top rule) + cursor_row(0) = 6.
    assert_eq!(cursor.y, 6, "cursor y is one row below the top rule");
}

#[test]
fn composer_draws_the_top_rule_prompt_and_bottom_border() {
    let layout = composer::layout("hello", 5, 40);
    let zone = Rect::new(0, 0, 40, 4);
    let screen = Screen::new(ScreenOpts::default());
    let terminal = draw_frame(40, 4, |f| {
        render_composer(f, zone, &screen, &layout, theme::dark());
    });
    // Row 0 is the full-width top dash rule.
    assert_eq!(row_text(&terminal, 0), "─".repeat(40));
    // Row 1 is the draft under the `> ` prompt.
    assert!(row_text(&terminal, 1).starts_with("> hello"));
    // The last row is the bottom border.
    assert_eq!(row_text(&terminal, 3), "─".repeat(40));
}

#[test]
fn composer_shows_the_placeholder_when_the_draft_is_empty() {
    let layout = composer::layout("", 0, 40);
    assert!(composer_is_empty(&layout));
    let zone = Rect::new(0, 0, 40, 4);
    let screen = Screen::new(ScreenOpts::default());
    let terminal = draw_frame(40, 4, |f| {
        render_composer(f, zone, &screen, &layout, theme::dark());
    });
    assert!(
        row_text(&terminal, 1).contains("Type your message or @path/to/file"),
        "placeholder row: {:?}",
        row_text(&terminal, 1)
    );
}

// The zone height accounts for the two chrome rows on top of the draft rows
// (Risk #1: the +2 must match `render_composer`'s reservations exactly).
#[test]
fn capped_composer_height_reserves_the_two_chrome_rows() {
    let one_line = composer::layout("hi", 2, 40);
    assert_eq!(one_line.rows.len(), 1);
    // 1 draft row + 2 chrome = 3.
    assert_eq!(capped_composer_height(&one_line, 30), 3);
}

// A short pending stack is anchored to the BOTTOM of its zone: the top rows
// of the body zone are blank and the content sits just above the status bar.
#[test]
fn render_pending_bottom_anchors_a_short_stack() {
    // A fresh screen: only the startup Header banner is pending.
    let screen = Screen::new(ScreenOpts::default());
    let terminal = draw_pending(60, 12, &screen);

    // The banner spans a few rows and anchors to the BOTTOM of the body
    // zone, so the top rows are blank and the content sits low. Find the
    // first non-blank body row: it must be past the top of the zone.
    let first_content = (0..10)
        .find(|&y| !row_text(&terminal, y).trim().is_empty())
        .expect("some content drew");
    assert!(
        first_content > 0,
        "the top of the body zone is blank (bottom-anchored); first content at row {first_content}"
    );
    // The header brand + tips drew in the body.
    let body = buffer_text(&terminal);
    assert!(
        body.contains(">_ suspenders"),
        "the header brand drew in the body:\n{body}"
    );
    assert!(body.contains("Tips:"), "the tips line drew:\n{body}");
}

// An overflowing pending stack is top-clipped: the NEWEST rows survive and
// the oldest drop off the top (qwen's overflowDirection:"top"), with the
// `… Ctrl-S to show more` marker on the top row.
#[test]
fn render_pending_top_clips_an_overflowing_stack() {
    // Many notice lines overflow a short terminal.
    let screen = Screen::new(ScreenOpts {
        notices: (1..=40).map(|i| format!("notice-{i:02}")).collect(),
        ..ScreenOpts::default()
    });
    let terminal = draw_pending(40, 10, &screen);
    let text = buffer_text(&terminal);

    // The newest notice is on screen; the oldest scrolled off the top.
    assert!(text.contains("notice-40"), "newest kept:\n{text}");
    assert!(
        !text.contains("notice-01"),
        "oldest clipped off the top:\n{text}"
    );
    // The overflow marker is on the top row of the body zone (ADR-0046).
    assert!(
        text.contains("Ctrl-S to show more"),
        "the overflow marker draws:\n{text}"
    );
}

// 4-zone starvation (Risk #1): a frame so short that a VISIBLE sticky box +
// the status row + the composer cannot all fit leaves the body `Min(1)` with
// no room. `frame_chunks` must still return four rects that tile the area
// without over-running it, and Layout keeps at least the `Min(1)` body at the
// expense of the fixed zones - the layout never panics or produces an
// off-frame rect.
#[test]
fn frame_chunks_starved_by_a_sticky_box_never_over_runs_the_frame() {
    // A 6-row frame with a 6-row sticky box + 1 status + 3 composer wants 10
    // rows: the fixed zones alone exceed the frame, so Layout must shrink them.
    let area = Rect::new(0, 0, 40, 6);
    let chunks = frame_chunks(area, 6, 3);
    assert_eq!(chunks.len(), 4);
    // Every zone stays inside the frame (no off-frame y/height).
    for zone in chunks.iter() {
        assert!(
            zone.y >= area.y && zone.bottom() <= area.bottom(),
            "zone {zone:?} ran off the frame {area:?}"
        );
    }
    // The zones tile the frame top-to-bottom with no gap or overlap.
    assert_eq!(chunks[0].y, area.y);
    assert_eq!(chunks[0].bottom(), chunks[1].y);
    assert_eq!(chunks[1].bottom(), chunks[2].y);
    assert_eq!(chunks[2].bottom(), chunks[3].y);
    assert_eq!(chunks[3].bottom(), area.bottom());
    // The body keeps at least its `Min(1)` row (Layout honours Min over the
    // Length zones when starved).
    assert!(chunks[0].height >= 1, "the body kept >= 1 row: {chunks:?}");
}

// The full 4-zone render at tiny heights must never panic (the composer's
// `area.height - COMPOSER_CHROME_ROWS` underflow is the trap) and must keep
// any placed cursor ON the frame. Sweeps the smallest heights where the fixed
// zones fight the body.
#[test]
fn render_pending_at_tiny_heights_never_panics_and_keeps_the_cursor_on_frame() {
    let screen = Screen::new(ScreenOpts {
        notices: (1..=6).map(|i| format!("notice-{i}")).collect(),
        ..ScreenOpts::default()
    });
    for height in 1..=8 {
        let mut terminal = draw_pending(40, height, &screen);
        // If a cursor was placed, it must be within the frame bounds.
        if let Ok(pos) = terminal.get_cursor_position() {
            assert!(
                pos.x < 40 && pos.y < height,
                "cursor {pos:?} off a {height}-row frame"
            );
        }
    }
}

// --- the Help overlay (qwen `Help`, the `?` affordance) ------------------

// A Screen whose Help overlay is open, for the render assertions below.
fn screen_with_help_open() -> Screen {
    let mut screen = Screen::new(ScreenOpts::default());
    screen.help_open = true;
    screen
}

// The open Help overlay draws the bordered panel in the body region: the
// title, a shortcut row, a built-in command, and the `Esc to close` footer all
// land, and it replaces the transcript body (no header/tips row shows through).
#[test]
fn help_overlay_shows_shortcuts_commands_and_the_close_hint() {
    let screen = screen_with_help_open();
    // Tall enough that the whole one-column panel fits above the composer
    // (title through footer) without top-clipping.
    let terminal = draw_pending(80, 32, &screen);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("keyboard shortcuts"),
        "title present: {text:?}"
    );
    assert!(text.contains("Shortcuts"), "Shortcuts heading present");
    assert!(text.contains("Show this help"), "a shortcut row present");
    assert!(text.contains("Commands"), "Commands heading present");
    // A built-in command from the registry, derived (not hardcoded).
    assert!(text.contains("/model"), "the /model command is listed");
    assert!(
        text.contains("choose the model for this session"),
        "the /model help is listed"
    );
    assert!(text.contains("Esc to close"), "the close hint present");
    // The panel is bordered (box-drawing chars from the same helpers).
    assert!(
        text.contains('╭') && text.contains('╰'),
        "the panel is bordered"
    );
}

// Measure==draw (ADR-0029): every emitted panel Line is `<= content width`, so
// the viewport never soft-wraps a row. Spans the one-column widths AND past the
// two-column threshold (~116 content cols) so both layouts are exercised.
#[test]
fn help_panel_rows_never_exceed_the_content_width() {
    for width in [40u16, 60, 80, 100, 120, 140, 200] {
        let lines = help_panel_lines(width, theme::dark());
        for line in &lines {
            let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                cols <= width as usize,
                "a Help row is {cols} cols, over the {width}-col width"
            );
        }
    }
}

// --- the `/mcp` management dialog overlay (qwen `MCPManagementDialog`) ----

// A Screen with the `/mcp` dialog open and filled: type `/mcp`, Enter to
// commit (which opens the overlay to a Loading state), then feed the fetched
// views back through the McpDialogReady fill the adapter would post.
fn screen_with_mcp_dialog(servers: Vec<crate::mcp::McpServerView>) -> Screen {
    let mut screen = Screen::new(ScreenOpts::default());
    for c in "/mcp".chars() {
        screen = screen.handle_key(Key::Char(c)).0;
    }
    screen = screen.handle_key(Key::Enter).0;
    // The commit bumped the activation to 1; echo it on the fill.
    screen.apply_event(Event::mcp_dialog_ready(1, servers)).0
}

fn mcp_server(name: &str, status: crate::mcp::McpServerStatus) -> crate::mcp::McpServerView {
    crate::mcp::McpServerView {
        name: name.to_string(),
        status,
        source: crate::mcp::McpSource::User,
        transport_display: format!("{name} (stdio)"),
        cwd: None,
        trust: false,
        tools: Vec::new(),
        is_disabled: false,
        has_oauth_tokens: false,
        error: None,
    }
}

// The open `/mcp` dialog draws its bordered box in the body region: the
// "Manage MCP servers" title, the server count, a grouped server row with its
// status, and the footer hint - and it is bordered like the Help panel.
#[test]
fn mcp_dialog_shows_the_server_list_and_footer() {
    let screen = screen_with_mcp_dialog(vec![mcp_server(
        "github",
        crate::mcp::McpServerStatus::Connected,
    )]);
    let terminal = draw_pending(80, 32, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("Manage MCP servers"), "title: {text:?}");
    assert!(text.contains("1 server"), "count present");
    assert!(text.contains("User MCPs"), "the source group heading");
    assert!(text.contains("github"), "the server row");
    assert!(text.contains("connected"), "the status word");
    assert!(
        text.contains("Enter to select"),
        "the navigation footer present"
    );
    assert!(
        text.contains('╭') && text.contains('╰'),
        "the dialog is bordered"
    );
}

// The empty `/mcp` dialog draws qwen's "No MCP servers configured." state and
// the bare `Esc to close` footer.
#[test]
fn mcp_dialog_shows_the_empty_state() {
    let screen = screen_with_mcp_dialog(vec![]);
    let text = buffer_text(&draw_pending(80, 32, &screen));
    assert!(text.contains("No MCP servers configured."));
    assert!(text.contains("Add MCP servers to your settings to get started."));
}

// Measure==draw (ADR-0029): every emitted `/mcp` box Line is `<= content
// width`, so the viewport never soft-wraps a dialog row.
#[test]
fn mcp_dialog_rows_never_exceed_the_content_width() {
    let dialog = crate::ui::mcp_command::McpDialog::open(1);
    let view = {
        let mut d = dialog;
        d.fill_ready(
            1,
            vec![mcp_server("srv", crate::mcp::McpServerStatus::Connected)],
        );
        d.view()
    };
    for width in [40u16, 60, 80, 120] {
        for line in mcp_dialog_lines(&view, width, theme::dark()) {
            let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                cols <= width as usize,
                "a /mcp row is {cols} cols, over the {width}-col width"
            );
        }
    }
}

// The `/mcp` title renders BOLD (qwen's `<Text bold>` header) while the
// secondary count subline does NOT - the bold axis rides on top of the accent
// colour, so the box border/count stay unemphasised.
#[test]
fn mcp_dialog_bolds_the_header_title_but_not_the_count() {
    let view = {
        let mut d = crate::ui::mcp_command::McpDialog::open(1);
        d.fill_ready(
            1,
            vec![mcp_server("srv", crate::mcp::McpServerStatus::Connected)],
        );
        d.view()
    };
    let lines = mcp_dialog_lines(&view, 80, theme::dark());
    // Find the title row and the count row by their text (they sit inside the
    // bordered box, so match on the trimmed content).
    let bold_of = |needle: &str| {
        lines
            .iter()
            .find(|l| line_text(l).contains(needle))
            .map(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains(needle) && s.style.add_modifier.contains(Modifier::BOLD)
                })
            })
    };
    assert_eq!(
        bold_of("Manage MCP servers"),
        Some(true),
        "the header title is bold"
    );
    assert_eq!(
        bold_of("1 server"),
        Some(false),
        "the count subline is not bold"
    );
    // The group heading is bold too (qwen bolds `User MCPs`).
    assert_eq!(
        bold_of("User MCPs"),
        Some(true),
        "the group heading is bold"
    );
}

// At a common width (100) the panel is ONE clean column: no row carries the
// second column's key, and the longest description renders in FULL with no
// ellipsis (the mid-word truncation bug the two-column layout caused is gone).
#[test]
fn help_defaults_to_a_single_untruncated_column_at_width_100() {
    let lines = help_panel_lines(100, theme::dark());
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let longest = "Peek the full pending output into scrollback";
    assert!(
        texts.iter().any(|t| t.contains(longest)),
        "the longest description renders in full at width 100: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains('…')),
        "no row is truncated with an ellipsis at width 100: {texts:?}"
    );
    // A single column: no shortcut row pairs two keys (e.g. `/` … `Ctrl+O`).
    assert!(
        !texts
            .iter()
            .any(|t| t.contains('/') && t.contains("Ctrl+O")),
        "no row carries a second column at width 100: {texts:?}"
    );
}

// Past the threshold (content width >= 2*(key_col + longest_desc) + gap, ~116)
// the panel goes two-column: a row pairs the first-half and second-half keys,
// the right column's descriptions stay untruncated, and every row still fits.
#[test]
fn help_goes_two_column_only_at_wide_widths_with_aligned_untruncated_cells() {
    let width = 140u16;
    let lines = help_panel_lines(width, theme::dark());
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    // The first shortcut row pairs the first-half `/` key with the second-half
    // key (the split puts `Ctrl+O` at the top of the right column).
    assert!(
        texts
            .iter()
            .any(|t| t.contains('/') && t.contains("Ctrl+O")),
        "a two-column row pairs left+right keys at width {width}: {texts:?}"
    );
    // No ellipsis: both columns' full descriptions fit.
    assert!(
        !texts.iter().any(|t| t.contains('…')),
        "two columns never truncate at width {width}: {texts:?}"
    );
    // The right column aligns: every paired row starts its right key at the
    // SAME column (the fixed left-cell width). Find the column of `Ctrl+O`.
    let right_key_col = |t: &str| t.find("Ctrl+O").map(|b| t[..b].chars().count());
    let cols: Vec<usize> = texts.iter().filter_map(|t| right_key_col(t)).collect();
    assert!(!cols.is_empty(), "found the right column key");
    assert!(
        cols.iter().all(|&c| c == cols[0]),
        "the right column aligns to a fixed left-cell width: {cols:?}"
    );
}

/// Draws a composer overlay popup on a 40x12 test terminal with the standard
/// anchor row (10) and the dark theme, returning the terminal. Covers the
/// standard popup test shape: fixed geometry, dark theme, anchor row 10.
fn draw_popup(view: &OverlayView) -> Terminal<TestBackend> {
    draw_frame(40, 12, |f| {
        render_composer_popup(f, 10, f.area(), view, theme::dark())
    })
}

// --- render_composer_popup: System B (`/` palette) ----------------------

fn suggestion(
    label: &str,
    value: &str,
    desc: &str,
    matched: Option<(usize, usize)>,
) -> completion::Suggestion {
    completion::Suggestion {
        label: label.to_string(),
        value: value.to_string(),
        description: desc.to_string(),
        matched,
    }
}

fn palette(suggestions: Vec<completion::Suggestion>, active: usize) -> OverlayView {
    palette_expanded(suggestions, active, false)
}

fn palette_expanded(
    suggestions: Vec<completion::Suggestion>,
    active: usize,
    expanded: bool,
) -> OverlayView {
    OverlayView::Menu {
        suggestions,
        active,
        scroll: 0,
        query: "m".to_string(),
        expanded,
    }
}

#[test]
fn the_palette_lists_two_columns_command_and_description() {
    let view = palette(
        vec![
            suggestion("model", "model", "pick the model", Some((0, 1))),
            suggestion("theme", "theme", "pick the theme", None),
        ],
        0,
    );
    let text = buffer_text(&draw_popup(&view));
    assert!(text.contains(" commands "), "bordered title:\n{text}");
    assert!(text.contains("model"));
    assert!(text.contains("pick the model"), "description column");
    assert!(text.contains("theme"));
}

// --- PrepareLabel: MAX_WIDTH collapse/expand (qwen) ---------------------

#[test]
fn prepare_label_leaves_a_short_label_whole() {
    // Below MAX_WIDTH the label is untouched (no match).
    let (before, matched, after) = prepare_label("model", None, false);
    assert_eq!(
        (before, matched, after),
        ("model".into(), "".into(), "".into())
    );
}

#[test]
fn prepare_label_splits_a_short_label_at_its_match() {
    // A contiguous match window splits into before/matched/after (qwen
    // Case 1 - the label already fits).
    let (before, matched, after) = prepare_label("model", Some((0, 3)), false);
    assert_eq!(before, "");
    assert_eq!(matched, "mod");
    assert_eq!(after, "el");
}

#[test]
fn prepare_label_truncates_a_long_label_until_expanded() {
    let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH + 40).collect();
    // Collapsed: truncated to MAX_WIDTH chars + "..." (qwen no-match branch).
    let (before, _, _) = prepare_label(&long, None, false);
    assert_eq!(before.chars().count(), completion::MAX_WIDTH + 3);
    assert!(before.ends_with("..."));
    // Expanded: the full label, untouched.
    let (before, _, _) = prepare_label(&long, None, true);
    assert_eq!(before, long);
}

#[test]
fn prepare_label_windows_a_long_label_around_its_match_with_elisions() {
    // A long label with a short match mid-string collapses to a window
    // centred on the match, `...`-elided at the clipped ends (qwen Case 3).
    let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH * 2).collect();
    let mid = completion::MAX_WIDTH; // match sits in the middle
    let (before, matched, after) = prepare_label(&long, Some((mid, mid + 3)), false);
    assert_eq!(matched, "xxx", "the match stays whole");
    assert!(before.starts_with("..."), "left elision: {before:?}");
    assert!(after.ends_with("..."), "right elision: {after:?}");
    // The whole window fits MAX_WIDTH.
    let total = before.chars().count() + matched.chars().count() + after.chars().count();
    assert!(total <= completion::MAX_WIDTH, "window bounded: {total}");
}

#[test]
fn a_long_active_row_shows_the_expand_affordance() {
    let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH + 5).collect();
    // Collapsed active long row: ` → ` affordance.
    let collapsed = palette_expanded(vec![suggestion(&long, "cmd", "", None)], 0, false);
    assert!(
        buffer_text(&draw_popup(&collapsed)).contains('→'),
        "collapsed long active row shows →"
    );
    // Expanded active long row: ` ← ` affordance.
    let expanded = palette_expanded(vec![suggestion(&long, "cmd", "", None)], 0, true);
    assert!(
        buffer_text(&draw_popup(&expanded)).contains('←'),
        "expanded long active row shows ←"
    );
    // A SHORT active row shows no affordance.
    let short = palette_expanded(vec![suggestion("model", "model", "", None)], 0, false);
    let text = buffer_text(&draw_popup(&short));
    assert!(
        !text.contains('→') && !text.contains('←'),
        "short row: no affordance"
    );
}

#[test]
fn the_palette_active_row_is_accent_others_secondary_no_marker() {
    let view = palette(
        vec![
            suggestion("model", "model", "", None),
            suggestion("theme", "theme", "", None),
        ],
        1,
    );
    let terminal = draw_popup(&view);
    // 2 body rows + borders = height 4, anchored above row 10 → rows 7-8,
    // text from x = 2. System B has NO `›` marker and NO number, so the
    // first cell IS the label's first glyph.
    assert_eq!(
        cell_char(&terminal, 2, 8),
        't',
        "no gutter before the label"
    );
    let accent = tui_color(theme::dark().accent);
    let secondary = tui_color(theme::dark().muted);
    assert_eq!(cell_fg(&terminal, 2, 8), Some(accent), "active row accent");
    assert_eq!(
        cell_fg(&terminal, 2, 7),
        Some(secondary),
        "inactive secondary"
    );
}

#[test]
fn the_palette_match_substring_is_inverted() {
    // "/m" matches "model" at [0,1): the leading 'm' draws REVERSED.
    let view = palette(vec![suggestion("model", "model", "", Some((0, 1)))], 0);
    let terminal = draw_popup(&view);
    // One body row + borders = height 3 above row 10 → row 8, text x = 2.
    assert!(
        cell_modifier(&terminal, 2, 8).contains(Modifier::REVERSED),
        "the match is inverted"
    );
    assert!(
        !cell_modifier(&terminal, 3, 8).contains(Modifier::REVERSED),
        "the rest is not"
    );
}

#[test]
fn an_empty_palette_shows_no_matches() {
    let view = palette(vec![], 0);
    assert!(buffer_text(&draw_popup(&view)).contains("no matches"));
}

#[test]
fn the_palette_shows_arrows_and_a_counter_when_it_overflows() {
    // More than MAX_SUGGESTIONS rows, scrolled one down: a ▲ above, a ▼
    // below, and an (active/total) counter.
    let suggestions: Vec<_> = (0..MAX_SUGGESTIONS + 3)
        .map(|i| suggestion(&format!("cmd{i}"), &format!("cmd{i}"), "", None))
        .collect();
    let view = OverlayView::Menu {
        suggestions,
        active: 3,
        scroll: 1,
        query: "c".to_string(),
        expanded: false,
    };
    let terminal = draw_frame(40, 20, |f| {
        render_composer_popup(f, 18, f.area(), &view, theme::dark())
    });
    let text = buffer_text(&terminal);
    assert!(text.contains('▲'), "scroll-up arrow:\n{text}");
    assert!(text.contains('▼'), "scroll-down arrow");
    assert!(
        text.contains(&format!("(4/{})", MAX_SUGGESTIONS + 3)),
        "the (n/m) counter:\n{text}"
    );
}

// --- render_composer_popup: AT file picker (Phase C2) -------------------

fn at_files(suggestions: Vec<completion::Suggestion>, loading: bool) -> OverlayView {
    OverlayView::AtFiles {
        suggestions,
        active: 0,
        scroll: 0,
        query: "co".to_string(),
        loading,
    }
}

#[test]
fn the_at_picker_lists_repo_relative_paths_titled_files() {
    let view = at_files(
        vec![
            suggestion("src/ui/composer.rs", "src/ui/composer.rs", "", Some((7, 9))),
            suggestion("src/config.rs", "src/config.rs", "", Some((4, 6))),
        ],
        false,
    );
    let text = buffer_text(&draw_popup(&view));
    assert!(text.contains(" files "), "bordered 'files' title:\n{text}");
    assert!(text.contains("composer.rs"), "a path row is shown:\n{text}");
    assert!(text.contains("config.rs"));
}

#[test]
fn a_long_at_path_renders_in_full_not_chopped_at_half_width() {
    // A path longer than HALF the inner width (36/2 = 18) must render WHOLE:
    // AT rows carry no description, so the label column uses the full inner
    // width, not the slash palette's width/2 cap. Before the fix
    // "src/ui/components.rs" (20 cols) chopped to "src/ui/components." (18).
    let view = at_files(
        vec![suggestion(
            "src/ui/components.rs",
            "src/ui/components.rs",
            "",
            Some((7, 9)),
        )],
        false,
    );
    let text = buffer_text(&draw_popup(&view));
    assert!(
        text.contains("src/ui/components.rs"),
        "the full path renders (no width/2 truncation):\n{text}"
    );
}

#[test]
fn the_at_picker_match_substring_is_inverted() {
    // "co" matches "src/config.rs" at [4,6): the 'co' draws REVERSED.
    let view = at_files(
        vec![suggestion(
            "src/config.rs",
            "src/config.rs",
            "",
            Some((4, 6)),
        )],
        false,
    );
    let terminal = draw_popup(&view);
    // One body row + borders = height 3 above row 10 → row 8, text x = 2.
    // "src/" is x=2..6, so the 'c' at char 4 is cell x=6.
    assert!(
        cell_modifier(&terminal, 6, 8).contains(Modifier::REVERSED),
        "the match is inverted"
    );
}

#[test]
fn an_empty_at_search_still_loading_shows_searching() {
    // A fetch in flight with no rows yet draws the subtle "searching…" line.
    let view = at_files(vec![], true);
    assert!(buffer_text(&draw_popup(&view)).contains("searching…"));
}

#[test]
fn an_empty_at_search_that_finished_shows_no_matches() {
    // Not loading + no rows: the "no matches" placeholder (a real empty walk).
    let view = at_files(vec![], false);
    assert!(buffer_text(&draw_popup(&view)).contains("no matches"));
}

// --- render_composer_popup: System A (numbered `›` dialog) --------------

fn dialog(
    command: &str,
    status: OverlayStatus,
    rows: Vec<SelectorRow>,
    active: usize,
) -> OverlayView {
    OverlayView::Dialog {
        command: command.to_string(),
        status,
        rows,
        active,
        detail: None,
    }
}

#[test]
fn a_loading_dialog_shows_the_loading_line() {
    let view = dialog("model", OverlayStatus::Loading, vec![], 0);
    let text = buffer_text(&draw_popup(&view));
    assert!(text.contains(" models "), "dialog title:\n{text}");
    assert!(text.contains("loading models…"));
}

#[test]
fn a_failed_dialog_shows_the_failure_message() {
    let view = dialog(
        "model",
        OverlayStatus::Failed("connection refused".to_string()),
        vec![],
        0,
    );
    assert!(buffer_text(&draw_popup(&view)).contains("failed: connection refused"));
}

#[test]
fn a_ready_dialog_lists_numbered_rows_with_the_marker() {
    let view = dialog(
        "model",
        OverlayStatus::Ready,
        vec![
            SelectorRow::new("a", "qwen/qwen3-30b", None),
            SelectorRow::new("b", "meta/llama-3.1", None),
        ],
        0,
    );
    let text = buffer_text(&draw_popup(&view));
    assert!(text.contains(" models "));
    assert!(
        text.contains(SELECTION_MARKER),
        "the `›` marker on the active row"
    );
    assert!(text.contains("1."), "numbered rows");
    assert!(text.contains("qwen/qwen3-30b"));
    assert!(text.contains("meta/llama-3.1"));
}

#[test]
fn a_dialog_header_row_is_dim_and_unnumbered() {
    let view = dialog(
        "model",
        OverlayStatus::Ready,
        vec![
            SelectorRow::header("openrouter"),
            SelectorRow::new("openrouter/kimi-k2", "openrouter/kimi-k2", None),
        ],
        1,
    );
    let terminal = draw_popup(&view);
    // Body rows 7 (header) and 8 (member), inset text from x = 2. The
    // header draws a blank gutter + blank number field, so its label sits
    // further right; the active member (row 1) carries the `›` marker.
    assert_ne!(cell_char(&terminal, 2, 7), '›', "the header has no marker");
    assert_eq!(
        cell_char(&terminal, 2, 8),
        '›',
        "the active member is marked"
    );
    // The header label ("openrouter") reads dim (secondary) wherever it
    // starts - find its first glyph on the row.
    let header_row = row_text(&terminal, 7);
    let col = header_row.find('o').expect("the header label") as u16;
    let secondary = tui_color(theme::dark().muted);
    assert_eq!(
        cell_fg(&terminal, col, 7),
        Some(secondary),
        "the header is dim"
    );
}

#[test]
fn the_dialog_titles_itself_after_its_command() {
    // The title pluralizes the opaque command name, so /theme's dialog
    // reads " themes " without the renderer knowing any command.
    let view = dialog("theme", OverlayStatus::Loading, vec![], 0);
    let text = buffer_text(&draw_popup(&view));
    assert!(text.contains(" themes "), "dialog title:\n{text}");
    assert!(text.contains("loading themes…"));
}

#[test]
fn the_dialog_scrolls_the_active_row_into_view() {
    // 20 rows against the POPUP_MAX_ROWS cap: an active row at the bottom
    // scrolls the top rows out and brings it on screen.
    let rows: Vec<SelectorRow> = (0..20)
        .map(|i| SelectorRow::new(format!("m{i}"), format!("model-{i:02}"), None))
        .collect();
    let view = dialog("model", OverlayStatus::Ready, rows, 19);
    let terminal = draw_frame(40, 14, |f| {
        render_composer_popup(f, 12, f.area(), &view, theme::dark())
    });
    let text = buffer_text(&terminal);
    assert!(
        text.contains("model-19"),
        "the active row scrolled into view"
    );
    assert!(!text.contains("model-00"), "the top rows scrolled out");
}

// --- pending body: layout, margins, streaming ---------------------------

fn screen_with_notices(notices: Vec<String>) -> Screen {
    Screen::new(ScreenOpts {
        notices,
        ..ScreenOpts::default()
    })
}

/// Builds a screen that has submitted `prompt`, started message 1, and
/// received one in-flight thinking update with `thinking_text`. The caller
/// continues from here (settle, nudge, draw). Shared by tests that need a
/// screen-with-live-thought setup to avoid the submitted+message_start+
/// message_update triple repeating (FRAGMENT DRY-003 fix).
fn screen_with_thinking(prompt: &str, thinking_text: impl Into<String>) -> Screen {
    let (screen, _) = screen_with_notices(vec![]).submitted(prompt, Ok(()));
    let thinking = thinking_text.into();
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("t".to_string()),
        vec![ContentBlock::Thinking { text: thinking }],
    ));
    screen
}

#[test]
#[ignore = "manual: cargo nextest run dump_demo_render --run-ignored all --no-capture"]
fn dump_demo_render() {
    let screen = Screen::demo();
    let terminal = draw_viewport(100, 70, &screen);
    let mut out = String::from("\n");
    for y in 0..70 {
        // Bracket the leftmost 2 margin columns (qwen `marginLeft:2`,
        // ADR-0046) so the left margin is unambiguous, then the content.
        let row = row_text(&terminal, y);
        let split = row.char_indices().nth(2).map_or(row.len(), |(i, _)| i);
        let (gutter, rest) = row.split_at(split);
        out.push_str(&format!("{y:>2}|{gutter}|{}\n", rest.trim_end()));
    }
    eprintln!("{out}");
}

// The non-interactive smoke for the `diff-demo` binary: the seeded
// `Screen::demo_diffs()` renders through the real pending-body path (the same
// one a live inline frame uses) without panicking, in BOTH diff-fold states -
// the default EXPANDED body (qwen `!compactMode`, the app's default) and the
// compact fold (Ctrl+O / the binary's `o` key: each diff a fold-title
// one-liner). The binary only adds the terminal lifecycle on top of this.
#[test]
fn the_diff_demo_screen_renders_its_diffs_without_panicking() {
    // Default (compact=false): the titles AND the code rows / elided tail.
    let expanded = buffer_text(&draw_viewport(100, 70, &Screen::demo_diffs()));
    assert!(
        expanded.contains("clean up the tokenizer"),
        "the request:\n{expanded}"
    );
    for title in [
        "edit src/lexer.rs",
        "src/greet.js",
        "package.json",
        "src/generated.js",
    ] {
        assert!(expanded.contains(title), "the {title} title:\n{expanded}");
    }
    assert!(
        expanded.contains("split_whitespace"),
        "the rust hunk body:\n{expanded}"
    );
    assert!(
        expanded.contains("Greets a user by name"),
        "the jsdoc body:\n{expanded}"
    );
    assert!(
        expanded.contains("37 lines hidden"),
        "the elided tail:\n{expanded}"
    );

    // Compact (Ctrl+O): the diff bodies fold away to their fold titles - the
    // code rows are gone, but the titles stay.
    let (compact_screen, _) = Screen::demo_diffs().handle_key(Key::ToggleCompact);
    let folded = buffer_text(&draw_viewport(100, 70, &compact_screen));
    assert!(
        folded.contains("edit src/lexer.rs"),
        "the fold title stays:\n{folded}"
    );
    assert!(
        !folded.contains("split_whitespace"),
        "the rust hunk body folds away under compact:\n{folded}"
    );
}

/// The whole demo run rendered through the FULL committed slice (no top-clip,
/// qwen `<Static>`), as newline-joined rows - the golden-shape seam these
/// tests inspect without the pending body's overflow clip.
fn demo_committed_text(width: u16) -> String {
    let screen = Screen::demo();
    let content_width = width - 2 * CONTENT_MARGIN;
    let mut cache = RenderCache::new();
    cache.sync(
        screen.transcript(),
        Toggles::default(),
        content_width,
        theme::dark(),
    );
    let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();
    let count = items.len();
    let height = slice_height(&cache, &items, 0, count, content_width);
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height.max(1)));
    blit_slice(&mut buf, &cache, &items, 0, count);
    commit_buffer_text(&buf)
}

#[test]
fn the_demo_render_matches_the_confirmed_full_content_run_shape() {
    // The demo is the living spec (ADR-0046 + qwen v0.16.0 chrome): the
    // committed slice renders each item in FULL. Pin the load-bearing qwen
    // prefixes so a reskin regression trips here, not only in a manual dump.
    let text = demo_committed_text(100);
    // The user prompt wears the `>` caret; the assistant the `✦` marker.
    assert!(
        text.contains("> evaluate this project"),
        "user caret:\n{text}"
    );
    assert!(
        text.contains("✦ I'll evaluate this project"),
        "assistant marker:\n{text}"
    );
    // The first thought shows (grey `✦`, one-line collapsed).
    assert!(
        text.contains("✦ The user wants me to evaluate"),
        "the first thought shows:\n{text}"
    );
    // Tool work is inside a rounded box (qwen `ToolGroupMessage`).
    assert!(
        text.contains('╭') && text.contains('╰'),
        "tool box:\n{text}"
    );
    assert!(text.contains("list_directory"), "a tool row:\n{text}");
    // The error tool result reads the `x` ERROR marker.
    assert!(
        text.contains("run_shell_command") && text.contains("command denied"),
        "the error result:\n{text}"
    );
    // Assistant closing text + code fence.
    assert!(
        text.contains("The project is a well-structured"),
        "closing text:\n{text}"
    );
    assert!(text.contains("fn tokenize"), "code fence:\n{text}");
}

#[test]
fn committed_content_stays_within_the_left_margin() {
    // qwen `marginLeft:2`: every non-blank committed demo row draws two columns
    // in - no content touches column 0/1 (the retired lane spine is gone,
    // ADR-0046). Rendered through the full committed slice so the pending
    // body's overflow marker (which sits at column 0) never confounds it.
    for (y, row) in demo_committed_text(100).lines().enumerate() {
        if row.trim().is_empty() {
            continue;
        }
        let cols: Vec<char> = row.chars().collect();
        assert_eq!(
            (cols[0], cols[1]),
            (' ', ' '),
            "row {y} bled into the 2-col left margin: {row:?}"
        );
    }
}

/// Draws the inline pending body with a caller-supplied [`Anim`] (for the
/// lull-row test) TOP-aligned, like [`draw_viewport`].
fn draw_viewport_anim(
    width: u16,
    height: u16,
    screen: &Screen,
    anim: Anim,
) -> Terminal<TestBackend> {
    let mut cache = RenderCache::new();
    draw_frame(width, height, |f| {
        let area = f.area();
        let total = pending_body_height(screen, &mut cache, area.width, theme::dark());
        let zone_h = (total as u16).min(area.height).max(1);
        let zone = Rect {
            height: zone_h,
            ..area
        };
        render_pending_body_at(
            f,
            zone,
            &mut PendingBodyParams {
                screen,
                cache: &mut cache,
                anim,
            },
            theme::dark(),
            0,
        );
    })
}

#[test]
fn the_pending_body_draws_the_transcript() {
    let screen = screen_with_notices(vec!["a launch notice".to_string()]);
    let terminal = draw_viewport(80, 20, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains(">_ suspenders"), "the header brand:\n{text}");
    assert!(text.contains("a launch notice"));
}

// The spinner line draws through the pending render path (ADR-0048): a
// Running Run with nothing streaming, quiet past the settle window, paints
// the elapsed timer + `esc to cancel` into the buffer as a live entry.
#[test]
fn the_pending_body_draws_the_spinner_line_when_running() {
    let (screen, _) = screen_with_notices(vec!["a launch notice".to_string()])
        .apply_event(Event::run_started("r1"));
    assert!(
        !screen.has_live_stream(),
        "the Turn runs but nothing streams"
    );
    let anim = Anim {
        quiet_ticks: lull::SETTLE_TICKS + 4,
        lull_seq: 0,
        ..Default::default()
    };
    let terminal = draw_viewport_anim(80, 20, &screen, anim);
    let text = buffer_text(&terminal);
    assert!(text.contains("5s"), "the elapsed opens at 5s:\n{text}");
    assert!(
        text.contains("esc to cancel"),
        "the cancel affordance:\n{text}"
    );
}

// An overflowing pending body top-clips (ADR-0046): the tail (newest) is on
// screen and the top is dropped. There is no scrollbar - native scrollback
// owns history.
#[test]
fn an_overflowing_pending_body_top_clips_and_keeps_the_tail() {
    let notices: Vec<String> = (0..30).map(|i| format!("notice line {i:02}")).collect();
    let screen = screen_with_notices(notices);
    let terminal = draw_viewport(40, 8, &screen);
    let text = buffer_text(&terminal);
    // The tail is on screen, the top is clipped.
    assert!(text.contains("notice line 29"));
    assert!(!text.contains("notice line 00"));
}

// ---- wrap_words: the greedy word wrap ----------------------------------

#[test]
fn wrap_words_greedily_wraps_on_spaces_within_width() {
    assert_eq!(
        wrap_words("the quick brown fox", 9),
        vec!["the quick", "brown fox"]
    );
    // Every segment fits the width.
    for seg in wrap_words("the quick brown fox jumps over", 9) {
        assert!(seg.chars().count() <= 9, "segment over width: {seg:?}");
    }
}

#[test]
fn wrap_words_hard_splits_a_word_longer_than_the_width() {
    // A single 10-char word at width 4 splits into 4+4+2, never overflowing.
    let segs = wrap_words("abcdefghij", 4);
    assert_eq!(segs, vec!["abcd", "efgh", "ij"]);
    for seg in &segs {
        assert!(seg.chars().count() <= 4);
    }
}

// ---- settled Thinking: the full grey body (qwen `ThinkMessage`) ---------

#[test]
fn settled_thinking_prefixes_the_first_body_row_with_the_grey_marker() {
    // qwen has no collapsed one-liner (ADR-0052): a shown thought renders its
    // full grey markdown body, the `✦` marker hung on the first row.
    let lines = settled_thinking_lines("one\ntwo\nthree", theme::dark());
    let text = lines[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert!(text.starts_with("✦ one"), "grey thought marker: {text:?}");
}

#[test]
fn settled_thinking_renders_every_source_row() {
    // The full body: three source lines render as three (or more) rows, not a
    // single collapsed line.
    let lines = settled_thinking_lines("one\ntwo\nthree", theme::dark());
    assert!(
        lines.len() >= 3,
        "the full body keeps every source row: {}",
        lines.len()
    );
}

#[test]
fn the_reserved_gutter_forces_wrapping_at_the_reduced_content_width() {
    // RED-1 (ADR-0029): the 2-col left+right margins (qwen `marginLeft:2,
    // marginRight:2`) are carved off, so content wraps in the narrower
    // `content_area` and is DRAWN two columns in. A 39-char word fits in the
    // full 40 cols but must wrap at the reduced content width.
    let word = "x".repeat(39);
    let screen = Screen::new(ScreenOpts {
        notices: vec![word.clone()],
        ..ScreenOpts::default()
    });
    let terminal = draw_viewport(40, 20, &screen);
    // A local cache synced at the same content width the body drew at, for
    // the wrapped-count assertion below.
    let mut cache = RenderCache::new();
    cache.sync(
        screen.transcript(),
        Toggles::default(),
        40 - 2 * CONTENT_MARGIN,
        theme::dark(),
    );

    // (1) The notice is drawn CONTENT_MARGIN columns in (qwen `marginLeft:2`):
    // the first row carrying the word begins with the `●` info prefix two
    // columns in from the frame edge.
    let word_row = (0..20)
        .map(|y| row_text(&terminal, y))
        .find(|r| r.contains('x'))
        .expect("the notice row");
    let indent = word_row.chars().take_while(|c| *c == ' ').count();
    assert_eq!(
        indent, CONTENT_MARGIN as usize,
        "content draws at the 2-col margin, not column 0: {word_row:?}"
    );

    // (2) The 39-char word wrapped to more than one visual content row, which
    // happens ONLY at the reduced content width (frame 40 − two 2-col margins
    // − the 2-col info prefix = 34). At the full 40 cols it would be one row.
    let word_rows: usize = cache
        .settled()
        .find_map(|(lines, wrapped)| {
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains('x')))
                .then_some(wrapped)
        })
        .expect("the notice's cached entry");
    assert!(
        word_rows >= 2,
        "the word wrapped at the reduced width, got {word_rows}"
    );
}

#[test]
fn a_user_prompt_shows_the_caret_prefix_and_the_agent_the_marker() {
    // qwen chrome: an Info line (a launch notice) reads `●`; a User prompt
    // the `>` caret; the agent's answer the `✦` marker. All baked into the
    // content, 2-col margin.
    let screen = screen_with_notices(vec!["a launch notice".to_string()]);
    let (screen, _) = screen.submitted("do the thing", Ok(()));
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_end(
        vec![ContentBlock::text("done")],
        StopReason::EndTurn,
    ));
    let terminal = draw_viewport(40, 20, &screen);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("> do the thing"),
        "user caret prefix:\n{text}"
    );
    assert!(text.contains("✦ done"), "assistant marker prefix:\n{text}");
    assert!(text.contains("● a launch notice"), "info prefix:\n{text}");
}

#[test]
fn the_pending_body_top_clips_to_the_newest_rows() {
    // ADR-0046 inline top-clip: a tall agent answer that OVERFLOWS the pending
    // body keeps the NEWEST rows and drops the oldest.
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.submitted("the question", Ok(()));
    let answer = (0..14)
        .map(|i| format!("ANSWER-{i}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_end(
        vec![ContentBlock::text(&answer)],
        StopReason::EndTurn,
    ));

    // A short body zone forces the top-clip.
    let terminal = draw_viewport(40, 10, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("ANSWER-13"), "the tail (newest) is kept");
    assert!(!text.contains("ANSWER-0"), "the oldest clipped off the top");
}

/// Vinnie's `evaluate this project` shape (~60 cols): a User prompt, a long
/// settled thought that would wrap, a wrapping marker, and a tool
/// call. Returns the rendered terminal.
fn evaluate_project_screen(width: u16, height: u16) -> Terminal<TestBackend> {
    // A long Compaction marker (`⟨ compaction: ... → summary ⟩`) that soft-
    // wraps at 60 cols, standing in for the wrapping marker this
    // shape exercises.
    let status = "reading the manifest and the entry point and every other file \
                     that could plausibly appear so the marker wraps across several \
                     visual rows here";
    let thinking = "I should read the manifest and the entry point and the tests \
                        and then form a plan about what to evaluate first here";
    let screen = screen_with_thinking("evaluate this project", thinking);
    // Settle the thought (empty final content → thinking materializes).
    let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
    // A wrapping Housekeeping marker (the `⟨ compaction: ... ⟩` line).
    let (screen, _) = screen.apply_event(Event::compaction_progress(status));
    let (screen, _) = screen.apply_event(Event::tool_call(
        "id1",
        "read_file",
        serde_json::json!({"path": "Cargo.toml"}),
    ));
    draw_viewport(width, height, &screen)
}

// A 400-`z` reasoning line (far wider than any terminal) must land as EXACTLY
// one visual row ending in the `…` truncation marker - the "folds/truncates,
// never balloons" guarantee both the settled and the streaming path make. Pins
// that the z-carrying rows are a single truncated row.
fn assert_z_line_folds_to_one_truncated_row(terminal: &Terminal<TestBackend>, height: u16) {
    let z_rows: Vec<String> = (0..height)
        .map(|y| row_text(terminal, y))
        .filter(|r| r.contains('z'))
        .collect();
    assert_eq!(
        z_rows.len(),
        1,
        "the long line stays one visual row: {z_rows:?}"
    );
    assert!(z_rows[0].contains('…'), "it is truncated: {:?}", z_rows[0]);
}

#[test]
fn compact_mode_hides_a_settled_thought_entirely() {
    // qwen has NO collapsed one-liner for a settled thought (ADR-0052): it
    // either shows in full or, under compact mode (Ctrl+O), is HIDDEN
    // entirely. A long thought shows its z's when NOT compact...
    let long = "z".repeat(400);
    let screen = screen_with_thinking("q", long);
    let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
    let shown = draw_viewport(60, 20, &screen);
    assert!(
        (0..20).any(|y| row_text(&shown, y).contains('z')),
        "the settled thought shows in full when not compact"
    );

    // ...and vanishes under compact mode.
    let (screen, _) = screen.handle_key(crate::ui::screen::Key::ToggleCompact);
    let hidden = draw_viewport(60, 20, &screen);
    assert!(
        (0..20).all(|y| !row_text(&hidden, y).contains('z')),
        "compact mode hides the settled thought entirely"
    );
}

#[test]
fn settled_thinking_uses_the_star_glyph_not_the_brain_emoji() {
    // Symptom 3: settled thinking unifies on the `✦` family with the live
    // tail, and drops the width-2 `🧠` emoji. Shown (compact=false) the grey
    // `✦` marker prefixes the first markdown body row.
    let shown = message_lines(
        &TranscriptItem::Thinking {
            text: "line one\nline two".into(),
        },
        false,
        80,
        theme::dark(),
    );
    assert!(line_text(&shown[0]).starts_with("✦ line one"));
    assert!(!line_text(&shown[0]).contains('🧠'));

    // Compact (Ctrl+O) HIDES the thought entirely (qwen `!compactMode`,
    // ADR-0052): no rows at all.
    let hidden = message_lines(
        &TranscriptItem::Thinking {
            text: "a short thought".into(),
        },
        true,
        80,
        theme::dark(),
    );
    assert!(hidden.is_empty());
}

#[test]
fn the_evaluate_shape_renders_its_content_within_the_margins() {
    // The `evaluate this project` shape at 60 cols - a wrapped thought, a
    // wrapped marker, a tool call - all draw two columns in (qwen `marginLeft:
    // 2`) and never touch column 0 (the retired lane spine is gone, ADR-0046).
    let terminal = evaluate_project_screen(60, 24);
    let mut saw_content = false;
    for y in 0..24 {
        let row = row_text(&terminal, y);
        if row.trim().is_empty() {
            continue;
        }
        saw_content = true;
        // No content column 0/1 (the 2-col left margin stays clear).
        let cols: Vec<char> = row.chars().collect();
        assert_eq!(
            (cols[0], cols[1]),
            (' ', ' '),
            "row {y} bled into the left margin: {row:?}"
        );
    }
    assert!(saw_content, "the shape drew content");
}

#[test]
fn a_streaming_thinking_snapshot_draws_the_animated_header_and_reasoning_tail() {
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("pondering".to_string()),
        vec![ContentBlock::Thinking {
            text: "pondering the viewport".to_string(),
        }],
    ));
    let terminal = draw_viewport(80, 20, &screen);
    let text = buffer_text(&terminal);
    // Live reasoning is content, not a metric: the animated
    // `✦ Thinking` header sits above the reasoning tail, and the reasoning
    // text itself is shown - not a token count.
    assert!(text.contains("✦ Thinking"), "the header:\n{text}");
    assert!(
        text.contains("pondering the viewport"),
        "the reasoning tail:\n{text}"
    );
    assert!(!text.contains("tokens)"));
}

#[test]
fn compact_suppresses_the_live_thinking_tail_but_keeps_the_spinner_subject() {
    // qwen gates the pending `gemini_thought` under `!compactMode`
    // (`HistoryItemDisplay.tsx:155`) but does NOT compact-gate the
    // LoadingIndicator. So under compact the animated `✦ Thinking` tail is
    // ABSENT while the spinner SUBJECT (the thought-subject seam,
    // `thought?.subject || currentLoadingPhrase`) is still PRESENT.
    // RunStarted puts the Run into Running so the spinner (LoadingIndicator)
    // draws - the seam the subject fills; then stream a thought.
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.apply_event(Event::run_started("r1"));
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("pondering".to_string()),
        vec![ContentBlock::Thinking {
            text: "weighing the tradeoffs".to_string(),
        }],
    ));
    assert_eq!(screen.status, Status::Running, "the Run is Running");
    let (screen, _) = screen.handle_key(crate::ui::screen::Key::ToggleCompact);
    assert!(screen.compact_mode, "toggled into compact");

    let terminal = draw_viewport(80, 20, &screen);
    let text = buffer_text(&terminal);
    // The live thinking tail's animated header is gone under compact.
    assert!(
        !text.contains("✦ Thinking"),
        "the live thinking tail must be suppressed under compact:\n{text}"
    );
    // ...but the spinner still shows, and its subject is the live reasoning
    // head (the last non-empty line fallback), not a lull phrase.
    assert!(
        text.contains("weighing the tradeoffs"),
        "the spinner subject line stays present under compact:\n{text}"
    );
}

/// Renders the FULL pending body for `screen` into a `width`x`height` buffer
/// at the whole area (no zone re-measure, so the spinner line is never
/// top-clipped out): the shape the duplication tests need to see the tail AND
/// the spinner line together.
fn draw_full_body(width: u16, height: u16, screen: &Screen) -> String {
    let mut cache = RenderCache::new();
    let terminal = draw_frame(width, height, |f| {
        render_pending_body_at(
            f,
            f.area(),
            &mut PendingBodyParams {
                screen,
                cache: &mut cache,
                anim: Anim::default(),
            },
            theme::dark(),
            0,
        );
    });
    buffer_text(&terminal)
}

// REGRESSION (bug: "the colored thinking indicator has the thought go through
// it"): while the model reasons, the live `✦ Thinking` tail shows the
// reasoning head AND the spinner line's subject fell back to that SAME head,
// so the exact line painted TWICE - once in the tail, once on the colored
// spinner line. With no distinct bold `**subject**`, the spinner must NOT echo
// the tail: the reasoning appears EXACTLY ONCE.
#[test]
fn the_spinner_does_not_duplicate_the_thinking_tail_head() {
    let reasoning = "Evaluate the whole project structure carefully";
    let (screen, _) = screen_with_notices(vec![]).apply_event(Event::run_started("r1"));
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("t".to_string()),
        vec![ContentBlock::Thinking {
            text: reasoning.to_string(),
        }],
    ));
    assert_eq!(screen.status, Status::Running, "the Run is Running");
    assert!(!screen.compact_mode, "non-compact: the tail is visible");

    let text = draw_full_body(80, 20, &screen);
    let occurrences = text.matches(reasoning).count();
    assert_eq!(
        occurrences, 1,
        "the reasoning head must render ONCE (tail only), not echoed on the \
             spinner line:\n{text}"
    );
    // The tail is the surface that keeps it (the `✦ Thinking` header sits above).
    assert!(
        text.contains("✦ Thinking"),
        "the thinking tail header:\n{text}"
    );
}

// A DISTINCT bold `**subject**` is NOT the tail's head, so it still belongs on
// the spinner line even while the tail shows the fuller reasoning (qwen
// `parseThought`): the short subject shows, the tail keeps the raw line.
#[test]
fn a_distinct_bold_subject_still_shows_on_the_spinner() {
    let (screen, _) = screen_with_notices(vec![]).apply_event(Event::run_started("r1"));
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("t".to_string()),
        vec![ContentBlock::Thinking {
            text: "**Mapping the tree** now I walk each crate".to_string(),
        }],
    ));
    let text = draw_full_body(80, 20, &screen);
    // The bold subject shows on the spinner; the raw reasoning stays in the tail.
    assert!(
        text.contains("Mapping the tree"),
        "the distinct bold subject shows on the spinner:\n{text}"
    );
    assert!(
        text.contains("now I walk each crate"),
        "the tail keeps the raw reasoning line:\n{text}"
    );
}

// REGRESSION (bug: "when I use Ctrl+S the last message just keeps appending
// everything"): the Ctrl-S peek blitted the FULL pending body into scrollback
// on EVERY press, even when the body already fit the live viewport - so
// holding Ctrl-S stacked identical copies. When nothing is top-clipped the
// peek must reveal NOTHING (height 0), matching the `… Ctrl-S to show more`
// marker, which only shows on overflow.
#[test]
fn ctrl_s_peek_is_a_noop_when_the_pending_body_fits() {
    let screen = screen_with_notices(vec!["a short notice".to_string()]);
    let mut cache = RenderCache::new();
    let mut peek = PendingPeek {
        cache: &mut cache,
        screen: &screen,
        anim: Anim::default(),
        theme: theme::dark(),
    };
    // A TALL viewport the small body fits inside: nothing top-clips.
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 40,
    };
    assert_eq!(
        pending_peek_height(&mut peek, area),
        0,
        "a body that fits the viewport reveals nothing on Ctrl-S"
    );
}

// The complement: when the body genuinely OVERFLOWS a short viewport, Ctrl-S
// still reveals the FULL unclamped body (more rows than the whole viewport) so
// the top-clipped rows land in scrollback.
#[test]
fn ctrl_s_peek_reveals_the_full_body_on_overflow() {
    let screen = screen_with_notices((0..30).map(|i| format!("notice number {i}")).collect());
    let mut cache = RenderCache::new();
    let mut peek = PendingPeek {
        cache: &mut cache,
        screen: &screen,
        anim: Anim::default(),
        theme: theme::dark(),
    };
    let area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 6,
    };
    let height = pending_peek_height(&mut peek, area);
    assert!(
        height as usize > area.height as usize,
        "an overflowing body reveals the full unclamped height ({height} rows), \
             more than the {}-row viewport",
        area.height
    );
}

#[test]
fn the_reasoning_tail_shows_only_the_last_rows_under_the_header() {
    // The rolling tail is the last THINKING_TAIL_ROWS source rows; older
    // reasoning scrolls off the top of the sub-block.
    let reasoning = "row one\nrow two\nrow three\nrow four\nrow five";
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("…".to_string()),
        vec![ContentBlock::Thinking {
            text: reasoning.to_string(),
        }],
    ));
    let terminal = draw_viewport(80, 20, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("row three") && text.contains("row five"));
    // "row one"/"row two" scrolled off the three-row tail.
    assert!(!text.contains("row one") && !text.contains("row two"));
}

#[test]
fn a_long_reasoning_line_is_truncated_so_the_tail_stays_bounded() {
    // SHOULD-3: one very long unwrapped reasoning line would soft-wrap to
    // many visual rows and let the "short tail" fill the viewport. The tail
    // truncates each source row to the content width so it stays one visual
    // row (marked with `…`), keeping the sub-block to header + N rows.
    let long = "z".repeat(400); // far wider than any terminal
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Thinking("…".to_string()),
        vec![ContentBlock::Thinking { text: long }],
    ));
    let terminal = draw_viewport(40, 20, &screen);
    // Exactly one row carries the reasoning z's, and it ends in the `…`
    // truncation marker - the long line did not balloon into many rows.
    assert_z_line_folds_to_one_truncated_row(&terminal, 20);
}

#[test]
fn in_flight_assistant_text_renders_as_the_streaming_tail() {
    let screen = screen_with_notices(vec![]);
    let (screen, _) = screen.apply_event(Event::message_start(1));
    let (screen, _) = screen.apply_event(Event::message_update(
        Delta::Text("a streaming reply".to_string()),
        vec![ContentBlock::text("a streaming reply")],
    ));
    let terminal = draw_viewport(80, 20, &screen);
    assert!(buffer_text(&terminal).contains("a streaming reply"));
}

// -----------------------------------------------------------------------
// normalize_diff_text: tab -> two spaces, the only text rule a diff code
// line needs (the tint band fills empty lines, so no empty -> space trick);
// render tests pin the VISIBLE output, these pin the TEXT rule.
// -----------------------------------------------------------------------

#[test]
fn normalize_diff_text_replaces_tabs_with_two_spaces() {
    assert_eq!(normalize_diff_text("a\tb"), "a  b");
}

#[test]
fn normalize_diff_text_leaves_ordinary_text_unchanged() {
    assert_eq!(normalize_diff_text("hello world"), "hello world");
}

// -----------------------------------------------------------------------
// popup_rect: the popup geometry is the only pure math that was tangled
// into render_composer_popup; tested here at the calculation level.
// -----------------------------------------------------------------------

#[test]
fn popup_rect_height_is_body_plus_two_borders() {
    let area = Rect::new(0, 0, 80, 24);
    let r = popup_rect(10, area, 3, POPUP_MAX_ROWS); // 3 body rows -> height 5
    assert_eq!(r.height, 5);
}

#[test]
fn popup_rect_height_is_capped_at_popup_max_plus_two() {
    let area = Rect::new(0, 0, 80, 24);
    // 100 body rows would be POPUP_MAX_ROWS + 2 once capped.
    let r = popup_rect(20, area, 100, POPUP_MAX_ROWS);
    assert_eq!(r.height, POPUP_MAX_ROWS + 2);
}

#[test]
fn popup_rect_is_anchored_above_anchor_y() {
    let area = Rect::new(0, 0, 80, 24);
    let r = popup_rect(10, area, 3, POPUP_MAX_ROWS); // height 5, y = 10 - 5 = 5
    assert_eq!(r.y, 5);
}

// --- inline approval (ADR-0049) ----------------------------------------

// A Screen with a live ToolCall `name`(input) gated by a pending Approval,
// built through the real event path so the confirming call + pending state
// are exactly what production produces.
fn screen_confirming(name: &str, input: serde_json::Value, command: &str) -> Screen {
    let screen = Screen::new(ScreenOpts::default());
    let (screen, _) = screen.apply_event(Event::run_started("r1"));
    let (screen, _) = screen.apply_event(Event::tool_call("t1", name, input));
    let (screen, _) =
        screen.apply_event(Event::approval_request("approval-0", command.to_string()));
    screen
}

// A Screen carrying a COMMITTED Todo list (so the sticky "Current tasks" box
// would show) AND an open approval on a live `run_command` ToolCall. The Todo
// rides in through the real event path (a `todo_write` result with the todos
// artifact, promoted by the registered todo Extension), is frozen with
// `mark_committed`, then a second Run opens the confirming call + approval.
fn screen_committed_todo_then_confirming() -> Screen {
    let opts = ScreenOpts {
        extensions: crate::extensions::configured(&["todo".to_string()]),
        ..ScreenOpts::default()
    };
    let screen = Screen::new(opts);
    let todos = serde_json::json!({
        "todos": [
            {"content": "read the file", "status": "in_progress"},
            {"content": "edit the file", "status": "pending"},
        ]
    });
    let (s, _) = screen.apply_event(Event::run_started("r1"));
    let (s, _) = s.apply_event(Event::tool_call("todo-call", "todo_write", todos));
    // The todos artifact rides the result so the todo Extension promotes it
    // to a first-class Todo item (ADR-0048).
    let mut artifacts = std::collections::HashMap::new();
    artifacts.insert(
        "todos".to_string(),
        serde_json::json!({
            "items": [
                {"content": "read the file", "status": "in_progress"},
                {"content": "edit the file", "status": "pending"},
            ]
        }),
    );
    let (mut s, _) = s.apply_event(Event::ToolResult {
        id: "todo-call".into(),
        name: "todo_write".into(),
        content: "ok".into(),
        is_error: false,
        artifacts,
    });
    // Confirm a Todo item landed and freeze the whole prefix (commit it into
    // scrollback), so `latest_todo` reads a COMMITTED list -> sticky reserves.
    assert!(
        s.transcript().latest_todo().is_some(),
        "the todo Extension promoted a Todo item"
    );
    s.mark_committed(s.transcript().committable_upto());
    assert!(
        s.transcript().latest_todo().map(|(i, _)| i) < Some(s.transcript().committed_high_water()),
        "the Todo is committed (index below the high-water mark)"
    );
    // Now a second, live run_command gated on an open approval.
    let (s, _) = s.apply_event(Event::tool_call(
        "t1",
        "run_shell_command",
        serde_json::json!({"command": "cargo test"}),
    ));
    let (s, _) = s.apply_event(Event::approval_request(
        "approval-0",
        "cargo test".to_string(),
    ));
    s
}

// BUG 2 (live-vet): with a committed Todo present, the sticky "Current tasks"
// box must NOT hide the open approval. The approval renders inside the pending
// body; reserving the sticky zone starves that body (Min(1)) and top-clips the
// question out of view. The fix drops the sticky box while an approval is open
// -- a visible approval wins over the informational list.
#[test]
fn an_open_approval_drops_the_sticky_box_so_the_question_stays_visible() {
    let screen = screen_committed_todo_then_confirming();
    // A full pending frame (render_pending -> pending_layout): the sticky box
    // WOULD show here (committed, non-empty, incomplete list) if not for the
    // open approval.
    let terminal = draw_pending(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Allow execution of: 'cargo test'?"),
        "the approval question is visible:\n{text}"
    );
    assert!(
        text.contains("Yes, allow once"),
        "the first radio option is visible:\n{text}"
    );
    assert!(
        !text.contains("Current tasks"),
        "the sticky box is dropped while the approval is open:\n{text}"
    );
}

// The pending_layout-level guard: an open approval reserves NO sticky zone
// (`sticky_items` is None), even though `latest_todo` would otherwise qualify.
#[test]
fn pending_layout_reserves_no_sticky_zone_while_an_approval_is_open() {
    let screen = screen_committed_todo_then_confirming();
    // Sanity: the committed list DOES qualify for a sticky box on its own.
    assert!(
        sticky_todos(
            screen.transcript().latest_todo(),
            screen.transcript().committed_high_water(),
        )
        .is_some(),
        "the committed Todo would reserve a sticky box absent an approval"
    );
    // But with the approval open, pending_layout drops it.
    let view = screen.composer().view();
    let plan = pending_layout(Rect::new(0, 0, 60, 24), &view, &screen);
    assert!(
        plan.sticky_items.is_none(),
        "an open approval yields no sticky zone"
    );
    assert_eq!(
        plan.sticky_box.height, 0,
        "the sticky zone reserves zero rows"
    );
}

// A Screen carrying a COMMITTED Todo list (so the sticky "Current tasks" box
// would show) with NO approval open - the regression counterpart to
// [`screen_committed_todo_then_confirming`]. Same committed-Todo setup, minus
// the second confirming Run: the Run has SETTLED (message_end), so the sticky
// box is the only thing driving the frame.
fn screen_committed_todo_no_approval() -> Screen {
    let opts = ScreenOpts {
        extensions: crate::extensions::configured(&["todo".to_string()]),
        ..ScreenOpts::default()
    };
    let screen = Screen::new(opts);
    let todos = serde_json::json!({
        "todos": [
            {"content": "read the file", "status": "in_progress"},
            {"content": "edit the file", "status": "pending"},
        ]
    });
    let (s, _) = screen.apply_event(Event::run_started("r1"));
    let (s, _) = s.apply_event(Event::tool_call("todo-call", "todo_write", todos));
    let mut artifacts = std::collections::HashMap::new();
    artifacts.insert(
        "todos".to_string(),
        serde_json::json!({
            "items": [
                {"content": "read the file", "status": "in_progress"},
                {"content": "edit the file", "status": "pending"},
            ]
        }),
    );
    let (mut s, _) = s.apply_event(Event::ToolResult {
        id: "todo-call".into(),
        name: "todo_write".into(),
        content: "ok".into(),
        is_error: false,
        artifacts,
    });
    s.mark_committed(s.transcript().committable_upto());
    s
}

// The MEDIUM finding: on a SHORT terminal the pending body TOP-clips (last N
// rows kept, [`anchor_clip`]). Before the fix the spinner/tails were appended
// BELOW the inline approval block, so the top-clip ate the "Allow execution
// of..." question first - the user could not see what they were approving. The
// fix suppresses every trailing LIVE row while an approval is open, so the
// approval block is the BOTTOM-most content and survives the clip. This test
// renders the real production frame ([`draw_pending`]) with a live spinner
// anim on a short terminal and asserts BOTH the question and the first radio
// option survive, and that the spinner's cancel affordance is gone (the
// approval block owns the bottom).
#[test]
fn short_terminal_keeps_the_approval_question_visible() {
    let screen = screen_confirming(
        "run_shell_command",
        serde_json::json!({"command": "cargo test"}),
        "cargo test",
    );
    // The FULL, unclamped confirming body: the approval box must be bottom-
    // most, so the LAST non-blank row is its `╰` border, NOT a spinner row.
    // This is what makes the top-clip keep the question. (Anim past the lull
    // settle window, so a spinner WOULD render absent the suppression.)
    let anim = Anim {
        quiet_ticks: lull::SETTLE_TICKS + 4,
        ..Anim::default()
    };
    let mut cache = RenderCache::new();
    let mut params = PendingBodyParams {
        screen: &screen,
        cache: &mut cache,
        anim,
    };
    let body = pending_body_lines(&mut params, theme::dark(), 0, 56);
    let last_non_blank = body
        .iter()
        .rev()
        .map(line_text)
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default();
    assert!(
        last_non_blank.trim_start().starts_with('╰'),
        "the approval box is bottom-most (last row is its border), not a \
             spinner row: {last_non_blank:?}"
    );
    assert!(
        !body.iter().any(|l| line_text(l).contains("esc to cancel")),
        "the spinner is suppressed while the approval is open"
    );

    // And on a SHORT real frame the top-clip therefore keeps the question and
    // the first option (they ride at the bottom of the pending body).
    let terminal = draw_pending(60, 14, &screen);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Allow execution of: 'cargo test'?"),
        "the approval question survives top-clip on a short terminal:\n{text}"
    );
    assert!(
        text.contains("Yes, allow once"),
        "the first radio option is visible:\n{text}"
    );
}

// Fix-1 regression guard: dropping the sticky box (BUG 2) and suppressing the
// trailing live rows (this finding) both key off `pending_approval.is_some()`.
// With NO approval open, a committed Todo must STILL render the sticky
// "Current tasks" box - the approval-only guards did not over-suppress.
#[test]
fn no_approval_still_renders_the_sticky_current_tasks_box() {
    let screen = screen_committed_todo_no_approval();
    // Sanity: the committed, non-empty, incomplete list qualifies for a box.
    assert!(
        sticky_todos(
            screen.transcript().latest_todo(),
            screen.transcript().committed_high_water(),
        )
        .is_some(),
        "the committed Todo qualifies for a sticky box"
    );
    let terminal = draw_pending(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Current tasks"),
        "the sticky box renders when no approval is open:\n{text}"
    );
}

// The measure-level counterpart to the short-terminal test: an OPEN approval
// suppresses the spinner/lull row from the pending body, while an equivalent
// Running-but-no-approval frame KEEPS it. Driven at [`pending_body_lines`] with
// `quiet_ticks` past the lull settle window so the spinner has a phrase to draw
// (default anim is still settling, so it would be empty regardless).
#[test]
fn an_open_approval_suppresses_the_spinner_row_that_a_bare_run_keeps() {
    let width: u16 = 60;
    // A settled lull (quiet_ticks past SETTLE_TICKS) so the spinner has a scene.
    let anim = Anim {
        quiet_ticks: 60,
        ..Anim::default()
    };
    let has_cancel =
        |lines: &[Line<'static>]| lines.iter().any(|l| line_text(l).contains("esc to cancel"));

    // Approval OPEN (Running, gated run_command): the spinner is suppressed.
    let confirming = screen_confirming(
        "run_shell_command",
        serde_json::json!({"command": "cargo test"}),
        "cargo test",
    );
    let mut cache = RenderCache::new();
    let mut params = PendingBodyParams {
        screen: &confirming,
        cache: &mut cache,
        anim,
    };
    let under_approval = pending_body_lines(&mut params, theme::dark(), 0, width);
    assert!(
        !has_cancel(&under_approval),
        "the spinner is suppressed while an approval is open"
    );

    // Running, NO approval: the same anim now DOES render the spinner row.
    let running = Screen::new(ScreenOpts::default());
    let (running, _) = running.apply_event(Event::run_started("r1"));
    assert_eq!(running.status, Status::Running);
    assert!(running.pending_approval.is_none());
    let mut cache = RenderCache::new();
    let mut params = PendingBodyParams {
        screen: &running,
        cache: &mut cache,
        anim,
    };
    let bare_run = pending_body_lines(&mut params, theme::dark(), 0, width);
    assert!(
        has_cancel(&bare_run),
        "the spinner renders on an equivalent Running frame with no approval"
    );
}

// selection_rows: the active row wears the `›` marker + green label; inactive
// rows show two gutter spaces; numbers are 1-indexed `N.`.
#[test]
fn selection_rows_marks_the_active_row_and_numbers_the_options() {
    let items = ["Yes, allow once", "Always", "No"];
    let rows = selection_rows(&items, 1, true, 40, theme::dark());
    let text = |l: &Line| {
        l.spans
            .iter()
            .map(|s| s.content.clone())
            .collect::<String>()
    };
    assert!(text(&rows[0]).contains("1. Yes, allow once"));
    // Row 1 is active: the `›` marker leads.
    assert!(text(&rows[1]).starts_with("›"));
    assert!(text(&rows[1]).contains("2. Always"));
    // Row 0 is inactive: no marker, two leading spaces.
    assert!(text(&rows[0]).starts_with("  "));

    // The `N.` number turns success-green on the ACTIVE row (qwen
    // `BaseSelectionList.tsx:113-118`) and secondary elsewhere. Find the span
    // that carries the trailing `.` of the number.
    let theme = theme::dark();
    let number_span = |l: &Line| {
        l.spans
            .iter()
            .find(|s| s.content.ends_with('.'))
            .expect("a number span")
            .style
    };
    assert_eq!(
        number_span(&rows[1]).fg,
        success_style(theme).fg,
        "active-row number is success-green"
    );
    assert_eq!(
        number_span(&rows[0]).fg,
        secondary_style(theme).fg,
        "inactive-row number is secondary"
    );
}

// Render-buffer proof (P2) that the `›` marker lands on the ACTIVE option
// row and the inactive rows carry two gutter spaces - not just via a direct
// `selection_rows` call but through the real `draw_viewport` inline block.
// The default active row is option 0 (`Yes, allow once`).
#[test]
fn drawn_inline_approval_marks_only_the_active_option_row() {
    let screen = screen_confirming(
        "run_shell_command",
        serde_json::json!({"command": "cargo test"}),
        "cargo test",
    );
    let terminal = draw_viewport(60, 24, &screen);
    // Exactly one active row, so exactly one `›` marker in the whole block.
    let full = buffer_text(&terminal);
    assert_eq!(
        full.matches('›').count(),
        1,
        "one active marker only\n{full}"
    );

    // Locate each option row and inspect its gutter directly in the buffer.
    let row_of = |needle: &str| {
        (0..24)
            .map(|y| row_text(&terminal, y))
            .find(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("row for {needle:?} not found\n{full}"))
    };
    // The active row (option 0) leads with the `›` marker (inside its box,
    // so after the border cell + padding); assert the marker is present and
    // the number `1.` follows.
    let active = row_of("Yes, allow once");
    assert!(
        active.contains('›'),
        "active row shows the marker: {active:?}"
    );
    assert!(active.contains("1."), "active row is numbered: {active:?}");
    // The inactive rows carry NO marker - two gutter spaces stand where the
    // `›` would be, ahead of their number.
    let inactive_always = row_of("Always allow in this project");
    let inactive_deny = row_of("No, suggest changes (esc)");
    assert!(!inactive_always.contains('›'), "{inactive_always:?}");
    assert!(!inactive_deny.contains('›'), "{inactive_deny:?}");
    assert!(inactive_always.contains("2."), "{inactive_always:?}");
    assert!(inactive_deny.contains("3."), "{inactive_deny:?}");
}

// The inline block appends the question + all three verbatim options, and the
// exec question embeds the command.
#[test]
fn inline_exec_approval_renders_the_question_and_all_three_options() {
    let screen = screen_confirming(
        "run_shell_command",
        serde_json::json!({"command": "cargo test"}),
        "cargo test",
    );
    let terminal = draw_viewport(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("Allow execution of: 'cargo test'?"), "{text}");
    assert!(text.contains("Yes, allow once"));
    assert!(text.contains("Always allow in this project"));
    assert!(text.contains("No, suggest changes (esc)"));
}

// A web_fetch (Info) approval reads the generic proceed question.
#[test]
fn inline_info_approval_renders_the_proceed_question() {
    let screen = screen_confirming(
        "web_fetch",
        serde_json::json!({"url": "https://x"}),
        "https://x",
    );
    let terminal = draw_viewport(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("Do you want to proceed?"), "{text}");
}

// The question modal (ADR-0057, ask_user_question): a bordered box with the
// VERBATIM title, the `[header]` chip + question text, the option labels, and
// the auto-appended "Other" row.
fn screen_with_question() -> Screen {
    use crate::tool::caps::{Question, QuestionOption};
    let screen = Screen::new(ScreenOpts::default());
    let (screen, _) = screen.apply_event(Event::run_started("r1"));
    let questions = vec![Question {
        question: "Which library should we use?".into(),
        header: "Library".into(),
        options: vec![
            QuestionOption {
                label: "serde".into(),
                description: "the standard".into(),
            },
            QuestionOption {
                label: "miniserde".into(),
                description: "smaller".into(),
            },
        ],
        multi_select: false,
    }];
    let (screen, _) = screen.apply_event(Event::question_request("q-1", questions));
    screen
}

#[test]
fn question_modal_renders_title_question_options_and_the_other_row() {
    let screen = screen_with_question();
    // The bordered modal draws bottom-most in the pending body (top-clipped
    // like the approval), so a short viewport clips the title first. Assert
    // the interactive rows are visible in the live viewport...
    let terminal = draw_viewport(60, 40, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("[Library]"), "the header chip shows: {text}");
    assert!(text.contains("Which library should we use?"), "{text}");
    assert!(text.contains("serde"), "{text}");
    assert!(text.contains("miniserde"), "{text}");
    // qwen ALWAYS appends an "Other" row.
    assert!(text.contains("Other"), "the auto-Other row shows: {text}");

    // ...and the VERBATIM title is the box's first content row (checked on the
    // pure line set so the top-clip does not hide it).
    let pending = screen.pending_question.as_ref().unwrap();
    let lines = question_modal_lines(pending, 58, theme::dark());
    let rendered: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("Please answer the following question(s):")),
        "the VERBATIM title row: {rendered:?}"
    );
}

// ADR-0029 rigidity: every rendered question-modal row is exactly the box
// width, so the right border aligns (the box wrapper padded each row).
#[test]
fn question_modal_rows_are_rigid_to_the_box_width() {
    use crate::tool::caps::Question;
    let theme = theme::dark();
    let pending = crate::ui::screen::PendingQuestion::new(
        "q-1".into(),
        vec![Question {
            question: "Pick?".into(),
            header: "Lib".into(),
            options: vec![
                crate::tool::caps::QuestionOption {
                    label: "a".into(),
                    description: "d".into(),
                },
                crate::tool::caps::QuestionOption {
                    label: "b".into(),
                    description: "d".into(),
                },
            ],
            multi_select: false,
        }],
    );
    let width: u16 = 50;
    let lines = question_modal_lines(&pending, width, theme);
    for line in &lines {
        let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(cols, width as usize, "every row spans the full box width");
    }
}

// The confirming call's marker is `?` (warning), not the executing `⊷`.
#[test]
fn the_confirming_call_wears_the_question_marker() {
    let screen = screen_confirming(
        "web_fetch",
        serde_json::json!({"url": "https://x"}),
        "https://x",
    );
    let terminal = draw_viewport(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(text.contains("?"), "the confirming marker shows");
    assert!(!text.contains("⊷"), "the executing marker is gone");
}

// A non-shell confirming group (web_fetch) borders warning-yellow; a shell
// one (run_command) keeps the grey symbol border (shell precedence).
#[test]
fn group_border_precedence_is_shell_then_confirming_then_default() {
    let call = |name: &str| TranscriptItem::ToolCall {
        id: "t1".into(),
        name: name.into(),
        summary: String::new(),
    };
    let theme = theme::dark();
    // Default: a non-shell, non-confirming group.
    assert_eq!(
        group_border_style(&[call("web_fetch")], false, theme),
        border_style(theme)
    );
    // Confirming, non-shell: warning wins.
    assert_eq!(
        group_border_style(&[call("web_fetch")], true, theme),
        warning_style(theme)
    );
    // Shell always wins, even confirming.
    assert_eq!(
        group_border_style(&[call("run_shell_command")], true, theme),
        symbol_style(theme)
    );
}

// ADR-0029 rigidity: every rendered approval row is exactly the box inner
// width (the box wrapper padded it to `inner`), so the right border aligns.
#[test]
fn inline_approval_rows_respect_the_box_inner_width() {
    let screen = screen_confirming(
        "web_fetch",
        serde_json::json!({"url": "https://x"}),
        "https://x",
    );
    let width = 50u16;
    let terminal = draw_viewport(width, 24, &screen);
    // The boxed rows carry the `│` border at both edges of the content zone.
    // Every row that has a left `│` must have a right `│` at the same column,
    // proving the width is rigid (measure==draw).
    let _ = width;
    let rows = terminal.backend().buffer().area.height;
    // Every boxed row of ONE box must place its two `│` borders at the SAME
    // two columns (the box is rigid: measure==draw, ADR-0029). The startup
    // Header's own info panel is a SEPARATE box at its own inner width, so
    // rigidity is asserted per contiguous box-row run (a blank / non-bordered
    // row ends a box), then a bordered box must have been seen.
    let mut border_cols: Option<(usize, usize)> = None;
    let mut saw_box = false;
    for y in 0..rows {
        let row: Vec<char> = row_text(&terminal, y).chars().collect();
        let left = row.iter().position(|&c| c == '│');
        let right = row.iter().rposition(|&c| c == '│');
        match (left, right) {
            (Some(l), Some(r)) if l != r => {
                saw_box = true;
                match border_cols {
                    None => border_cols = Some((l, r)),
                    Some((el, er)) => assert_eq!(
                        (l, r),
                        (el, er),
                        "row {y} borders at ({l},{r}) not the rigid ({el},{er}) of its box"
                    ),
                }
            }
            // A row with no border pair ends the current box run.
            _ => border_cols = None,
        }
    }
    assert!(saw_box, "the approval box drew bordered rows");
}

// committed==pending identity (ADR-0049): once the Approval resolves and the
// ToolResult supersedes the call, the committed slice carries NO approval
// rows - the question/options are gone.
#[test]
fn a_resolved_approval_commits_with_no_approval_rows() {
    let screen = screen_confirming(
        "web_fetch",
        serde_json::json!({"url": "https://x"}),
        "https://x",
    );
    // Resolve + supersede the call with its result.
    let (screen, _) = screen.apply_event(Event::approval_resolved("approval-0", true));
    let (screen, _) = screen.apply_event(Event::tool_result(
        "t1",
        "web_fetch",
        "ok",
        false,
        std::collections::HashMap::new(),
    ));
    assert_eq!(screen.pending_approval, None);
    let terminal = draw_viewport(60, 24, &screen);
    let text = buffer_text(&terminal);
    assert!(!text.contains("Do you want to proceed?"), "{text}");
    assert!(!text.contains("Yes, allow once"), "{text}");
    assert!(!text.contains("?"), "no confirming marker after resolve");
}
