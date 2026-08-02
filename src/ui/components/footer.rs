use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::approvals::ApprovalMode;
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;

use super::pending::ConnectionView;
use super::style::{error_style, secondary_style, success_style, warning_style};

// ---------------------------------------------------------------------------
// The flat footer (ADR-0053, qwen `Footer.tsx`): ONE row, space-between,
// `paddingX:2`, no powerline triangles, no block backgrounds. Replaces the
// powerline status bar (retiring ADR-0046's `status_bar` and the ADR-0008/0040
// segment palette).
// ---------------------------------------------------------------------------

/// The cost threshold below which `cost_label` emits the `<$0.01` floor label
/// instead of a two-decimal dollar amount.
const COST_SUB_CENT: f64 = 0.01;

/// The sentinel session cost below which the Cost segment is hidden entirely: a
/// session that spent nothing (or whose provider carries no Catalog pricing) shows
/// exactly the bar it always did.
const COST_HIDDEN: f64 = 0.0;

/// The horizontal inset the footer row wears on each side (qwen `paddingX:2`).
const FOOTER_PADDING_X: usize = 2;

/// The ` | ` separator qwen joins the right-group items with (`text.secondary`),
/// emitted BETWEEN items only - no leading separator (qwen `index > 0`).
pub(super) const FOOTER_SEP: &str = " | ";

// Note: qwen's `isNarrowWidth` (< 80) switches the footer to a two-line column;
// suspenders instead stays ONE row and sheds right items (cost → model), a
// documented divergence (ADR-0046 fixed a 1-row footer zone, ADR-0053). The shed
// is width-driven (see [`Footer::fit`]), so there is no narrow-threshold const.

/// One right-group footer item's MEANING, ratatui-free (ADR-0019). The pure
/// assembly ([`footer`]) emits these carrying only the display state they
/// convey - no colours (that is [`render_footer`], ADR-0008), no separators.
/// The testable seam: the footer's contents can be asserted without a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterItem {
    /// The Active Model (qwen keeps model/cost out of its footer; suspenders
    /// keeps them as the load-bearing local-first facts, Option B, ADR-0053).
    /// Rendered `text.secondary` like the whole right group.
    Model {
        /// The Active Model identifier.
        model: String,
    },
    /// The context-usage figure (qwen `ContextUsageDisplay`, ADR-0048): the
    /// flat `NN.N% used` / `NN.N% context used` label (the pure
    /// [`context_percent_label`] rule) and whether usage is over the budget (the
    /// painter reads `error` when so, else `text.secondary`). Assembled only when
    /// a budget exists.
    Context {
        /// The flat `context_percent_label` figure (no block padding).
        label: String,
        /// Whether the Conversation is over its context budget.
        over_limit: bool,
    },
    /// The Session's cumulative dollar cost (ADR-0037), the pre-formatted
    /// [`cost_label`] total. Assembled only when the total is positive.
    Cost {
        /// The [`cost_label`]-formatted total, e.g. `$0.42` or `<$0.01`.
        label: String,
    },
}

impl FooterItem {
    /// This item's display text (no separator, no padding). Semantics-in,
    /// text-out - the seam ADR-0019 wants.
    pub(super) fn text(&self) -> String {
        match self {
            FooterItem::Model { model } => format!("model {model}"),
            FooterItem::Context { label, .. } => label.clone(),
            FooterItem::Cost { label } => label.clone(),
        }
    }

    /// The columns this item occupies once painted, ratatui-free. Kept in
    /// lockstep with [`FooterItem::text`] so the fit policy measures what the
    /// painter draws.
    pub(super) fn cells(&self) -> usize {
        self.text().chars().count()
    }
}

/// The footer's left content (qwen `leftBottomContent`): the AutoAcceptIndicator
/// when the Approval mode is not Default, else the `? for shortcuts` hint.
/// Ratatui-free - the painter picks the per-mode colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterLeft {
    /// The AutoAcceptIndicator (qwen `AutoAcceptIndicator.tsx`, ADR-0050): the
    /// mode's label + the ` (shift + tab to cycle)` hint. Never `Default` (that
    /// falls to [`FooterLeft::Shortcuts`], matching qwen rendering nothing).
    AutoAccept(ApprovalMode),
    /// The resting `? for shortcuts` hint (qwen's fallthrough), `text.secondary`.
    Shortcuts,
}

