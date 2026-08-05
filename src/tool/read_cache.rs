//! The session (Run-scoped) file-read cache - the enforcement half of qwen
//! v0.16.0 `packages/core/src/services/fileReadCache.ts`.
//!
//! It tracks which files the model has Read or written in THIS Run, plus the
//! `(mtime, size)` snapshot at that operation, so a mutating tool can verify the
//! model is editing bytes it has actually seen and that those bytes have not
//! drifted on disk since. Two consumers:
//!
//!  - `notebook_edit` (enforcement): consults [`FileReadCache::check`] plus the
//!    [`FileReadEntry::last_read_was_full`] flag before mutating a notebook.
//!  - `read_file` (the `file_unchanged` fast-path): consults
//!    [`FileReadCache::is_unchanged_full_read`] to serve qwen's unchanged
//!    placeholder instead of re-reading a file it already fully read this Run.
//!
//! qwen's fast-path additionally gates on `readResidentInHistory` (the prior
//! content still being present in the transcript after idle micro-compaction)
//! and carries a FIFO history eviction. Those are transcript-lifecycle concerns
//! outside this port's model contract; the placeholder itself instructs the
//! model to re-read after compaction, so they are left out (ADR-0060).
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

/// The `(mtime, size)` snapshot of a file at one moment - the staleness
/// fingerprint every cache operation keys on (qwen `stats.mtimeMs` +
/// `stats.size`). One value, built by [`Fingerprint::of`] from a stat the
/// CALLER performs (the cache itself never touches the filesystem), so the
/// pairing can never be transposed or half-updated across the record/check
/// sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    /// mtime in ms since the epoch (`0` when the platform reports none -
    /// qwen's `stats.mtimeMs` fallback).
    pub mtime_ms: u128,
    /// Size in bytes.
    pub size_bytes: u64,
}

impl Fingerprint {
    /// The fingerprint of an already-stat'd file. The mtime falls back to `0`
    /// when the platform cannot report a modified time (matching every prior
    /// per-tool copy of this conversion).
    pub fn of(meta: &std::fs::Metadata) -> Fingerprint {
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Fingerprint {
            mtime_ms,
            size_bytes: meta.len(),
        }
    }

    /// Stat `path` and fingerprint it - the one stat-to-fingerprint conversion
    /// (previously copy-pasted per tool). Callers own the error wording: a
    /// read tolerates a miss, a mutating tool refuses on one.
    pub fn stat(path: &std::path::Path) -> std::io::Result<Fingerprint> {
        std::fs::metadata(path).map(|meta| Fingerprint::of(&meta))
    }
}

/// A single tracked file (qwen `FileReadEntry`, narrowed to the enforcement
/// fields). `fingerprint` is what [`FileReadCache::check`] compares against the
/// current on-disk stat; the `last_*` flags describe the most recent record so
/// `notebook_edit` can require a FULL read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadEntry {
    /// The `(mtime, size)` snapshot at the time of the most recent record.
    pub fingerprint: Fingerprint,
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

impl FileReadEntry {
    /// A fresh entry at `fingerprint` with no read/write stamped yet - the
    /// seed both `record_read` and `record_write` insert before overwriting the
    /// operation-specific fields.
    fn seed(fingerprint: Fingerprint) -> FileReadEntry {
        FileReadEntry {
            fingerprint,
            last_read_at: None,
            last_write_at: None,
            last_read_was_full: false,
            last_read_cacheable: false,
        }
    }
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
    /// The fingerprint is refreshed to `fingerprint`, `last_read_at` is
    /// stamped, and the two flags are set to exactly what THIS read produced.
    /// The qwen sticky-on-true-across-same-fingerprint subtlety only matters to
    /// the fast-path (a `Read full -> Read partial` sequence keeping read-
    /// rights); the enforcement-only port does not carry the fast-path, so a
    /// plain overwrite is correct and matches qwen's own drift-reset arm.
    pub fn record_read(
        &self,
        path: PathBuf,
        fingerprint: Fingerprint,
        full: bool,
        cacheable: bool,
    ) {
        let mut by_path = self.lock();
        let entry = by_path
            .entry(path)
            .or_insert_with(|| FileReadEntry::seed(fingerprint));
        entry.fingerprint = fingerprint;
        entry.last_read_at = Some(now_ms());
        entry.last_read_was_full = full;
        entry.last_read_cacheable = cacheable;
    }

