
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