/// The ratio at/above which the context figure reads `>100` (qwen `>1`).
const CONTEXT_FULL_RATIO: f64 = 1.0;
/// The percent scaling applied to the usage ratio for display.
const CONTEXT_PERCENT_SCALE: f64 = 100.0;
/// The terminal width below which the context label shortens to `% used` (qwen's
/// `terminalWidth < 100` rule).
const CONTEXT_NARROW_WIDTH: usize = 100;

/// The context-usage figure (qwen `ContextUsageDisplay` / `formatPercentageUsed`,
/// ADR-0048): `percentage = tokens / budget`; `>1 → ">100"` else `(p*100)` to one
/// decimal, then the label - `% used` when the terminal is narrower than
/// [`CONTEXT_NARROW_WIDTH`] else `% context used` (the leading `%` is part of the
/// label). FLAT - no block padding (the powerline wrapper is retired, ADR-0053);
/// the footer joins items with ` | ` instead. A zero (or missing) budget has no
/// figure to show.
pub(super) fn context_percent_label(tokens: u64, budget: u64, width: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let ratio = tokens as f64 / budget as f64;
    let figure = if ratio > CONTEXT_FULL_RATIO {
        ">100".to_string()
    } else {
        format!("{:.1}", ratio * CONTEXT_PERCENT_SCALE)
    };
    let label = if width < CONTEXT_NARROW_WIDTH {
        "% used"
    } else {
        "% context used"
    };
    Some(format!("{figure}{label}"))
}

/// Whether the context usage is over the budget (qwen `isOverLimit`): the figure
/// reads `error` rather than secondary. Separated from the label so the painter
/// routes the colour without re-deriving the ratio.
pub(super) fn context_over_limit(tokens: u64, budget: u64) -> bool {
    budget > 0 && tokens > budget
}

/// The Cost item's display text (ADR-0037: the Session's cumulative
/// Catalog-priced total, in dollars). Two decimals from a cent up; a flat
/// `<$0.01` below that - a sub-cent figure would render `$0.00` and read as
/// free. Only prices a positive total: the assembly hides the item entirely at
/// zero, so this never formats one.
pub fn cost_label(total: f64) -> String {
    if total < COST_SUB_CENT {
        "<$0.01".to_string()
    } else {
        format!("${total:.2}")
    }
}

/// The footer's assembled MEANING (ADR-0053): the left content and the ordered
/// ` | `-joined right group (model, context%, cost), already fitted to the
/// terminal width. Pure and ratatui-free - what the colocated tests assert
/// against without drawing a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// The left content (AutoAcceptIndicator or the shortcuts hint).
    pub left: FooterLeft,
    /// The right group, in display order (model, context%, cost), post-fit.
    pub right: Vec<FooterItem>,
}

impl Footer {
    /// Drops right-group items until the footer fits `width`, lowest-value
    /// first: cost, then model - context% NEVER drops (it is qwen's sole footer
    /// figure and the one the operator steers by). Which items show at a given
    /// width is a SEMANTIC decision, so it lives here in the pure layer; the
    /// arithmetic reads each item's own [`FooterItem::cells`] plus the left
    /// content, the ` | ` joins, and the two-side padding. This is the narrow
    /// "shed, don't wrap" divergence from qwen's two-line stack (ADR-0053).
    fn fit(mut self, width: usize, left_cells: usize) -> Footer {
        let drop_order: [fn(&FooterItem) -> bool; FOOTER_DROP_TIER_COUNT] = [
            |i| matches!(i, FooterItem::Cost { .. }),
            |i| matches!(i, FooterItem::Model { .. }),
        ];
        for dropped in drop_order {
            if self.cells(left_cells) <= width {
                break;
            }
            self.right.retain(|i| !dropped(i));
        }
        self
    }

    /// The columns the footer occupies: the two-side padding, the left content,
    /// a gap of at least one cell, and the right group (item texts + one ` | `
    /// join between each pair).
    fn cells(&self, left_cells: usize) -> usize {
        FOOTER_PADDING_X * 2 + left_cells + FOOTER_MIN_GAP + self.right_cells()
    }

