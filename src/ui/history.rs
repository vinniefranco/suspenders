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
//! effects; the Composer owns those - its `submitted_ok` pairs this ring's
//! record with the on-disk `HistoryAppend`.

/// The in-memory prompt-history ring cap.
const MAX_HISTORY: usize = 100;

/// The prompt-history ring: submitted prompts (oldest first), the recall cursor
/// (`None` when parked in the live draft), and the draft stashed on first Up.
/// `PartialEq` serves the Composer's refusal contract: a refused key must
/// leave the whole Composer - this ring and its stash included - bit-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[path = "../../tests/ui/history.rs"]
mod tests;
