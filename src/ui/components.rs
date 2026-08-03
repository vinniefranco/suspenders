//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`DiffSide`] →
//! color for a diff's lines, [`FooterItem`] → the flat footer's grey right
//! group. Extensions and the Screen core never touch ratatui; they speak the
//! vocabulary and this module renders it. Everything here is pure presentation
//! of [`TranscriptItem`]s - no state, no IO. Only this module and [`crate::ui`]
//! `use ratatui` / `use crossterm` (ADR-0019 invariant).

#[cfg(test)]
use ratatui::Frame;
#[cfg(test)]
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::{Color, Modifier, Style};
#[cfg(test)]
use ratatui::text::{Line, Span};
#[cfg(test)]
use ratatui::widgets::{Paragraph, Wrap};

#[cfg(test)]
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
use crate::plan::TodoItem;
#[cfg(test)]
use crate::ui::composer::OverlayView;
#[cfg(test)]
use crate::ui::screen::{Screen, Status};
#[cfg(test)]
use crate::ui::theme::Theme;

#[cfg(test)]
use crate::approvals::ApprovalMode;
#[cfg(test)]
use crate::plan::TodoStatus;
#[cfg(test)]
use crate::ui::completion;
#[cfg(test)]
use crate::ui::composer::{self, OverlayStatus};
#[cfg(test)]
use crate::ui::lull;
#[cfg(test)]
use crate::ui::markdown::MdStyle;
#[cfg(test)]
use crate::ui::theme;
#[cfg(test)]
use crate::view_model::SelectorRow;
#[cfg(test)]
use crate::view_model::Tone;
#[cfg(test)]
use crate::view_model::TranscriptItem;
#[cfg(test)]
use crate::view_model::{DiffHunk, DiffSide};

// The render helpers now live in cohesive submodules (Wave 2 split): the frame
// orchestration in `pending`, the semantic→color mapping in `style`, the tool
// group + box body in `tool_group`, the footer in `footer`, and so on. This root
// is the module seam: it declares the submodules and re-exports the public API
// (and, `#[cfg(test)]`, the internals the colocated test module reaches by bare
// name through `use super::*`).

pub use render_cache::RenderCache;

// Tier-0 leaf primitives (Wave 1 split): the semantic→color mapping, the pure
// text/column helpers, and the box-border glyphs each live in their own leaf
// submodule. Re-exported by bare name so the rest of this module and the test
// module's `use super::*` resolve them unchanged.
mod box_draw;
mod style;
mod text;

pub use style::md_style;
#[cfg(test)]
use style::{
    accent_style, border_style, diff_chrome_style, diff_side_fg, error_style, primary_style,
    secondary_style, success_style, symbol_style, tui_color, warning_style,
};
#[cfg(test)]
use text::wrap_words;

mod approval;
mod composer_input;
mod diff;
mod footer;
mod header;
mod markdown_render;
mod message;
mod overlay;
mod pending;
mod picker;
mod popup;
mod scroll;
mod spinner;
mod sticky;
mod suggestion;
mod tool_body;
mod tool_group;

pub use pending::{
    Anim, ConnectionFacts, ConnectionView, FrameCtx, PendingBodyParams, body_height, render_pending,
};

#[cfg(test)]
use message::{message_lines, settled_thinking_lines};
#[cfg(test)]
use tool_body::{BOX_CHROME, STATUS_INDICATOR_WIDTH, tool_todo_lines};
#[cfg(test)]
use tool_group::{
    Approving, CONTENT_MARGIN, GroupedRows, MAX_CONTENT_WIDTH, Segment, Toggles, content_width,
    group_border_style, group_segments, grouped_rows, grouped_rows_with_approval,
    newest_live_tool_index, wrapped_count,
};

pub use picker::render_picker;
#[cfg(test)]
use popup::{MAX_SUGGESTIONS, POPUP_MAX_ROWS, popup_rect, render_composer_popup};
#[cfg(test)]
use suggestion::prepare_label;

pub use composer_input::render_composer;
#[cfg(test)]
use composer_input::{COMPOSER_CHROME_ROWS, composer_is_empty};
#[cfg(test)]
use footer::{
    CYCLE_HINT, FOOTER_SEP, FooterView, SHORTCUTS_HINT, approval_mode_label, approval_mode_style,
    context_over_limit, context_percent_label, footer,
};
pub use footer::{FigureView, Footer, FooterItem, FooterLeft, cost_label};

#[cfg(test)]
use approval::SELECTION_MARKER;
#[cfg(test)]
use approval::selection_rows;
#[cfg(test)]
use diff::{
    diff_elided_tail, diff_lines, hunk_line_numbers, normalize_diff_text, parse_hunk_header,
};
#[cfg(test)]
use header::{HeaderTier, header_tier, tildeify_with_home};
#[cfg(test)]
use markdown_render::{CODE_INSET, CodeFragment, highlight_code, markdown_lines};
#[cfg(test)]
use overlay::{help_panel_lines, plan_modal_lines, question_modal_lines};
#[cfg(test)]
use pending::{
    append_live, capped_composer_height, frame_chunks, pending_body_lines, pending_layout,
    render_pending_body_at,
};
#[cfg(test)]
use scroll::{ScrollIntent, anchor_clip, scrolled_clip};
#[cfg(test)]
use spinner::{SpinnerState, format_token_count, live_thinking_lines, spinner_line};
#[cfg(test)]
use sticky::{
    ordered_sticky_todos, render_sticky_todos, sticky_fits, sticky_todos, sticky_todos_height,
};

/// The cache is a private child module (the same move ADR-0034 made for the
/// store's streaming snapshot): still in `ui/components`, still ratatui
/// [`Line`]s (ADR-0029 rejects a frame-free extraction). The boundary exists
/// so the fields are genuinely private - the frame path reads through the two
/// accessors - and the extend-vs-rebuild invariant is pinned by unit tests at
/// this seam, next to the state they inspect, not through full-screen renders.
mod render_cache;

#[cfg(test)]
#[path = "../../tests/ui/components.rs"]
mod tests;