    /// The right group's painted width: the item texts plus one ` | ` separator
    /// between each adjacent pair (no leading separator).
    pub(super) fn right_cells(&self) -> usize {
        let text: usize = self.right.iter().map(FooterItem::cells).sum();
        let joins = self.right.len().saturating_sub(1) * FOOTER_SEP.chars().count();
        text + joins
    }
}

/// The figures the footer's right side draws: the token estimate (`None` before
/// any estimate exists) the context figure measures against the budget, the
/// Session's cumulative dollar cost (ADR-0037; `0.0` hides the Cost item), and
/// the context budget for the `% used` figure (ADR-0048).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FigureView {
    /// The token estimate the context figure divides by the budget. The
    /// powerline's per-`PressureLevel` block colour retired with it (ADR-0053);
    /// the flat footer routes context colour off `over_limit` alone, so only the
    /// count survives.
    pub tokens: Option<u64>,
    pub session_cost: f64,
    /// The Conversation's context budget (the model's usable window). `None`
    /// before any estimate/budget arrives; a zero budget shows no figure.
    pub context_budget: Option<u64>,
}

/// All display facts for one footer assembly, bundled to keep [`footer`] within
/// the param ceiling. Each field is an independent semantic fact the footer
/// renders.
pub(crate) struct FooterView<'a> {
    pub(crate) conn: ConnectionView<'a>,
    pub(crate) figures: FigureView,
    /// The current Approval mode (ADR-0050), for the left AutoAcceptIndicator.
    /// `Default` shows the shortcuts hint instead.
    pub(crate) approval_mode: ApprovalMode,
}

/// Assembles the footer's MEANING, pure and ratatui-free (ADR-0019, ADR-0053):
/// the left content and the ordered right group (model, context%, cost), fitted
/// to `width`. No colours, glyphs, or separators are decided here - that is the
/// painter's job ([`render_footer`]) - so every rule (item order, the shed/drop
/// policy, the model-always / cost-hidden-at-zero / context-needs-a-budget
/// rules, the Default-mode-shows-shortcuts rule) is assertable without a frame.
pub(crate) fn footer(width: usize, view: FooterView<'_>) -> Footer {
    let FooterView {
        conn,
        figures,
        approval_mode,
    } = view;

    let left = if approval_mode == ApprovalMode::Default {
        FooterLeft::Shortcuts
    } else {
        FooterLeft::AutoAccept(approval_mode)
    };

    let mut right = vec![FooterItem::Model {
        model: conn.model.to_string(),
    }];
    // The context-usage figure (qwen `ContextUsageDisplay`, ADR-0048): shown once
    // a budget exists. The label's `% used` / `% context used` form depends on the
    // terminal `width` (qwen's <100-column rule).
    if let Some(estimate) = figures.tokens
        && let Some(budget) = figures.context_budget
        && let Some(label) = context_percent_label(estimate, budget, width)
    {
        right.push(FooterItem::Context {
            label,
            over_limit: context_over_limit(estimate, budget),
        });
    }
    // The cost item exists only once a priced Response landed: at zero the
    // Session has spent nothing meterable and the footer omits it.
    if figures.session_cost > COST_HIDDEN {
        right.push(FooterItem::Cost {
            label: cost_label(figures.session_cost),
        });
    }

    Footer { left, right }.fit(width, footer_left_cells(left))
}

/// The AutoAcceptIndicator's mode label (qwen `AutoAcceptIndicator.tsx` verbatim,
/// ADR-0050): `plan mode` / `auto-accept edits` / `auto mode (classifier-
/// evaluated)` / `YOLO mode`. `Default` has no label (it never renders as an
/// indicator); it maps to the empty string defensively so the function is total.
pub(super) fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => "plan mode",
        ApprovalMode::AutoEdit => "auto-accept edits",
        ApprovalMode::Auto => "auto mode (classifier-evaluated)",
        ApprovalMode::Yolo => "YOLO mode",
        ApprovalMode::Default => "",
    }
}

/// The AutoAcceptIndicator's per-mode label colour (qwen `AutoAcceptIndicator`):
/// plan → success, auto-edit/auto → warning, yolo → error. Default never renders
/// as an indicator, so it borrows the neutral secondary defensively.
pub(super) fn approval_mode_style(mode: ApprovalMode, theme: &Theme) -> Style {
    match mode {
        ApprovalMode::Plan => success_style(theme),
        ApprovalMode::AutoEdit | ApprovalMode::Auto => warning_style(theme),
        ApprovalMode::Yolo => error_style(theme),
        ApprovalMode::Default => secondary_style(theme),
    }
}

