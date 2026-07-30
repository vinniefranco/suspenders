//! SelectionList - the pure fold behind a numbered radio list (ADR-0019,
//! ADR-0049): the shared selection mechanic qwen's `useSelectionList.ts` drives
//! for the approval radio today and (Phase 5) the model/theme dialogs. No
//! ratatui, no crossterm, no timer thread - a host-driven `expire(now)` stands
//! in for qwen's `setTimeout` (the ui.rs tick calls it), so the digit
//! quick-select's `NUMBER_INPUT_TIMEOUT_MS` never crosses the pure seam.
//!
//! ## The mechanic (qwen `useSelectionList.ts`)
//! - Up/Down wrap over `[0, len)`.
//! - Enter selects the active row.
//! - Digit quick-select is 1-indexed (`1.` selects row 0). A digit that CANNOT
//!   be a prefix of another valid row number (`buffer + "0"` already past the
//!   end) selects immediately; otherwise it moves the active row and buffers,
//!   waiting for more input or the timeout ([`SelectionList::expire`]). A single
//!   `0` is invalid (1-indexed); an out-of-range number clears the buffer.
//! - Escape cancels.
//!
//! With `<= 4` rows the buffered case never arises (`buffer + "0" >= 10 > len`),
//! so the approval radio (3 rows) always selects on the first digit and the
//! timeout is a no-op - but the buffered path is implemented and tested so the
//! Phase-5 dialogs (10-12 rows) reuse it unchanged.

/// A monotonic host clock reading, in milliseconds (the ui.rs tick's own clock).
/// The pure fold stores a deadline in these units and compares against a later
/// reading in [`SelectionList::expire`] - it never reads a real clock itself.
pub type Millis = u64;

/// qwen `NUMBER_INPUT_TIMEOUT_MS`: how long a buffered digit waits for a
/// following digit before it auto-selects the buffered target.
pub const NUMBER_INPUT_TIMEOUT_MS: Millis = 1000;

/// The decimal radix: appending the smallest next digit (`0`) multiplies a
/// buffered number by this to test whether it could still prefix a longer valid
/// row number (qwen `parseInt(buffer + '0')`).
const DECIMAL_RADIX: usize = 10;

/// A key press reduced to what the selection fold acts on (ADR-0019). The
/// adapter (ui.rs) maps a real key to one of these; `Digit` carries the 0-9 the
/// quick-select reads, `Other` is any key the list ignores (so a non-numeric key
/// still clears a stale digit buffer, matching qwen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKey {
    Up,
    Down,
    Enter,
    Escape,
    /// A digit 0-9 (the caller guarantees the range).
    Digit(u8),
    /// Any other key - ignored, but it clears a pending digit buffer.
    Other,
}

/// What folding one key produced (the host acts on it): the active row moved, a
/// row was selected (its index), the list was cancelled, or nothing happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOutcome {
    /// The active row changed (or a digit buffered without selecting); redraw.
    Moved,
    /// A row was selected - its 0-based index.
    Selected(usize),
    /// The list was cancelled (Escape).
    Cancelled,
    /// The key did nothing (e.g. a digit out of range).
    Ignored,
}

/// The pure selection state: the active row, the row count, and the digit
/// quick-select buffer with its deadline. Cloneable/Eq so a `PendingApproval`
/// holding one stays comparable in Screen tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionList {
    active: usize,
    len: usize,
    /// The digits typed so far toward a quick-select (qwen `numberInputRef`),
    /// empty when idle.
    number_buffer: String,
    /// When the buffered digits auto-select, in host `Millis`. `None` when the
    /// buffer is empty. Set by a buffering digit, consulted by [`Self::expire`].
    deadline: Option<Millis>,
}

impl SelectionList {
    /// A fresh list of `len` rows with row 0 active and no digit buffered.
    pub fn new(len: usize) -> Self {
        SelectionList {
            active: 0,
            len,
            number_buffer: String::new(),
            deadline: None,
        }
    }

    /// The active row index.
    pub fn active(&self) -> usize {
        self.active
    }

