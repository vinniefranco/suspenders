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

// The parent BODY renders only the per-item cache's window math (`wrapped_count`)
// and owns the shared cache facts (`Toggles`, `CONTENT_MARGIN`); each render
// concern lives in its own submodule and imports the primitives it needs
// EXPLICITLY. So the parent's own imports are just the few ratatui types
// `wrapped_count` draws with.
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

// ---------------------------------------------------------------------------
// The per-responsibility render submodules (the god-module split, ADR-0001/0019).
// Each owns ONE rendering concern and reaches the shared primitives + siblings it
// needs via its own `use super::*`. The parent imports EXPLICITLY (no wildcard
// re-export) only what its own body calls, plus the public render API `ui.rs`
// drives; the colocated test module globs each submodule directly (a test-file
// wildcard the production gate does not flag).
// ---------------------------------------------------------------------------

/// The semantic→ratatui Style/Color mapping (ADR-0008/0038).
mod style;
pub use style::md_style;

/// The rounded-box drawing primitives (`box_top`/`box_bottom`/`box_row`/
/// `frame_box`).
mod box_draw;

/// The shared text + column primitives (`push_cols`, `truncate_cols`, …).
mod text;

/// The fuzzy `/` palette + numbered `›` dialog popups (ADR-0051).
mod popup;

/// The sticky "Current tasks" box (ADR-0048).
mod sticky;

/// The startup Header banner (qwen `AppHeader`).
mod header;

/// The body-region overlays: the Help panel + the `/mcp` dialog.
mod overlay;

/// The interactive Approval + question modal (ADR-0049/0057).
mod approval;

/// Transcript-item + tool-group rendering (ADR-0047/0049).
mod tool_group;

/// Diff rendering + code-fence syntax highlighting + markdown (ADR-0008).
mod diff;

/// The flat bottom footer (ADR-0053).
mod footer;
pub use footer::FigureView;

/// The Composer input (ADR-0048, qwen `InputPrompt`). Named `composer_input` to
/// avoid clashing with the imported `crate::ui::composer` layout core.
mod composer_input;
pub use composer_input::render_composer;

/// The `--resume` Session Picker modal.
mod picker;
pub use picker::render_picker;

/// The inline PENDING region + committed-slice blit + Ctrl-S peek (ADR-0046).
/// The public render API is re-exported for `ui.rs`.
mod pending;
pub use pending::{
    Anim, CommittedSlice, ConnectionFacts, ConnectionView, FrameCtx, PendingBodyParams,
    PendingPeek, commit_slice_height, pending_peek_height, render_committed_slice, render_pending,
    render_pending_peek, sync_commit_cache,
};

/// The content side margin (columns): qwen `HistoryItemDisplay` wraps every item
/// in `marginLeft:2, marginRight:2` (HistoryItemDisplay.tsx:64), so content is
/// the frame width minus a 2-col left AND 2-col right margin. `pub(crate)` so the
/// adapter sizes the committed-slice content width (ADR-0046) at the same margin
/// the pending region uses.
pub(crate) const CONTENT_MARGIN: u16 = 2;

// ---------------------------------------------------------------------------
// The per-item render cache + the visible-window math.
//
// WHY: rebuilding every settled item's lines (markdown parse + syntect
// highlight) and re-wrapping the whole session on EVERY frame pegged a core
// while scrolling and made typing expensive - each keystroke only changes the
// Composer, each wheel tick only a scroll offset. Settled items never change
// content under an unchanged `Transcript::revision` (the store's contract:
// appends never bump, structural edits always do), so their lines and wrapped
// counts are built once and reused; the frame then renders only the items
// intersecting the window.
// ---------------------------------------------------------------------------

/// The single detail-on-demand display toggle the settled lines are built with:
/// compact mode (Ctrl+O, qwen `compactMode`, ADR-0052). `compact == true` hides
/// settled Thinking items entirely and folds tool result bodies to their header
/// rows. Named field (not a bare `bool` parameter) so the cache key reads at
/// every call site and a future second display fact has an obvious home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Toggles {
    pub(crate) compact: bool,
}

pub use render_cache::RenderCache;

/// The cache is a private child module (the same move ADR-0034 made for the
/// store's streaming snapshot): still in `ui/components`, still ratatui
/// [`Line`]s (ADR-0029 rejects a frame-free extraction). The boundary exists
/// so the fields are genuinely private - the frame path reads through the two
/// accessors - and the extend-vs-rebuild invariant is pinned by unit tests at
/// this seam, next to the state they inspect, not through full-screen renders.
mod render_cache;

/// The rows `lines` wrap to at `width`, measured by a throwaway `Paragraph`
/// with the SAME `Wrap { trim: false }` the viewport draws with - the window
/// math is only correct if measuring and drawing agree exactly.
fn wrapped_count(lines: Vec<Line<'static>>, width: u16) -> usize {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "../../tests/ui/components.rs"]
mod tests;
