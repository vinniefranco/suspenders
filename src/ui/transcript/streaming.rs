//! In-flight streaming state - the ONE owner of the assistant message being
//! streamed (CONTEXT.md: the snapshot the Transcript shows mid-Run, before it
//! settles into discrete items). A PRIVATE child of the Transcript store
//! (ADR-0034): only the store's verbs reach it.
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

use crate::view_model::TranscriptItem;

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

    /// Back to not streaming, discarding any snapshot (Run boundary reset).
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
    #[must_use = "the materialized items ARE the finished message - dropping them loses it (the snapshot is already emptied)"]
    pub fn end(&mut self, final_content: &[ContentBlock]) -> Vec<TranscriptItem> {
        let thinking = blocks_text(self.snapshot(), BlockKind::Thinking);
        let text = blocks_text(final_content, BlockKind::Text);
        self.0 = None;
        materialize(thinking, text)
    }

    /// Materialize a live snapshot after a cancel/crash mid-stream: BOTH
    /// Thinking and text come from the last snapshot (there is no final
    /// content). Empties the snapshot; a no-op when already idle.
    #[must_use = "the materialized items ARE the salvaged mid-stream content - dropping them loses it (the snapshot is already emptied)"]
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
#[path = "../../../tests/ui/transcript/streaming.rs"]
mod tests;
