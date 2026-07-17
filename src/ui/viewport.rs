//! UI Viewport - the pure, bottom-anchored scroll state of the transcript
//! view. Lives in its own module because `ui.rs` is untested-by-design
//! (ADR-0001's split): the scroll rules are all here, all unit-tested, and
//! plain `usize` in/out - no ratatui types (ADR-0019); the adapter saturates
//! to `u16` only at the ratatui boundary, since long sessions can exceed
//! `u16` wrapped lines.
//!
//! The contract:
//!
//! * **Pinned follows the tail exactly** (`tail -f`): while pinned, the last
//!   wrapped line is always the bottom line of the viewport, however much
//!   content streams in.
//! * **Scrolling up unpins**, and the first step lands exactly `n` lines
//!   above the tail - never at the top of the session.
//! * **Unpinned is stationary**: the state is an absolute top offset into the
//!   wrapped content, so new content appending below never moves what the
//!   user is reading.
//! * **Only user actions re-pin**: a downward scroll that reaches/passes the
//!   tail, or an explicit [`Viewport::pin_bottom`] (prompt submit). Agent
//!   events - turn end, streaming - never call in here.
//! * **Clamped both ends**: the top offset stays in
//!   `[0, total_lines - height]`; when the content fits the viewport,
//!   scrolling is a no-op and the view starts at the top (offset 0).
//!
//! Geometry (`total_lines`, `height`) is passed in per call rather than
//! stored: the adapter measures wrapped lines at draw time and the numbers
//! change with every frame and resize.

/// Lines one mouse-wheel tick scrolls. Page keys scroll by [`page_step`]
/// instead (one viewport page).
pub const WHEEL_LINES: usize = 3;

/// The transcript viewport's scroll state. See the module doc for the
/// contract this encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    /// Absolute top-line offset into the wrapped content. Only meaningful
    /// while unpinned - pinned derives the offset from geometry, so the tail
    /// is followed without any per-frame mutation.
    top: usize,
    /// Whether the view follows the tail.
    pinned: bool,
}

impl Viewport {
    /// A fresh viewport, pinned to the tail.
    pub fn new() -> Self {
        Viewport {
            top: 0,
            pinned: true,
        }
    }

    /// The clamped top-line offset to draw at: pinned returns
    /// `total_lines - height` (the tail); unpinned returns the stored offset
    /// clamped into `[0, total_lines - height]`. This clamp is authoritative:
    /// the scroll methods may run against one-frame-stale geometry (see
    /// `ui.rs`), and this corrects any overshoot at draw time.
    pub fn top_offset(&self, total_lines: usize, height: usize) -> usize {
        let max_top = total_lines.saturating_sub(height);
        if self.pinned {
            max_top
        } else {
            self.top.min(max_top)
        }
    }

    /// Scrolls up `n` lines and unpins. From pinned, the offset is
    /// initialized from the current tail, so the first step lands exactly
    /// `n` lines above it. Clamps at the top; a no-op (still pinned) when the
    /// content already fits.
    pub fn scroll_up(&mut self, n: usize, total_lines: usize, height: usize) {
        if total_lines <= height {
            return;
        }
        self.top = self.top_offset(total_lines, height).saturating_sub(n);
        self.pinned = false;
    }

    /// Scrolls down `n` lines; re-pins when the step reaches or passes the
    /// tail. A no-op while pinned (already at the tail).
    pub fn scroll_down(&mut self, n: usize, total_lines: usize, height: usize) {
        if self.pinned {
            return;
        }
        let max_top = total_lines.saturating_sub(height);
        let next = self.top_offset(total_lines, height).saturating_add(n);
        if next >= max_top {
            self.pin_bottom();
        } else {
            self.top = next;
        }
    }

    /// Scrolls up one page (the viewport height minus one line of overlap).
    pub fn page_up(&mut self, total_lines: usize, height: usize) {
        self.scroll_up(page_step(height), total_lines, height);
    }

    /// Scrolls down one page (the viewport height minus one line of overlap).
    pub fn page_down(&mut self, total_lines: usize, height: usize) {
        self.scroll_down(page_step(height), total_lines, height);
    }

    /// Pins to the tail ([`Effect::PinBottom`](crate::ui::screen::Effect)
    /// - prompt submit and other explicit user actions only).
    pub fn pin_bottom(&mut self) {
        self.pinned = true;
        self.top = 0;
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport::new()
    }
}

