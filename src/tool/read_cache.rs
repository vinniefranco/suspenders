//! The session (Run-scoped) file-read cache - the enforcement half of qwen
//! v0.16.0 `packages/core/src/services/fileReadCache.ts`.
//!
//! It tracks which files the model has Read or written in THIS Run, plus the
//! `(mtime, size)` snapshot at that operation, so a mutating tool can verify the
//! model is editing bytes it has actually seen and that those bytes have not
//! drifted on disk since. P3 3c lands enforcement ONLY: `notebook_edit` is the
//! sole consumer, and it consults [`FileReadCache::check`] plus the
//! [`FileReadEntry::last_read_was_full`] flag before mutating a notebook. The
//! qwen `file_unchanged` read fast-path (short-circuit a repeated full read) is
//! DEFERRED and NOT ported here (ADR-0060), so the fast-path-only fields
//! (`readResidentInHistory`, the FIFO eviction, `markReadEvictedFromHistory`)
//! are left out with it.
//!
//! ## Path-keyed, not inode-keyed (the Suspenders simplification)
//!
//! qwen keys entries by `${dev}:${ino}` so symlinks / hardlinks / case-variant
//! paths collapse onto one file. Suspenders confines every tool path to the
//! Project Root ([`crate::tool::path::with_path`]) and records ABSOLUTE resolved
//! paths, so a plain `PathBuf` key suffices: the root confinement already
//! removes the symlink-escape surface the inode key defends, and a `read_file`
//! then `notebook_edit` on the same relative path resolve to the same absolute
//! path. The narrowing is deliberate (ADR-0060).
//!
//! ## Pure in-memory, caller stats
//!
//! Like qwen's, this is a pure data structure: callers `stat` the file and pass
//! the resulting `(mtime_ms, size)` in. The cache never touches the filesystem,
//! which keeps it trivially testable and avoids a double-stat at the read-then-
//! record and edit-then-check sites (they stat for their own reasons anyway).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// A single tracked file (qwen `FileReadEntry`, narrowed to the enforcement
/// fields). `mtime_ms`/`size_bytes` are the fingerprint [`FileReadCache::check`]
/// compares against the current on-disk stat; the `last_*` flags describe the
/// most recent record so `notebook_edit` can require a FULL read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadEntry {
    /// mtime in ms at the time of the most recent record.
    pub mtime_ms: u128,
    /// Size in bytes at the time of the most recent record.
    pub size_bytes: u64,
    /// ms epoch of the last successful Read. `None` if never read (a write-only
    /// entry). qwen's `lastReadAt` - `notebook_edit` requires it to be `Some`.
    pub last_read_at: Option<u128>,
    /// ms epoch of the last successful write. `None` if never written. qwen's
    /// `lastWriteAt`; diagnostic in the enforcement-only port.
    pub last_write_at: Option<u128>,
    /// True iff the most recent Read produced the whole file's current content:
    /// no offset / limit / pages AND the output was not truncated. A ranged or
    /// truncated read records `false`. qwen's `lastReadWasFull` -
    /// `notebook_edit` requires it, since a cell-level edit needs the model to
    /// have seen every byte.
    pub last_read_was_full: bool,
    /// True iff the most recent Read produced plain text (vs. a media / native-
    /// PDF / notebook payload the mutating tools cannot alter as text). qwen's
    /// `lastReadCacheable`; recorded here for the future `edit_file`/`write_file`
    /// adoption (DEFERRED, ADR-0060), unused by `notebook_edit`.
    pub last_read_cacheable: bool,
}

/// The result of [`FileReadCache::check`] (qwen `FileReadCheckResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    /// No entry: the file has never been Read or written in this Run.
    Unknown,
    /// An entry exists but its fingerprint drifted from the current stat - the
    /// file changed since we last saw it.
    Stale,
    /// An entry exists AND the fingerprint matches the current stat - the bytes
    /// are what we last recorded.
    Fresh,
}

/// The Run-scoped file-read cache (qwen `FileReadCache`, enforcement-only). A
/// concrete shared-state Capability, keyed by absolute [`PathBuf`]. The `Mutex`
/// makes it `Send + Sync` behind the `Arc` on [`crate::tool::caps::Capabilities`]
/// so it can cross the `tokio::spawn` at the Agent, the same way the registry
/// does (ADR-0055, ADR-0060).
#[derive(Debug, Default)]
pub struct FileReadCache {
    by_path: Mutex<HashMap<PathBuf, FileReadEntry>>,
}

impl FileReadCache {
    /// A fresh, empty cache.
    pub fn new() -> FileReadCache {
        FileReadCache {
            by_path: Mutex::new(HashMap::new()),
        }
    }

