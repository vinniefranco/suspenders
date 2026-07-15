//! The prompt-history ring - the ONE owner of Readline-style recall through
//! previously submitted prompts (CONTEXT.md: the Composer remembers what you
//! sent, so Up/Down walk back and forth through it).
//!
//! The ring holds submitted prompts oldest-first, an optional cursor `idx` into
//! them (`None` means "parked in the live composer draft, not recalling"), and
//! a stashed `draft` - the composer text as it stood the moment recall began.
//!
//! Navigation is EDGE-TRIGGERED and stateful in exactly one way: the FIRST
//! [`up`](History::up) stashes the caller's live draft, and the [`down`] that
//! walks PAST the newest entry restores it, resetting `idx` to `None`. Between
//! those ends, up/down just step the cursor. Recording a new submission resets
//! the cursor and clears the stash - the next Up starts fresh from the newest.
//!
//! No terminal, no async, no IO (ADR-0019): the caller passes its live draft
//! text in and gets back the text to place in the composer, or `None` for a
//! no-op (empty ring, or already at an end). The History struct mints no
//! effects; the Transcript owns those.

/// The in-memory prompt-history ring cap.
const MAX_HISTORY: usize = 100;

/// The prompt-history ring: submitted prompts (oldest first), the recall cursor
/// (`None` when parked in the live draft), and the draft stashed on first Up.
#[derive(Debug, Clone)]
pub struct History {
    entries: Vec<String>,
    idx: Option<usize>,
    draft: String,
}

impl History {
    /// A ring seeded with `entries` (oldest first), parked in the live draft:
    /// `idx` `None`, no stash.
    pub fn new(entries: Vec<String>) -> Self {
        History {
            entries,
            idx: None,
            draft: String::new(),
        }
    }

    /// Record a successfully submitted prompt: reset the cursor and clear the
    /// stash, then append - deduplicating a repeat of the newest entry and
    /// capping the ring at [`MAX_HISTORY`] (dropping the oldest).
    pub fn record(&mut self, prompt: &str) {
        self.idx = None;
        self.draft = String::new();
        if self.entries.last().map(|s| s.as_str()) == Some(prompt) {
            return;
        }
        self.entries.push(prompt.to_string());
        if self.entries.len() > MAX_HISTORY {
            let drop = self.entries.len() - MAX_HISTORY;
            self.entries.drain(0..drop);
        }
    }

