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
mod tests {
    use super::*;

    fn entry(n: usize) -> SessionEntry {
        SessionEntry {
            path: format!("/logs/{n}.jsonl"),
            stamp: format!("2026-07-1{n} 00:00"),
            label: format!("prompt {n}"),
        }
    }

    fn picker(n: usize) -> Picker {
        Picker::new((0..n).map(entry).collect())
    }

    #[test]
    fn new_starts_on_the_newest_row() {
        assert_eq!(picker(3).cursor, 0);
    }

    #[test]
    fn arrows_move_the_cursor_saturating_at_both_ends() {
        let mut p = picker(3);

        assert_eq!(p.handle_key(Key::ArrowUp), None);
        assert_eq!(p.cursor, 0, "up at the top stays");

        assert_eq!(p.handle_key(Key::ArrowDown), None);
        assert_eq!(p.cursor, 1);
        p.handle_key(Key::ArrowDown);
        assert_eq!(p.cursor, 2);
        p.handle_key(Key::ArrowDown);
        assert_eq!(p.cursor, 2, "down at the bottom stays");

        p.handle_key(Key::ArrowUp);
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn the_wheel_moves_the_cursor_like_the_arrows() {
        let mut p = picker(2);
        assert_eq!(p.handle_key(Key::WheelDown), None);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.handle_key(Key::WheelDown), None);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.handle_key(Key::WheelUp), None);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.handle_key(Key::WheelUp), None);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn enter_resolves_to_the_selected_entrys_path() {
        let mut p = picker(3);
        p.handle_key(Key::ArrowDown);
        assert_eq!(
            p.handle_key(Key::Enter),
            Some(PickerOutcome::Resume("/logs/1.jsonl".into()))
        );
    }

    #[test]
    fn escape_resolves_to_a_fresh_session() {
        assert_eq!(
            picker(3).handle_key(Key::Escape),
            Some(PickerOutcome::FreshSession)
        );
    }

    #[test]
    fn q_quits() {
        assert_eq!(
            picker(3).handle_key(Key::Char('q')),
            Some(PickerOutcome::Quit)
        );
    }

    #[test]
    fn other_keys_are_ignored() {
        let mut p = picker(2);
        assert_eq!(p.handle_key(Key::Char('x')), None);
        assert_eq!(p.handle_key(Key::PageUp), None);
        assert_eq!(p.handle_key(Key::Backspace), None);
        assert_eq!(p.handle_key(Key::Other), None);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn an_empty_list_never_resumes_but_still_leaves() {
        let mut p = picker(0);
        assert_eq!(p.handle_key(Key::ArrowDown), None);
        assert_eq!(p.handle_key(Key::ArrowUp), None);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.handle_key(Key::Enter), None, "nothing to resume");
        assert_eq!(p.handle_key(Key::Escape), Some(PickerOutcome::FreshSession));
        assert_eq!(p.handle_key(Key::Char('q')), Some(PickerOutcome::Quit));
    }
}