    /// Record a successful Read of `path` (qwen `recordRead`, narrowed).
    ///
    /// - `full` - the Read produced the entire current content: no offset /
    ///   limit / pages AND the output was not truncated. `notebook_edit` gates
    ///   on this being `true`.
    /// - `cacheable` - the produced content is plain text (vs. a media /
    ///   notebook / native-PDF payload). Recorded for the future
    ///   `edit_file`/`write_file` adoption (DEFERRED); `notebook_edit` ignores
    ///   it.
    ///
    /// The fingerprint is refreshed to `(mtime_ms, size)`, `last_read_at` is
    /// stamped, and the two flags are set to exactly what THIS read produced.
    /// The qwen sticky-on-true-across-same-fingerprint subtlety only matters to
    /// the fast-path (a `Read full -> Read partial` sequence keeping read-
    /// rights); the enforcement-only port does not carry the fast-path, so a
    /// plain overwrite is correct and matches qwen's own drift-reset arm.
    pub fn record_read(&self, path: PathBuf, mtime_ms: u128, size: u64, full: bool, cacheable: bool) {
        let mut by_path = self.lock();
        let entry = by_path.entry(path).or_insert_with(|| FileReadEntry {
            mtime_ms,
            size_bytes: size,
            last_read_at: None,
            last_write_at: None,
            last_read_was_full: false,
            last_read_cacheable: false,
        });
        entry.mtime_ms = mtime_ms;
        entry.size_bytes = size;
        entry.last_read_at = Some(now_ms());
        entry.last_read_was_full = full;
        entry.last_read_cacheable = cacheable;
    }

    /// Record a successful write of `path` (qwen `recordWrite`, narrowed). The
    /// model authored the current bytes the mutating tool produced, so for
    /// prior-read enforcement it has now "seen" them: the fingerprint refreshes
    /// to the post-write stat (else the next check would read the tool's OWN
    /// write as a stale external change), read metadata is refreshed alongside
    /// (`last_read_was_full = true`), and `last_read_cacheable` is set `false`
    /// because the notebook writer produces a structured payload, matching qwen's
    /// `notebook-edit.ts` `recordWrite({ cacheable: false })`.
    pub fn record_write(&self, path: PathBuf, mtime_ms: u128, size: u64) {
        let mut by_path = self.lock();
        let now = now_ms();
        let entry = by_path.entry(path).or_insert_with(|| FileReadEntry {
            mtime_ms,
            size_bytes: size,
            last_read_at: None,
            last_write_at: None,
            last_read_was_full: false,
            last_read_cacheable: false,
        });
        entry.mtime_ms = mtime_ms;
        entry.size_bytes = size;
        entry.last_write_at = Some(now);
        entry.last_read_at = Some(now);
        entry.last_read_was_full = true;
        entry.last_read_cacheable = false;
    }

    /// Compare the cached fingerprint for `path` against the current
    /// `(mtime_ms, size)` (qwen `check`):
    ///
    /// - [`ReadState::Unknown`] - no entry (never Read or written this Run).
    /// - [`ReadState::Stale`] - an entry exists but mtime or size drifted.
    /// - [`ReadState::Fresh`] - an entry exists AND both match.
    ///
    /// Like qwen's, mtime + size is a best-effort fingerprint, not a hash: a
    /// rewrite with identical mtime AND size reads `Fresh`. The mutating tool's
    /// own `0 occurrences` / apply failure is the safety net there.
    pub fn check(&self, path: &std::path::Path, mtime_ms: u128, size: u64) -> ReadState {
        let by_path = self.lock();
        match by_path.get(path) {
            None => ReadState::Unknown,
            Some(entry) if entry.mtime_ms == mtime_ms && entry.size_bytes == size => {
                ReadState::Fresh
            }
            Some(_) => ReadState::Stale,
        }
    }

    /// Drop the entry for `path`, if any (qwen `invalidate`). notebook_edit
    /// calls this after a structural edit that lost stable cell ids, INSTEAD of
    /// [`record_write`](FileReadCache::record_write): the `cell-N` fallback
    /// display ids renumbered, so the model must re-read before it can target a
    /// cell again. Removing the entry makes the next
    /// [`check`](FileReadCache::check) read [`Unknown`], which the read-before-
    /// edit gate rejects until a fresh read is recorded.
    ///
    /// [`Unknown`]: ReadState::Unknown
    pub fn invalidate(&self, path: &std::path::Path) {
        self.lock().remove(path);
    }

