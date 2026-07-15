//! In-flight streaming state - the ONE owner of the assistant message being
//! streamed (CONTEXT.md: the snapshot the Transcript shows mid-Turn, before it
//! settles into discrete items).
//!
//! Streaming is STATELESS (ADR-0001, and see `ui::transcript`): each
//! [`Event::MessageUpdate`](crate::event::Event) carries the accumulated
//! snapshot, so [`Streaming::update`] replaces the in-flight view wholesale -
//! no delta accumulation. Two moments MATERIALIZE the snapshot into discrete
//! [`TranscriptItem`]s, and they differ in ONE way:
//!
//! * [`Streaming::end`] (a clean [`Event::MessageEnd`](crate::event::Event))
//!   takes Thinking from the LAST SNAPSHOT - the final `content` the event
//!   carries never repeats thinking - and text from that final content.
//! * [`Streaming::flush`] (a cancel or crash mid-stream) has no final content,
//!   so it takes BOTH thinking and text from the last snapshot.
//!
//! Either way, Thinking materializes first, then the assistant text, and
//! empties are skipped. Materializing clears the snapshot to idle.
//!
//! No terminal, no async, no IO (ADR-0019): [`ContentBlock`]s in, plain
//! [`String`]s and [`TranscriptItem`]s out.

use crate::content::ContentBlock;
use crate::ui::transcript::TranscriptItem;

/// The latest streaming snapshot (`None` when not streaming). Owns the
/// stateless-streaming rules: replace-wholesale, and the two materialize paths.
pub struct Streaming(Option<Vec<ContentBlock>>);

impl Streaming {
    /// Not streaming.
    pub fn idle() -> Self {
        Streaming(None)
    }

    /// A message began: an empty snapshot, ready for the first update.
    pub fn start(&mut self) {
        self.0 = Some(Vec::new());
    }

    /// Stateless streaming: the snapshot replaces the in-flight view wholesale.
    pub fn update(&mut self, content: Vec<ContentBlock>) {
        self.0 = Some(content);
    }

    /// Back to not streaming, discarding any snapshot (Turn boundary reset).
    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// The in-flight assistant text, from the latest snapshot.
    pub fn text(&self) -> String {
        blocks_text(self.snapshot(), BlockKind::Text)
    }

    /// The in-flight Thinking text, from the latest snapshot.
    pub fn thinking(&self) -> String {
        blocks_text(self.snapshot(), BlockKind::Thinking)
    }

    /// Materialize a finished message (a clean [`Event::MessageEnd`]): Thinking
    /// from the last snapshot - the final content never carries it - then the
    /// assistant text from that final `content`. Empties the snapshot.
    ///
    /// [`Event::MessageEnd`]: crate::event::Event
    pub fn end(&mut self, final_content: &[ContentBlock]) -> Vec<TranscriptItem> {
        let thinking = blocks_text(self.snapshot(), BlockKind::Thinking);
        let text = blocks_text(final_content, BlockKind::Text);
        self.0 = None;
        materialize(thinking, text)
    }

    /// Materialize a live snapshot after a cancel/crash mid-stream: BOTH
    /// Thinking and text come from the last snapshot (there is no final
    /// content). Empties the snapshot; a no-op when already idle.
    pub fn flush(&mut self) -> Vec<TranscriptItem> {
        let snapshot = match self.0.take() {
            None => return Vec::new(),
            Some(blocks) => blocks,
        };
        let thinking = blocks_text(&snapshot, BlockKind::Thinking);
        let text = blocks_text(&snapshot, BlockKind::Text);
        materialize(thinking, text)
    }

    fn snapshot(&self) -> &[ContentBlock] {
        self.0.as_deref().unwrap_or(&[])
    }
}

// Thinking item first (if any), then the assistant item (if any) - the order
// the Transcript pushes them in. Empties are skipped.
fn materialize(thinking: String, text: String) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !thinking.is_empty() {
        items.push(TranscriptItem::Thinking { text: thinking });
    }
    if !text.is_empty() {
        items.push(TranscriptItem::Assistant { text });
    }
    items
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

fn blocks_text(blocks: &[ContentBlock], kind: BlockKind) -> String {
    let mut out = String::new();
    for block in blocks {
        match (kind, block) {
            (BlockKind::Text, ContentBlock::Text { text }) => out.push_str(text),
            (BlockKind::Thinking, ContentBlock::Thinking { text }) => out.push_str(text),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }
    fn thinking_block(text: &str) -> ContentBlock {
        ContentBlock::Thinking { text: text.into() }
    }
    fn thinking(text: &str) -> TranscriptItem {
        TranscriptItem::Thinking { text: text.into() }
    }
    fn assistant(text: &str) -> TranscriptItem {
        TranscriptItem::Assistant { text: text.into() }
    }

    #[test]
    fn idle_reads_empty_and_flushes_to_nothing() {
        let mut s = Streaming::idle();
        assert_eq!(s.text(), "");
        assert_eq!(s.thinking(), "");
        assert_eq!(s.flush(), vec![]);
    }

    #[test]
    fn start_then_update_replaces_wholesale() {
        let mut s = Streaming::idle();
        s.start();
        s.update(vec![text_block("Hel")]);
        assert_eq!(s.text(), "Hel");
        // The second update REPLACES the first - no accumulation.
        s.update(vec![thinking_block("hm"), text_block("Hello")]);
        assert_eq!(s.text(), "Hello");
        assert_eq!(s.thinking(), "hm");
    }

    #[test]
    fn end_takes_thinking_from_snapshot_text_from_final_content() {
        let mut s = Streaming::idle();
        s.start();
        // Snapshot carries both; the final content carries only text (thinking
        // is never repeated in it).
        s.update(vec![thinking_block("hmm"), text_block("reading")]);
        let items = s.end(&[text_block("reading")]);
        assert_eq!(items, vec![thinking("hmm"), assistant("reading")]);
        // Snapshot emptied.
        assert_eq!(s.text(), "");
        assert_eq!(s.thinking(), "");
    }

    #[test]
    fn end_with_no_thinking_in_snapshot_yields_only_assistant() {
        let mut s = Streaming::idle();
        s.start();
        let items = s.end(&[text_block("no thinking here")]);
        assert_eq!(items, vec![assistant("no thinking here")]);
    }

    #[test]
    fn end_with_empty_final_text_skips_the_assistant_item() {
        let mut s = Streaming::idle();
        s.start();
        s.update(vec![thinking_block("hmm")]);
        let items = s.end(&[]);
        assert_eq!(items, vec![thinking("hmm")]);
    }

    #[test]
    fn flush_takes_both_thinking_and_text_from_snapshot() {
        let mut s = Streaming::idle();
        s.start();
        s.update(vec![thinking_block("mid"), text_block("partial")]);
        let items = s.flush();
        assert_eq!(items, vec![thinking("mid"), assistant("partial")]);
        // Emptied - a second flush yields nothing.
        assert_eq!(s.flush(), vec![]);
    }

    #[test]
    fn clear_discards_the_snapshot() {
        let mut s = Streaming::idle();
        s.start();
        s.update(vec![text_block("stale")]);
        s.clear();
        assert_eq!(s.text(), "");
        assert_eq!(s.flush(), vec![]);
    }
}
