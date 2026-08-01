//! Session Picker - the PURE selection core behind bare `--resume` (ADR-0001's
//! TEA shape, ADR-0019: no ratatui/crossterm here). The adapter
//! ([`crate::ui::pick_session`]) owns the terminal, maps crossterm input to the
//! shared [`Key`] vocabulary, and renders; every selection rule lives here,
//! where it is tested.
//!
//! The model is a cursor over [`SessionEntry`] rows (newest first, as
//! [`crate::session::log::list`] orders them). A key press either moves the
//! cursor (`None`) or resolves the picker with a [`PickerOutcome`].

use crate::session::log::SessionEntry;
use crate::ui::screen::Key;

/// The picker's whole state: the rows and the highlighted index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub entries: Vec<SessionEntry>,
    pub cursor: usize,
}

/// How the picker resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Resume from this Session Log path.
    Resume(String),
    /// Skip resuming; start a clean Session (Escape).
    FreshSession,
    /// Leave without starting anything (q / Ctrl-C).
    Quit,
}

impl Picker {
    /// A picker over `entries`, cursor on the first (newest) row.
    pub fn new(entries: Vec<SessionEntry>) -> Self {
        Picker { entries, cursor: 0 }
    }

    /// Folds one key press: `None` keeps picking (cursor may have moved),
    /// `Some` resolves. Arrows and the wheel move the cursor, saturating at
    /// both ends; Enter resumes the selected row (a no-op on an empty list);
    /// Escape starts fresh; `q` quits. Every other key is ignored.
    pub fn handle_key(&mut self, key: Key) -> Option<PickerOutcome> {
        match key {
            Key::ArrowUp | Key::WheelUp => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            Key::ArrowDown | Key::WheelDown => {
                if self.cursor + 1 < self.entries.len() {
                    self.cursor += 1;
                }
                None
            }
            Key::Enter => self
                .entries
                .get(self.cursor)
                .map(|entry| PickerOutcome::Resume(entry.path.clone())),
            Key::Escape => Some(PickerOutcome::FreshSession),
            Key::Char('q') => Some(PickerOutcome::Quit),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/ui/picker.rs"]
mod tests;