    /// Record a successful write of `path` (qwen `recordWrite`, narrowed). The
    /// model authored the current bytes the mutating tool produced, so for
    /// prior-read enforcement it has now "seen" them: the fingerprint refreshes
    /// to the post-write stat (else the next check would read the tool's OWN
    /// write as a stale external change), and read metadata is refreshed
    /// alongside (`last_read_was_full = true`).
    ///
    /// `cacheable` mirrors qwen's `recordWrite({ cacheable })` (default `true`):
    /// a plain-text writer (`write_file`) passes `true`, so a following full
    /// `read_file` can serve the `file_unchanged` placeholder; the notebook
    /// writer passes `false` because it produces a structured payload the model
    /// must re-materialize, matching `notebook-edit.ts recordWrite({ cacheable:
    /// false })`.
    pub fn record_write(&self, path: PathBuf, fingerprint: Fingerprint, cacheable: bool) {
        let mut by_path = self.lock();
        let now = now_ms();
        let entry = by_path
            .entry(path)
            .or_insert_with(|| FileReadEntry::seed(fingerprint));
        entry.fingerprint = fingerprint;
        entry.last_write_at = Some(now);
        entry.last_read_at = Some(now);
        entry.last_read_was_full = true;
        entry.last_read_cacheable = cacheable;
    }

    /// Compare the cached fingerprint for `path` against the current one
    /// (qwen `check`):
    ///
    /// - [`ReadState::Unknown`] - no entry (never Read or written this Run).
    /// - [`ReadState::Stale`] - an entry exists but mtime or size drifted.
    /// - [`ReadState::Fresh`] - an entry exists AND the fingerprint matches.
    ///
    /// Like qwen's, mtime + size is a best-effort fingerprint, not a hash: a
    /// rewrite with identical mtime AND size reads `Fresh`. The mutating tool's
    /// own `0 occurrences` / apply failure is the safety net there.
    pub fn check(&self, path: &std::path::Path, fingerprint: Fingerprint) -> ReadState {
        let by_path = self.lock();
        match by_path.get(path) {
            None => ReadState::Unknown,
            Some(entry) if entry.fingerprint == fingerprint => ReadState::Fresh,
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

    /// Whether a repeat full Read of `path` at the current `fingerprint`
    /// can be served from cache with qwen's `file_unchanged` placeholder instead
    /// of re-reading the bytes (qwen `read-file.ts`'s fast-path predicate,
    /// narrowed). All must hold:
    ///
    /// - the fingerprint is [`Fresh`] (mtime + size match what we last saw), and
    /// - the last Read produced the WHOLE current content
    ///   ([`FileReadEntry::last_read_was_full`]), and
    /// - the last Read produced plain text ([`FileReadEntry::last_read_cacheable`]);
    ///   a media / native-PDF / notebook payload cannot be quoted back as
    ///   "unchanged text", and
    /// - the Read is not older than the last write to this path
    ///   (`last_read_at > last_write_at`) - a write since the read means the
    ///   model has not seen the current bytes.
    ///
    /// qwen also gates on `readResidentInHistory` (the prior content still being
    /// present in the transcript after idle micro-compaction). That flag is a
    /// compaction concern out of this port's model contract; the placeholder
    /// itself tells the model to re-read if it cannot retrieve the prior content
    /// (e.g. after compaction), so omitting the flag is safe here (ADR-0060).
    ///
    /// [`Fresh`]: ReadState::Fresh
    pub fn is_unchanged_full_read(&self, path: &std::path::Path, fingerprint: Fingerprint) -> bool {
        let by_path = self.lock();
        by_path.get(path).is_some_and(|entry| {
            entry.fingerprint == fingerprint
                && entry.last_read_was_full
                && entry.last_read_cacheable
                && entry.last_read_at.is_some()
                && entry.last_read_at >= entry.last_write_at
        })
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
#[path = "../../tests/tool/read_cache.rs"]
mod tests;
