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
///
/// A per-row `disabled` mask (qwen `useSelectionList.ts` `findNextValidIndex`)
/// lets the Phase-5 model/theme dialogs carry unnavigable rows (Provider
/// headers, greyed collapsed rows, broken themes): nav skips them in both
/// directions and the initial active row snaps off any disabled row
/// ([`SelectionList::with_active`]). The approval radio's [`SelectionList::new`]
/// leaves the mask all-`false`, so its behaviour is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionList {
    active: usize,
    len: usize,
    /// Per-row nav mask (qwen `SelectionListItem.disabled`): `disabled[i]` is
    /// `true` for a row Up/Down skips and the initial active row snaps off.
    /// Always `len` long. All-`false` for the approval radio.
    disabled: Vec<bool>,
    /// The digits typed so far toward a quick-select (qwen `numberInputRef`),
    /// empty when idle.
    number_buffer: String,
    /// When the buffered digits auto-select, in host `Millis`. `None` when the
    /// buffer is empty. Set by a buffering digit, consulted by [`Self::expire`].
    deadline: Option<Millis>,
}

impl SelectionList {
    /// A fresh list of `len` rows with row 0 active, no row disabled, and no
    /// digit buffered - the approval radio's shape, unchanged.
    pub fn new(len: usize) -> Self {
        SelectionList {
            active: 0,
            len,
            disabled: vec![false; len],
            number_buffer: String::new(),
            deadline: None,
        }
    }

    /// A list over a `disabled` mask (one bool per row) with the active row
    /// snapped onto the first navigable row at or after `initial` (qwen
    /// `computeInitialIndex`): an out-of-range `initial` clamps to 0, and a
    /// disabled target walks DOWN to the next navigable row, wrapping if
    /// needed. An all-disabled list keeps `initial` (clamped) - nothing is
    /// navigable, and Enter still guards on the disabled mask.
    pub fn with_active(disabled: Vec<bool>, initial: usize) -> Self {
        let len = disabled.len();
        let clamped = if initial >= len { 0 } else { initial };
        let active = if len == 0 || !disabled[clamped] {
            clamped
        } else {
            find_next_valid(clamped, Direction::Down, &disabled).unwrap_or(clamped)
        };
        SelectionList {
            active,
            len,
            disabled,
            number_buffer: String::new(),
            deadline: None,
        }
    }

    /// Whether row `i` is disabled (skipped by nav; Enter refuses it).
    pub fn is_disabled(&self, i: usize) -> bool {
        self.disabled.get(i).copied().unwrap_or(false)
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
            // Enter refuses a disabled active row (qwen guards the pendingSelect
            // on `!currentItem.disabled`); with an all-`false` mask (the
            // approval radio) it always selects, unchanged.
            SelectionKey::Enter if self.is_disabled(self.active) => SelectionOutcome::Ignored,
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
        // Walk to the next NAVIGABLE row in `dir`, skipping disabled rows and
        // wrapping (qwen `findNextValidIndex`). An all-disabled list, or a
        // one-navigable-row list where the step returns to itself, does not
        // move. With an all-`false` mask this is the plain wrap.
        match find_next_valid(self.active, dir, &self.disabled) {
            Some(next) if next != self.active => {
                self.active = next;
                SelectionOutcome::Moved
            }
            _ => SelectionOutcome::Ignored,
        }
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

        if target >= self.len || self.is_disabled(target) {
            // Out of bounds, or a disabled row the digit cannot land on
            // (a header/greyed/broken row): drop the buffer, nothing moves.
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

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

/// The next NAVIGABLE row from `current` in `direction`, wrapping, skipping
/// disabled rows (qwen `useSelectionList.ts` `findNextValidIndex`): scans at
/// most `len` steps and returns the first non-disabled index it lands on, or
/// `None` when every row is disabled (or the list is empty). May return
/// `current` itself when it is the only navigable row - the caller treats a
/// no-move return as "nothing happened".
fn find_next_valid(current: usize, direction: Direction, disabled: &[bool]) -> Option<usize> {
    let len = disabled.len();
    if len == 0 {
        return None;
    }
    let mut index = current;
    for _ in 0..len {
        index = match direction {
            Direction::Down => (index + 1) % len,
            Direction::Up => (index + len - 1) % len,
        };
        if !disabled[index] {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
#[path = "../../tests/unit/ui/selection.rs"]
mod tests;