    /// The row count.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list has no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Folds one key at host time `now` (ADR-0019). Up/Down wrap; Enter selects
    /// the active row; Escape cancels; a digit runs the 1-indexed quick-select
    /// (immediate when it cannot be a prefix, else buffered until [`Self::expire`]
    /// or another digit); any other key clears a stale buffer and is ignored.
    pub fn handle(&mut self, key: SelectionKey, now: Millis) -> SelectionOutcome {
        // A non-digit key clears a pending digit buffer (qwen's first branch).
        if !matches!(key, SelectionKey::Digit(_)) {
            self.clear_buffer();
        }
        match key {
            SelectionKey::Up => self.step(Direction::Up),
            SelectionKey::Down => self.step(Direction::Down),
            SelectionKey::Enter => SelectionOutcome::Selected(self.active),
            SelectionKey::Escape => SelectionOutcome::Cancelled,
            SelectionKey::Digit(d) => self.digit(d, now),
            SelectionKey::Other => SelectionOutcome::Ignored,
        }
    }

    /// The host tick's timeout hook (ADR-0049, no bg timer): if a digit is
    /// buffered and `now` has reached its deadline, auto-select the buffered
    /// target and clear the buffer; otherwise nothing. With `<= 4` rows this
    /// never fires (every digit selects immediately), but the Phase-5 dialogs
    /// rely on it.
    pub fn expire(&mut self, now: Millis) -> SelectionOutcome {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.clear_buffer();
                SelectionOutcome::Selected(self.active)
            }
            _ => SelectionOutcome::Ignored,
        }
    }

    // ---- internals ----

    fn step(&mut self, dir: Direction) -> SelectionOutcome {
        if self.len == 0 {
            return SelectionOutcome::Ignored;
        }
        self.active = match dir {
            Direction::Up => (self.active + self.len - 1) % self.len,
            Direction::Down => (self.active + 1) % self.len,
        };
        SelectionOutcome::Moved
    }

    // The 1-indexed digit quick-select (qwen `useSelectionList.ts:343-388`).
    fn digit(&mut self, d: u8, now: Millis) -> SelectionOutcome {
        let candidate = format!("{}{d}", self.number_buffer);

        // A single '0' is invalid (1-indexed): buffer it so a following digit
        // can still form a valid number, and arm the timeout to clear it.
        if candidate == "0" {
            self.number_buffer = candidate;
            self.deadline = Some(now + NUMBER_INPUT_TIMEOUT_MS);
            return SelectionOutcome::Ignored;
        }

        let Ok(number) = candidate.parse::<usize>() else {
            self.clear_buffer();
            return SelectionOutcome::Ignored;
        };
        let target = number - 1;

        if target >= self.len {
            // Out of bounds: drop the buffer, nothing moves.
            self.clear_buffer();
            return SelectionOutcome::Ignored;
        }

        self.active = target;

        // If appending the smallest next digit ('0') already overshoots the
        // list, this number cannot be a prefix of another valid row - select
        // now. Otherwise buffer and wait for another digit or the timeout.
        let smallest_extension = number * DECIMAL_RADIX;
        if smallest_extension > self.len {
            self.clear_buffer();
            SelectionOutcome::Selected(target)
        } else {
            self.number_buffer = candidate;
            self.deadline = Some(now + NUMBER_INPUT_TIMEOUT_MS);
            SelectionOutcome::Moved
        }
    }

    fn clear_buffer(&mut self) {
        self.number_buffer.clear();
        self.deadline = None;
    }
}

enum Direction {
    Up,
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Millis = 0;

    fn list(len: usize) -> SelectionList {
        SelectionList::new(len)
    }

    #[test]
    fn new_starts_at_row_zero() {
        let l = list(3);
        assert_eq!(l.active(), 0);
        assert_eq!(l.len(), 3);
        assert!(!l.is_empty());
    }

    #[test]
    fn down_advances_and_wraps() {
        let mut l = list(3);
        assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
        assert_eq!(l.active(), 1);
        l.handle(SelectionKey::Down, T0);
        assert_eq!(l.active(), 2);
        // Wrap from the last row back to the first.
        assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
        assert_eq!(l.active(), 0);
    }

    #[test]
    fn up_wraps_to_the_last_row() {
        let mut l = list(3);
        assert_eq!(l.handle(SelectionKey::Up, T0), SelectionOutcome::Moved);
        assert_eq!(l.active(), 2);
    }

