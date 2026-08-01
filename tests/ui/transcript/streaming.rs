
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