/// One page is the height minus one line of overlap (the overlapping line
/// keeps reading continuity), never zero so paging always moves.
fn page_step(height: usize) -> usize {
    height.saturating_sub(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_viewport_is_pinned_to_the_tail() {
        let v = Viewport::new();
        assert!(v.pinned);
        assert_eq!(v.top_offset(100, 20), 80);
    }

    #[test]
    fn pinned_follows_the_tail_as_content_grows() {
        let v = Viewport::new();
        assert_eq!(v.top_offset(100, 20), 80);
        assert_eq!(v.top_offset(150, 20), 130);
        assert_eq!(v.top_offset(151, 20), 131);
    }

    #[test]
    fn content_that_fits_starts_at_the_top() {
        let v = Viewport::new();
        assert_eq!(v.top_offset(5, 20), 0);
        assert_eq!(v.top_offset(20, 20), 0);
        assert_eq!(v.top_offset(0, 20), 0);
    }

    #[test]
    fn scroll_up_from_pinned_moves_exactly_n_lines_above_the_tail() {
        let mut v = Viewport::new();
        v.scroll_up(3, 100, 20); // tail top is 80
        assert!(!v.pinned);
        assert_eq!(v.top_offset(100, 20), 77);
    }

    #[test]
    fn an_unpinned_view_is_stationary_as_content_grows() {
        let mut v = Viewport::new();
        v.scroll_up(3, 100, 20);
        let reading = v.top_offset(100, 20);
        assert_eq!(v.top_offset(500, 20), reading);
        assert_eq!(v.top_offset(10_000, 20), reading);
    }

    #[test]
    fn scroll_up_clamps_at_the_top() {
        let mut v = Viewport::new();
        v.scroll_up(1_000, 100, 20);
        assert_eq!(v.top_offset(100, 20), 0);
        v.scroll_up(3, 100, 20);
        assert_eq!(v.top_offset(100, 20), 0);
    }

    #[test]
    fn scroll_down_back_to_the_tail_repins() {
        let mut v = Viewport::new();
        v.scroll_up(3, 100, 20); // top 77
        v.scroll_down(3, 100, 20); // exactly the tail (80)
        assert!(v.pinned);
        // Pinned again: the tail is followed as content grows.
        assert_eq!(v.top_offset(200, 20), 180);
    }

    #[test]
    fn scroll_down_past_the_tail_repins() {
        let mut v = Viewport::new();
        v.scroll_up(2, 100, 20); // top 78
        v.scroll_down(50, 100, 20);
        assert!(v.pinned);
        assert_eq!(v.top_offset(100, 20), 80);
    }

    #[test]
    fn scroll_down_short_of_the_tail_stays_unpinned() {
        let mut v = Viewport::new();
        v.scroll_up(10, 100, 20); // top 70
        v.scroll_down(3, 100, 20); // top 73, tail is 80
        assert!(!v.pinned);
        assert_eq!(v.top_offset(100, 20), 73);
    }

    #[test]
    fn scroll_down_while_pinned_is_a_noop() {
        let mut v = Viewport::new();
        v.scroll_down(3, 100, 20);
        assert!(v.pinned);
        assert_eq!(v.top_offset(100, 20), 80);
    }

    #[test]
    fn scrolling_is_a_noop_when_the_content_fits() {
        let mut v = Viewport::new();
        v.scroll_up(3, 10, 20);
        assert!(v.pinned); // still following: streaming past a page re-anchors
        assert_eq!(v.top_offset(10, 20), 0);
        v.scroll_down(3, 10, 20);
        assert!(v.pinned);
        assert_eq!(v.top_offset(10, 20), 0);
    }

    #[test]
    fn page_steps_move_a_viewport_minus_one_line_of_overlap() {
        let mut v = Viewport::new();
        v.page_up(100, 20); // 80 - 19 = 61
        assert_eq!(v.top_offset(100, 20), 61);
        v.page_up(100, 20);
        assert_eq!(v.top_offset(100, 20), 42);
        v.page_down(100, 20);
        assert_eq!(v.top_offset(100, 20), 61);
        assert!(!v.pinned);
        v.page_down(100, 20); // back at the tail
        assert!(v.pinned);
    }

    #[test]
    fn a_wheel_tick_is_smaller_than_a_page() {
        // Guard the contract's step sizes: wheel = 3 lines, page = height - 1
        // (and paging always moves, even in a degenerate viewport).
        assert_eq!(WHEEL_LINES, 3);
        assert_eq!(page_step(20), 19);
        assert_eq!(page_step(1), 1);
        assert_eq!(page_step(0), 1);
    }

    #[test]
    fn pin_bottom_repins_after_scrolling_away() {
        let mut v = Viewport::new();
        v.scroll_up(10, 100, 20);
        assert!(!v.pinned);
        v.pin_bottom();
        assert!(v.pinned);
        assert_eq!(v.top_offset(100, 20), 80);
        assert_eq!(v.top_offset(300, 20), 280);
    }

    #[test]
    fn top_offset_reclamps_a_stale_offset_at_draw_time() {
        // Scroll effects run against last frame's geometry; the draw-time
        // clamp is authoritative when the fresh measure disagrees.
        let mut v = Viewport::new();
        v.scroll_up(3, 1_000, 20); // top 977 against the stale measure
        assert_eq!(v.top_offset(100, 20), 80);
    }

    #[test]
    fn scroll_up_from_a_stale_offset_moves_from_the_clamped_position() {
        let mut v = Viewport::new();
        v.scroll_up(3, 1_000, 20); // top 977 against the stale measure
        v.scroll_up(3, 100, 20); // fresh measure: clamp to 80, then up 3
        assert_eq!(v.top_offset(100, 20), 77);
    }
}