    /// Navigate backward (older). `current` is the live composer draft, stashed
    /// on the first Up so a later Down can restore it. Returns the recalled
    /// entry's text, or `None` for a no-op: an empty ring, or already at the
    /// oldest entry.
    pub fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.idx {
            None => {
                let idx = self.entries.len() - 1;
                self.idx = Some(idx);
                self.draft = current.to_string();
                Some(self.entries[idx].clone())
            }
            Some(0) => None,
            Some(idx) => {
                let new_idx = idx - 1;
                self.idx = Some(new_idx);
                Some(self.entries[new_idx].clone())
            }
        }
    }

    /// Navigate forward (newer). Past the newest entry, restore the stashed
    /// draft and park (`idx` back to `None`). Returns the text to place in the
    /// composer, or `None` for a no-op: an empty ring, or not currently
    /// recalling (already parked in the draft).
    pub fn down(&mut self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.idx?;
        let last_idx = self.entries.len() - 1;
        if idx >= last_idx {
            self.idx = None;
            Some(std::mem::take(&mut self.draft))
        } else {
            let new_idx = idx + 1;
            self.idx = Some(new_idx);
            Some(self.entries[new_idx].clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_on_empty_ring_is_a_no_op() {
        let mut h = History::new(vec![]);
        assert_eq!(h.up("draft"), None);
    }

    #[test]
    fn down_on_empty_ring_is_a_no_op() {
        let mut h = History::new(vec![]);
        assert_eq!(h.down(), None);
    }

    #[test]
    fn down_before_any_up_is_a_no_op() {
        // Parked in the draft (idx None): Down has nowhere newer to go.
        let mut h = History::new(vec!["a".into(), "b".into()]);
        assert_eq!(h.down(), None);
    }

    #[test]
    fn up_walks_backward_from_the_newest() {
        let mut h = History::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(h.up("live"), Some("c".to_string()));
        assert_eq!(h.up("live"), Some("b".to_string()));
        assert_eq!(h.up("live"), Some("a".to_string()));
    }

    #[test]
    fn up_at_the_oldest_is_a_no_op() {
        let mut h = History::new(vec!["a".into(), "b".into()]);
        assert_eq!(h.up("live"), Some("b".to_string()));
        assert_eq!(h.up("live"), Some("a".to_string()));
        // Already at the oldest - further Up does nothing.
        assert_eq!(h.up("live"), None);
    }

    #[test]
    fn first_up_stashes_the_live_draft() {
        let mut h = History::new(vec!["a".into(), "b".into()]);
        assert_eq!(h.up("typing..."), Some("b".to_string()));
        // Walk back to the newest end and one Down past it restores the stash.
        assert_eq!(h.down(), Some("typing...".to_string()));
    }

    #[test]
    fn down_past_the_newest_restores_the_draft_and_parks() {
        let mut h = History::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(h.up("draft"), Some("c".to_string()));
        assert_eq!(h.up("draft"), Some("b".to_string()));
        assert_eq!(h.down(), Some("c".to_string()));
        // Past the newest: the stashed draft returns and we're parked again.
        assert_eq!(h.down(), Some("draft".to_string()));
        // Parked - a further Down is a no-op.
        assert_eq!(h.down(), None);
    }

    #[test]
    fn only_the_first_up_stashes_a_draft() {
        // A second Up must NOT overwrite the stash with the recalled text.
        let mut h = History::new(vec!["a".into(), "b".into()]);
        assert_eq!(h.up("original"), Some("b".to_string()));
        assert_eq!(h.up("ignored"), Some("a".to_string()));
        assert_eq!(h.down(), Some("b".to_string()));
        assert_eq!(h.down(), Some("original".to_string()));
    }

    #[test]
    fn record_dedups_a_repeat_of_the_newest() {
        let mut h = History::new(vec!["a".into()]);
        h.record("b");
        h.record("b");
        h.record("c");
        // Fold the ring out through navigation: c, b, a.
        assert_eq!(h.up(""), Some("c".to_string()));
        assert_eq!(h.up(""), Some("b".to_string()));
        assert_eq!(h.up(""), Some("a".to_string()));
        assert_eq!(h.up(""), None);
    }

    #[test]
    fn record_caps_the_ring_at_max_history() {
        let entries: Vec<String> = (1..=MAX_HISTORY).map(|n| format!("prompt {n}")).collect();
        let mut h = History::new(entries);
        h.record(&format!("prompt {}", MAX_HISTORY + 1));
        // Newest is the just-recorded entry; the oldest ("prompt 1") was dropped.
        assert_eq!(h.up(""), Some(format!("prompt {}", MAX_HISTORY + 1)));
        for n in (2..=MAX_HISTORY).rev() {
            assert_eq!(h.up(""), Some(format!("prompt {n}")));
        }
        // "prompt 1" is gone - the ring stayed capped at MAX_HISTORY.
        assert_eq!(h.up(""), None);
    }

    #[test]
    fn record_resets_the_cursor_and_clears_the_stash() {
        let mut h = History::new(vec!["a".into()]);
        h.up("stashed"); // park mid-history with a stash
        h.record("b");
        // After record: parked (Down is a no-op) and the next Up starts fresh
        // from the newest, stashing the new live draft.
        assert_eq!(h.down(), None);
        assert_eq!(h.up("fresh"), Some("b".to_string()));
        assert_eq!(h.down(), Some("fresh".to_string()));
    }
}