    #[test]
    fn enter_selects_the_active_row() {
        let mut l = list(3);
        l.handle(SelectionKey::Down, T0);
        assert_eq!(
            l.handle(SelectionKey::Enter, T0),
            SelectionOutcome::Selected(1)
        );
    }

    #[test]
    fn escape_cancels() {
        let mut l = list(3);
        assert_eq!(
            l.handle(SelectionKey::Escape, T0),
            SelectionOutcome::Cancelled
        );
    }

    // With <= 9 rows every single digit already overshoots on the '0'
    // extension, so a digit selects immediately (the approval radio's case).
    #[test]
    fn a_digit_selects_immediately_when_it_cannot_be_a_prefix() {
        let mut l = list(3);
        assert_eq!(
            l.handle(SelectionKey::Digit(2), T0),
            SelectionOutcome::Selected(1)
        );
        assert_eq!(l.active(), 1);
    }

    #[test]
    fn digit_one_selects_the_first_row() {
        let mut l = list(3);
        assert_eq!(
            l.handle(SelectionKey::Digit(1), T0),
            SelectionOutcome::Selected(0)
        );
    }

    // In a long list a leading digit that COULD prefix a two-digit row buffers
    // (moves) and waits; a following digit resolves it.
    #[test]
    fn a_prefix_digit_buffers_then_a_second_digit_selects() {
        let mut l = list(12);
        // '1' could prefix 10/11/12, so it buffers and moves to row 0.
        assert_eq!(
            l.handle(SelectionKey::Digit(1), T0),
            SelectionOutcome::Moved
        );
        assert_eq!(l.active(), 0);
        // '2' → "12" → row 11, which cannot be extended: select now.
        assert_eq!(
            l.handle(SelectionKey::Digit(2), T0),
            SelectionOutcome::Selected(11)
        );
    }

    // A buffered digit auto-selects once the host tick reaches the deadline.
    #[test]
    fn a_buffered_digit_expires_into_a_selection() {
        let mut l = list(12);
        // '1' buffers (could prefix 10-12), active row 0.
        assert_eq!(
            l.handle(SelectionKey::Digit(1), T0),
            SelectionOutcome::Moved
        );
        // Before the deadline: nothing.
        assert_eq!(
            l.expire(NUMBER_INPUT_TIMEOUT_MS - 1),
            SelectionOutcome::Ignored
        );
        // At the deadline: the buffered target (row 0) selects.
        assert_eq!(
            l.expire(NUMBER_INPUT_TIMEOUT_MS),
            SelectionOutcome::Selected(0)
        );
        // Buffer cleared: a second expire does nothing.
        assert_eq!(
            l.expire(NUMBER_INPUT_TIMEOUT_MS + 10),
            SelectionOutcome::Ignored
        );
    }

    #[test]
    fn an_out_of_range_digit_is_ignored_and_clears_the_buffer() {
        let mut l = list(3);
        assert_eq!(
            l.handle(SelectionKey::Digit(9), T0),
            SelectionOutcome::Ignored
        );
        assert_eq!(l.active(), 0, "an out-of-range digit does not move");
    }

    #[test]
    fn a_single_zero_is_invalid_and_buffers_without_selecting() {
        let mut l = list(3);
        assert_eq!(
            l.handle(SelectionKey::Digit(0), T0),
            SelectionOutcome::Ignored
        );
        // A following '1' → "01" → 1 → row 0, selects (cannot extend past 3).
        assert_eq!(
            l.handle(SelectionKey::Digit(1), T0),
            SelectionOutcome::Selected(0)
        );
    }

    #[test]
    fn a_non_digit_key_clears_a_pending_digit_buffer() {
        let mut l = list(12);
        l.handle(SelectionKey::Digit(1), T0); // buffers
        // A non-digit key clears the buffer; the timeout no longer fires.
        assert_eq!(l.handle(SelectionKey::Down, T0), SelectionOutcome::Moved);
        assert_eq!(
            l.expire(NUMBER_INPUT_TIMEOUT_MS + 10),
            SelectionOutcome::Ignored
        );
    }

    #[test]
    fn expire_with_no_buffer_is_ignored() {
        let mut l = list(3);
        assert_eq!(l.expire(9_999), SelectionOutcome::Ignored);
    }
}