/// The AutoAcceptIndicator's cycle hint (qwen ` (shift + tab to cycle)`), carried
/// with a leading space so it reads as one phrase across the colour boundary.
pub(super) const CYCLE_HINT: &str = " (shift + tab to cycle)";

/// The resting left hint (qwen `? for shortcuts`).
pub(super) const SHORTCUTS_HINT: &str = "? for shortcuts";

/// The left content's painted width (ratatui-free), so the pure [`Footer::fit`]
/// measures the same cells the painter draws.
fn footer_left_cells(left: FooterLeft) -> usize {
    match left {
        FooterLeft::AutoAccept(mode) => {
            approval_mode_label(mode).chars().count() + CYCLE_HINT.chars().count()
        }
        FooterLeft::Shortcuts => SHORTCUTS_HINT.chars().count(),
    }
}

/// The number of shed tiers in the footer's right-group drop policy.
const FOOTER_DROP_TIER_COUNT: usize = 2;

/// The minimum gap the footer reserves between the left content and the right
/// group (qwen's space-between never lets them touch).
const FOOTER_MIN_GAP: usize = 1;

/// Screen-state bundle for [`render_footer`], so the painter stays within the
/// param ceiling.
pub(crate) struct FooterCtx<'a> {
    pub(crate) screen: &'a Screen,
    pub(crate) conn: ConnectionView<'a>,
}

/// The flat bottom footer (ADR-0053, qwen `Footer.tsx`): ONE row, hand-rolled
/// space-between with a [`FOOTER_PADDING_X`] inset on each side and NO background
/// fill. A thin painter over the pure [`footer`] assembly (which items, in what
/// order): the left content wears its per-mode colour, the right group is
/// ` | `-joined in `text.secondary` (context% goes `error` when over-limit).
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, ctx: FooterCtx<'_>, theme: &Theme) {
    let FooterCtx { screen: t, conn } = ctx;
    let width = area.width as usize;
    let bar = footer(
        width,
        FooterView {
            conn,
            figures: FigureView {
                tokens: t.token_estimate,
                session_cost: t.session_cost,
                context_budget: t.context_budget,
            },
            approval_mode: t.approval_mode,
        },
    );

    // Left content: the AutoAcceptIndicator (mode label + hint) or the shortcuts
    // hint. The hint is always `text.secondary` (qwen); the mode label wears its
    // per-mode colour.
    let mut left_spans: Vec<Span> = Vec::new();
    match bar.left {
        FooterLeft::AutoAccept(mode) => {
            left_spans.push(Span::styled(
                approval_mode_label(mode).to_string(),
                approval_mode_style(mode, theme),
            ));
            left_spans.push(Span::styled(CYCLE_HINT.to_string(), secondary_style(theme)));
        }
        FooterLeft::Shortcuts => {
            left_spans.push(Span::styled(
                SHORTCUTS_HINT.to_string(),
                secondary_style(theme),
            ));
        }
    }

    // Right group: ` | `-joined in `text.secondary`, no leading separator; the
    // context% item goes `error` when over the budget (qwen's inner colour).
    let mut right_spans: Vec<Span> = Vec::new();
    for (i, item) in bar.right.iter().enumerate() {
        if i > 0 {
            right_spans.push(Span::styled(FOOTER_SEP, secondary_style(theme)));
        }
        let style = match item {
            FooterItem::Context {
                over_limit: true, ..
            } => error_style(theme),
            _ => secondary_style(theme),
        };
        right_spans.push(Span::styled(item.text(), style));
    }

    // Hand-rolled space-between (qwen `justifyContent:"space-between"`): pad the
    // gap between the inset left and right groups so the right group sits flush
    // against the right inset.
    let left_cells: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_cells: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let content = FOOTER_PADDING_X * 2 + left_cells + right_cells;
    let gap = width.saturating_sub(content).max(FOOTER_MIN_GAP);

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" ".repeat(FOOTER_PADDING_X)));
    spans.append(&mut left_spans);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.append(&mut right_spans);
    spans.push(Span::raw(" ".repeat(FOOTER_PADDING_X)));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
