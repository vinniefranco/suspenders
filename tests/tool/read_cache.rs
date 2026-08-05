use super::*;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn fp(mtime_ms: u128, size_bytes: u64) -> Fingerprint {
    Fingerprint {
        mtime_ms,
        size_bytes,
    }
}

#[test]
fn an_untracked_path_is_unknown() {
    let cache = FileReadCache::new();
    assert_eq!(cache.check(&p("/a"), fp(1, 2)), ReadState::Unknown);
}

#[test]
fn a_recorded_read_with_the_same_fingerprint_is_fresh() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(100, 200), true, true);
    assert_eq!(cache.check(&p("/a"), fp(100, 200)), ReadState::Fresh);
}

#[test]
fn a_drifted_mtime_or_size_reads_stale() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(100, 200), true, true);
    assert_eq!(cache.check(&p("/a"), fp(101, 200)), ReadState::Stale);
    assert_eq!(cache.check(&p("/a"), fp(100, 201)), ReadState::Stale);
}

#[test]
fn unknown_then_stale_then_fresh_transitions() {
    let cache = FileReadCache::new();
    // Unknown: nothing recorded.
    assert_eq!(cache.check(&p("/a"), fp(1, 1)), ReadState::Unknown);
    // Record a read at one fingerprint.
    cache.record_read(p("/a"), fp(10, 20), true, true);
    // A different fingerprint (the file changed on disk) reads Stale.
    assert_eq!(cache.check(&p("/a"), fp(11, 20)), ReadState::Stale);
    // Re-recording at the new fingerprint makes it Fresh again.
    cache.record_read(p("/a"), fp(11, 20), true, true);
    assert_eq!(cache.check(&p("/a"), fp(11, 20)), ReadState::Fresh);
}

#[test]
fn record_read_stamps_the_read_flags() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), false, true);
    let entry = cache.entry(&p("/a")).unwrap();
    assert!(entry.last_read_at.is_some());
    assert!(entry.last_write_at.is_none());
    assert!(!entry.last_read_was_full);
    assert!(entry.last_read_cacheable);
}

#[test]
fn record_read_full_records_full() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, false);
    let entry = cache.entry(&p("/a")).unwrap();
    assert!(entry.last_read_was_full);
    assert!(!entry.last_read_cacheable);
}

#[test]
fn a_partial_read_over_a_prior_full_read_resets_the_full_flag() {
    // Enforcement-only: the flags describe the MOST RECENT read (qwen's
    // drift-reset arm; the sticky fast-path is not ported).
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, true);
    cache.record_read(p("/a"), fp(10, 20), false, true);
    assert!(!cache.entry(&p("/a")).unwrap().last_read_was_full);
}

#[test]
fn record_write_refreshes_fingerprint_and_honors_cacheable() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, true);
    // A cacheable (write_file) write lands at a new fingerprint (a rewrite
    // bumped mtime + size).
    cache.record_write(p("/a"), fp(30, 40), true);
    // The tool's own write is not seen as a stale external change.
    assert_eq!(cache.check(&p("/a"), fp(30, 40)), ReadState::Fresh);
    let entry = cache.entry(&p("/a")).unwrap();
    assert!(entry.last_write_at.is_some());
    assert!(entry.last_read_at.is_some());
    assert!(entry.last_read_was_full);
    // write_file is text-cacheable, so a following full read serves the
    // unchanged placeholder.
    assert!(entry.last_read_cacheable);
    assert!(cache.is_unchanged_full_read(&p("/a"), fp(30, 40)));

    // A notebook write (cacheable = false) refreshes the fingerprint but must
    // NOT serve the fast-path: it produced a structured payload the model
    // re-materializes.
    cache.record_write(p("/b"), fp(30, 40), false);
    assert!(!cache.entry(&p("/b")).unwrap().last_read_cacheable);
    assert!(!cache.is_unchanged_full_read(&p("/b"), fp(30, 40)));
}

#[test]
fn invalidate_drops_the_entry_so_it_reads_unknown() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, true);
    assert_eq!(cache.check(&p("/a"), fp(10, 20)), ReadState::Fresh);
    // Invalidate drops the entry: the path reads Unknown again, which the
    // read-before-edit gate rejects until a fresh read is recorded.
    cache.invalidate(&p("/a"));
    assert_eq!(cache.check(&p("/a"), fp(10, 20)), ReadState::Unknown);
    assert!(cache.entry(&p("/a")).is_none());
    // Invalidating an untracked path is a no-op, not a panic.
    cache.invalidate(&p("/never"));
}

#[test]
fn a_fresh_full_cacheable_read_serves_the_unchanged_fast_path() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, true);
    assert!(cache.is_unchanged_full_read(&p("/a"), fp(10, 20)));
}

#[test]
fn the_unchanged_fast_path_declines_on_every_disqualifier() {
    let cache = FileReadCache::new();
    // Untracked path: no fast-path.
    assert!(!cache.is_unchanged_full_read(&p("/a"), fp(10, 20)));
    // A drifted fingerprint declines.
    cache.record_read(p("/a"), fp(10, 20), true, true);
    assert!(!cache.is_unchanged_full_read(&p("/a"), fp(11, 20)));
    // A partial (non-full) read declines.
    cache.record_read(p("/b"), fp(10, 20), false, true);
    assert!(!cache.is_unchanged_full_read(&p("/b"), fp(10, 20)));
    // A non-cacheable (media / notebook) read declines.
    cache.record_read(p("/c"), fp(10, 20), true, false);
    assert!(!cache.is_unchanged_full_read(&p("/c"), fp(10, 20)));
}

#[test]
fn a_cacheable_write_serves_the_unchanged_fast_path_the_model_authored_the_bytes() {
    let cache = FileReadCache::new();
    cache.record_read(p("/a"), fp(10, 20), true, true);
    // A cacheable write lands after the read: record_write sets last_read_at
    // == last_write_at (both `now`), so the read-is-not-older guard (`>=`)
    // permits it. That is correct - the model authored the current bytes, so
    // quoting them back as "unchanged" is sound (this is qwen's file_unchanged
    // path after write_file).
    cache.record_write(p("/a"), fp(30, 40), true);
    let entry = cache.entry(&p("/a")).unwrap();
    assert!(entry.last_read_at >= entry.last_write_at);
    assert!(cache.is_unchanged_full_read(&p("/a"), fp(30, 40)));
}

#[test]
fn record_write_on_a_fresh_path_creates_the_entry() {
    let cache = FileReadCache::new();
    cache.record_write(p("/a"), fp(30, 40), true);
    assert_eq!(cache.check(&p("/a"), fp(30, 40)), ReadState::Fresh);
    assert!(cache.entry(&p("/a")).unwrap().last_read_at.is_some());
}
