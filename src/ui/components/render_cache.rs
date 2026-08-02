use ratatui::text::Line;

use super::{Toggles, markdown_lines, message_lines, wrapped_count};
use crate::ui::theme::{self, Theme};
use crate::ui::transcript::Transcript;

/// Per-item render state for the fullscreen transcript body (ADR-0046), owned
/// by the adapter's run loop and threaded through [`super::render_pending`].
/// Holds ratatui [`Line`]s, so it lives HERE, not in the pure modules
/// (ADR-0019).
pub struct RenderCache {
    /// The text width everything below was built/measured at.
    width: u16,
    /// The [`Toggles`] the settled lines were built with (either flip
    /// changes every affected item's lines, so it clears the cache
    /// wholesale).
    toggles: Toggles,
    /// The [`Theme`] every cached line was colored with. Cached lines
    /// BAKE their colors (styled spans, syntect-highlighted code), so a
    /// theme swap (Stage C's live preview) stales them all: any
    /// difference clears the cache wholesale, exactly like a resize.
    theme: Theme,
    /// The store's [`Transcript::revision`] the entries were built at:
    /// while it holds still, the settled items only extend (the store's
    /// prefix contract) and the cache extends with them; when it moves (a
    /// structural edit), the cache rebuilds from scratch.
    revision: u64,
    /// One entry per settled [`Transcript::items`] item, same order.
    items: Vec<CachedItem>,
    /// The in-flight streaming markdown, keyed on its char length: within
    /// one message the snapshot only grows, so the length is a cheap
    /// monotonic key that changes exactly when the text does. Cleared
    /// between messages (empty streaming text) so a new message can never
    /// collide with a stale entry of the same length.
    streaming: Option<CachedStreaming>,
}

/// One settled item's built lines and its wrapped row count at the
/// cache's width - the numbers the pending body does its
/// prefix-sum math over.
struct CachedItem {
    lines: Vec<Line<'static>>,
    wrapped: usize,
}

/// The cached streaming-markdown tail (see [`RenderCache::streaming`]).
struct CachedStreaming {
    char_len: usize,
    lines: Vec<Line<'static>>,
    wrapped: usize,
}

impl RenderCache {
    pub fn new() -> Self {
        RenderCache {
            width: 0,
            toggles: Toggles::default(),
            theme: theme::dark().clone(),
            revision: 0,
            items: Vec::new(),
            streaming: None,
        }
    }

    /// The settled entries in [`Transcript::items`] order: each item's
    /// built lines with its wrapped row count at the cache's width.
    pub(super) fn settled(&self) -> impl Iterator<Item = (&[Line<'static>], usize)> {
        self.items
            .iter()
            .map(|item| (item.lines.as_slice(), item.wrapped))
    }

    /// The streaming-markdown tail, if a snapshot is in flight: its lines
    /// with their wrapped row count. Always after every settled entry.
    pub(super) fn streaming_tail(&self) -> Option<(&[Line<'static>], usize)> {
        self.streaming
            .as_ref()
            .map(|s| (s.lines.as_slice(), s.wrapped))
    }

    /// Brings the cache up to date with the Transcript at `width`: clears
    /// wholesale when [`Self::needs_rebuild`] says a key input changed,
    /// then builds entries for the newly appended items only - the
    /// steady-state cost of a frame is zero rebuilt items.
    pub(super) fn sync(&mut self, t: &Transcript, toggles: Toggles, width: u16, theme: &Theme) {
        if self.needs_rebuild(t, toggles, width, theme) {
            self.items.clear();
            self.streaming = None;
            self.width = width;
            self.toggles = toggles;
            self.theme = theme.clone();
            self.revision = t.revision();
        }
        for item in &t.items()[self.items.len()..] {
            let lines = message_lines(item, toggles.compact, width, theme);
            // Per-item separators are added at assembly (`grouped_rows`
            // interleaves a blank `separator_row`, qwen `marginTop:1`), not
            // baked into each cached item - so the cache holds only the
            // item's own body lines (ADR-0046).
            let wrapped = wrapped_count(lines.clone(), width);
            self.items.push(CachedItem { lines, wrapped });
        }
        self.sync_streaming(&t.streaming_text(), width, theme);
    }

    /// Whether [`Self::sync`] must clear wholesale instead of extending.
    /// The extend-only fast path is safe because the store guarantees the
    /// settled items are a strict PREFIX of the last read while the
    /// revision holds still (appends never bump, structural edits always
    /// do - see `ui/transcript`); a width or [`Toggles`] change restyles
    /// every settled line, so either clears too. The length check is
    /// cheap defense in kind: a store shorter than the cache (a swapped
    /// Transcript whose revision happens to coincide) cannot extend it.
    fn needs_rebuild(&self, t: &Transcript, toggles: Toggles, width: u16, theme: &Theme) -> bool {
        self.width != width
            || self.toggles != toggles
            || self.theme != *theme
            || self.revision != t.revision()
            || self.items.len() > t.items().len()
    }

    /// Re-parses the streaming markdown only when its char length moved
    /// (monotonic within a message - see the field doc); drops the entry
    /// when streaming ended so the next message starts from nothing.
    fn sync_streaming(&mut self, text: &str, width: u16, theme: &Theme) {
        if text.is_empty() {
            self.streaming = None;
            return;
        }
        let char_len = text.chars().count();
        if self
            .streaming
            .as_ref()
            .is_some_and(|s| s.char_len == char_len)
        {
            return;
        }
        let lines = markdown_lines(text, theme);
        let wrapped = wrapped_count(lines.clone(), width);
        self.streaming = Some(CachedStreaming {
            char_len,
            lines,
            wrapped,
        });
    }
}

impl Default for RenderCache {
    fn default() -> Self {
        RenderCache::new()
    }
}

// The extend-vs-rebuild invariant, pinned at the cache's own seam. These
// sync against a bare Transcript store (ADR-0034) seeded through its
// verbs, and they live INSIDE the module because proving "not rebuilt"
// takes a sentinel planted in the private entries - identity, not
// equality. Accessor-expressible cache tests stay in the outer module.

#[cfg(test)]
#[path = "../../../tests/ui/components/render_cache.rs"]
mod tests;