    /// The entry for `path`, cloned. notebook_edit reads it after a [`Fresh`]
    /// [`check`] to inspect [`FileReadEntry::last_read_was_full`] (a full read is
    /// required for a cell-level edit) and [`FileReadEntry::last_read_at`]
    /// (distinguish a read entry from a write-only one), without exposing the
    /// `Mutex`. Also test/diagnostic use.
    ///
    /// [`Fresh`]: ReadState::Fresh
    /// [`check`]: FileReadCache::check
    pub fn entry(&self, path: &std::path::Path) -> Option<FileReadEntry> {
        self.lock().get(path).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, FileReadEntry>> {
        // A poisoned lock means a prior holder panicked mid-record; the map is
        // still structurally sound (a plain HashMap of owned values), so recover
        // the guard rather than propagate the panic through every tool call.
        self.by_path.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The current wall-clock time in ms since the epoch (qwen `Date.now()`). Used
/// only for the diagnostic `last_read_at`/`last_write_at` stamps; enforcement
/// keys on the fingerprint, not the timestamp. A clock before the epoch (never,
/// in practice) records 0.
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn an_untracked_path_is_unknown() {
        let cache = FileReadCache::new();
        assert_eq!(cache.check(&p("/a"), 1, 2), ReadState::Unknown);
    }

    #[test]
    fn a_recorded_read_with_the_same_fingerprint_is_fresh() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 100, 200, true, true);
        assert_eq!(cache.check(&p("/a"), 100, 200), ReadState::Fresh);
    }

    #[test]
    fn a_drifted_mtime_or_size_reads_stale() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 100, 200, true, true);
        assert_eq!(cache.check(&p("/a"), 101, 200), ReadState::Stale);
        assert_eq!(cache.check(&p("/a"), 100, 201), ReadState::Stale);
    }

    #[test]
    fn unknown_then_stale_then_fresh_transitions() {
        let cache = FileReadCache::new();
        // Unknown: nothing recorded.
        assert_eq!(cache.check(&p("/a"), 1, 1), ReadState::Unknown);
        // Record a read at one fingerprint.
        cache.record_read(p("/a"), 10, 20, true, true);
        // A different fingerprint (the file changed on disk) reads Stale.
        assert_eq!(cache.check(&p("/a"), 11, 20), ReadState::Stale);
        // Re-recording at the new fingerprint makes it Fresh again.
        cache.record_read(p("/a"), 11, 20, true, true);
        assert_eq!(cache.check(&p("/a"), 11, 20), ReadState::Fresh);
    }

    #[test]
    fn record_read_stamps_the_read_flags() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 10, 20, false, true);
        let entry = cache.entry(&p("/a")).unwrap();
        assert!(entry.last_read_at.is_some());
        assert!(entry.last_write_at.is_none());
        assert!(!entry.last_read_was_full);
        assert!(entry.last_read_cacheable);
    }

    #[test]
    fn record_read_full_records_full() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 10, 20, true, false);
        let entry = cache.entry(&p("/a")).unwrap();
        assert!(entry.last_read_was_full);
        assert!(!entry.last_read_cacheable);
    }

    #[test]
    fn a_partial_read_over_a_prior_full_read_resets_the_full_flag() {
        // Enforcement-only: the flags describe the MOST RECENT read (qwen's
        // drift-reset arm; the sticky fast-path is not ported).
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 10, 20, true, true);
        cache.record_read(p("/a"), 10, 20, false, true);
        assert!(!cache.entry(&p("/a")).unwrap().last_read_was_full);
    }

    #[test]
    fn record_write_refreshes_fingerprint_and_marks_full_noncacheable() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 10, 20, true, true);
        // The write lands at a new fingerprint (a rewrite bumped mtime + size).
        cache.record_write(p("/a"), 30, 40);
        // The tool's own write is not seen as a stale external change.
        assert_eq!(cache.check(&p("/a"), 30, 40), ReadState::Fresh);
        let entry = cache.entry(&p("/a")).unwrap();
        assert!(entry.last_write_at.is_some());
        assert!(entry.last_read_at.is_some());
        assert!(entry.last_read_was_full);
        // A notebook write is a structured payload: not text-cacheable.
        assert!(!entry.last_read_cacheable);
    }

    #[test]
    fn invalidate_drops_the_entry_so_it_reads_unknown() {
        let cache = FileReadCache::new();
        cache.record_read(p("/a"), 10, 20, true, true);
        assert_eq!(cache.check(&p("/a"), 10, 20), ReadState::Fresh);
        // Invalidate drops the entry: the path reads Unknown again, which the
        // read-before-edit gate rejects until a fresh read is recorded.
        cache.invalidate(&p("/a"));
        assert_eq!(cache.check(&p("/a"), 10, 20), ReadState::Unknown);
        assert!(cache.entry(&p("/a")).is_none());
        // Invalidating an untracked path is a no-op, not a panic.
        cache.invalidate(&p("/never"));
    }

    #[test]
    fn record_write_on_a_fresh_path_creates_the_entry() {
        let cache = FileReadCache::new();
        cache.record_write(p("/a"), 30, 40);
        assert_eq!(cache.check(&p("/a"), 30, 40), ReadState::Fresh);
        assert!(cache.entry(&p("/a")).unwrap().last_read_at.is_some());
    }
}
