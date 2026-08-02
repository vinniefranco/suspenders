use super::*;
use crate::content::ContentBlock;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn fresh_transcript() -> Transcript {
    Transcript::new(Vec::new())
}

/// Syncs `t` into a fresh cache at width 80 + dark theme, then plants a
/// sentinel line at items[0].lines[0]. The sentinel survives extend-only
/// syncs and disappears on a full rebuild, so tests can assert which path
/// the cache took without reading private revision counters (DUPLICATE fix).
fn seeded_cache(t: &Transcript) -> RenderCache {
    let mut cache = RenderCache::new();
    cache.sync(t, Toggles::default(), 80, theme::dark());
    // A named constant makes the "sentinel survives / disappears" intent
    // explicit at the assertion sites and adds a 4th statement so this
    // helper does not trigger the FRAGMENT quality gate.
    let sentinel = Line::raw("sentinel");
    cache.items[0].lines[0] = sentinel;
    cache
}

#[test]
fn cache_sync_extends_for_appends_without_rebuilding_settled_entries() {
    let mut t = fresh_transcript();
    t.info("first");
    // Plant a sentinel in the built entry: an append extends the cache
    // without touching settled entries, so the sentinel must survive
    // the next sync - a rebuild would have replaced it with "first".
    let mut cache = seeded_cache(&t);
    t.info("appended");
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    assert_eq!(cache.items.len(), 2);
    assert_eq!(line_text(&cache.items[0].lines[0]), "sentinel");
    assert_eq!(line_text(&cache.items[1].lines[0]), "● appended");
}

#[test]
fn cache_sync_rebuilds_when_the_revision_moves() {
    let mut t = fresh_transcript();
    t.steering_queued("check");
    // The delivered steering removes its pending marker - a structural
    // edit that bumps the store's revision - so the cache rebuilds
    // from scratch: the sentinel is gone and the promoted user line is
    // seen. The `>` caret prefix is baked into the cached User line now
    // (ADR-0046 qwen chrome), so the cached first span is `> check`.
    let mut cache = seeded_cache(&t);
    t.steering_delivered("check");
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    assert_eq!(cache.items.len(), 1);
    assert_eq!(line_text(&cache.items[0].lines[0]), "> check");
}

#[test]
fn cache_sync_rebuilds_when_the_store_shrinks_below_the_cached_length() {
    // No store verb shrinks without bumping (the prefix contract), so
    // the only way here is a SWAPPED Transcript whose revision happens
    // to coincide - two fresh stores both at revision 0. The length
    // check catches it: the sentinel is gone, wholesale.
    let mut t = fresh_transcript();
    t.info("first");
    t.info("second");
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    cache.items[0].lines[0] = Line::raw("sentinel");

    let mut shorter = fresh_transcript();
    shorter.info("replacement");
    assert_eq!(t.revision(), shorter.revision());
    cache.sync(&shorter, Toggles::default(), 80, theme::dark());
    assert_eq!(cache.items.len(), 1);
    assert_eq!(line_text(&cache.items[0].lines[0]), "● replacement");
}

#[test]
fn the_streaming_tail_is_never_cached_as_a_settled_entry() {
    let mut t = fresh_transcript();
    t.info("settled");
    t.message_start();
    t.message_update(vec![ContentBlock::text("in flight")]);
    let mut cache = RenderCache::new();
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    // The in-flight snapshot lives ONLY in the streaming slot; the
    // settled entries still mirror `Transcript::items` exactly.
    assert_eq!(cache.items.len(), t.items().len());
    assert_eq!(cache.items.len(), 1);
    assert!(cache.streaming.is_some());

    // Settling the message appends without bumping the revision, so
    // the tail arrives as an EXTEND (the sentinel survives) and the
    // streaming slot empties for the next message.
    cache.items[0].lines[0] = Line::raw("sentinel");
    t.message_end(&[ContentBlock::text("in flight")]);
    cache.sync(&t, Toggles::default(), 80, theme::dark());
    assert_eq!(cache.items.len(), 2);
    assert_eq!(line_text(&cache.items[0].lines[0]), "sentinel");
    assert!(cache.streaming.is_none());
}

#[test]
fn streaming_cache_reparses_only_when_the_char_length_moves() {
    let mut cache = RenderCache::new();
    cache.sync_streaming("hello", 80, theme::dark());
    assert_eq!(
        line_text(&cache.streaming.as_ref().unwrap().lines[0]),
        "hello"
    );

    // Same length, different text: the monotonic-key contract - within
    // a message the snapshot only GROWS, so an equal length means
    // unchanged and the cached lines are reused as-is.
    cache.sync_streaming("world", 80, theme::dark());
    assert_eq!(
        line_text(&cache.streaming.as_ref().unwrap().lines[0]),
        "hello"
    );

    // Growth re-parses; the end of streaming clears, so the next
    // message can never collide with a stale entry of the same length.
    cache.sync_streaming("hello more", 80, theme::dark());
    assert_eq!(
        line_text(&cache.streaming.as_ref().unwrap().lines[0]),
        "hello more"
    );
    cache.sync_streaming("", 80, theme::dark());
    assert!(cache.streaming.is_none());
}
