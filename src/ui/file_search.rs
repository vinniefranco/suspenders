//! The adapter-side `@path` file search (Phase C2, qwen `useAtCompletion` →
//! `core`'s `FileSearch`). The PURE ranking lives in [`crate::ui::completion`]
//! (`rank_paths`, reusing the nucleo matcher); this adapter module owns the two
//! IMPURE parts qwen's file searcher does: the gitignore-aware project WALK
//! (via [`crate::walk::walk_files`]) and a short-TTL CACHE of it, so a burst of
//! per-keystroke searches walks the tree once, not once per keypress (qwen
//! caches 30s).
//!
//! The composer emits one [`Effect::FileSearch`](crate::ui::screen::Effect::FileSearch)
//! per AT keystroke; the adapter spawns [`run`] off the event loop (ADR-0011),
//! which reads the cached walk, ranks + caps it against the pattern, and posts
//! [`Event::FileSearchReady`] back through the loop's injection channel - the
//! same seam `/model`'s selector fetch uses.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::event::{Event, FileSuggestion};
use crate::ui::completion::{self, MAX_SUGGESTIONS_TO_SHOW};
use crate::walk;

/// How long a walked file list stays fresh before the next search re-walks the
/// tree (qwen's `FileSearch` `cacheTtl: 30`). Short enough that a file created
/// mid-session shows up promptly, long enough that a keystroke burst reuses it.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// The most AT suggestions a search returns (qwen `MAX_SUGGESTIONS_TO_SHOW *
/// 3`): the popup windows to [`MAX_SUGGESTIONS_TO_SHOW`], so a few extra let the
/// user scroll without an unbounded payload crossing the seam.
const MAX_AT_RESULTS: usize = MAX_SUGGESTIONS_TO_SHOW * 3;

/// A short-TTL cache of the project's walked file list (repo-relative paths),
/// shared across the spawned search tasks so a keystroke burst walks once. The
/// `Arc<Mutex<_>>` lets the immutable [`crate::ui::AdapterCtx`] hand a clone to
/// each spawned task; the walk itself is cheap to hold (a `Vec<String>`).
#[derive(Clone, Default)]
pub(crate) struct WalkCache {
    inner: Arc<Mutex<Option<CachedWalk>>>,
}

// One cached walk: the repo-relative paths and when they were walked.
struct CachedWalk {
    paths: Vec<String>,
    walked_at: Instant,
}

impl WalkCache {
    /// A fresh, empty cache (the first search walks the tree).
    pub(crate) fn new() -> Self {
        WalkCache::default()
    }

    // The repo-relative file paths under `root`, from the cache when fresh
    // (within [`CACHE_TTL`]), else a fresh walk stored back into the cache. A
    // poisoned lock (a panicked task) or an unreadable root falls back to a live
    // walk, never a panic.
    fn paths(&self, root: &Path) -> Vec<String> {
        if let Ok(guard) = self.inner.lock()
            && let Some(cached) = guard.as_ref()
            && cached.walked_at.elapsed() < CACHE_TTL
        {
            return cached.paths.clone();
        }
        let paths = walk_relative(root);
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(CachedWalk {
                paths: paths.clone(),
                walked_at: Instant::now(),
            });
        }
        paths
    }
}

// Walks `root` (gitignore-aware, symlink-safe, sorted) and returns the file
// paths RELATIVE to `root`, `/`-separated - the repo-relative labels the picker
// shows. A path that will not strip its root prefix (never, the walk returns
// absolute paths under root) falls back to its lossy display form.
fn walk_relative(root: &Path) -> Vec<String> {
    walk::walk_files(root)
        .iter()
        .map(|p| relative_display(root, p))
        .collect()
}

// `path` relative to `root`, `/`-separated for display (Windows `\` normalized).
fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Ranks + caps the (cached) project walk against `query` and builds the AT
/// fill rows (qwen `FileSearch.search` → `results.map({label, value:
/// escapePath})`). PURE ranking is delegated to [`completion::rank_paths`]; here
/// we cap to [`MAX_AT_RESULTS`] and escape each kept path's spaces so the
/// composer's detection round-trips. Exposed for the adapter's spawned task
/// AND unit tests.
pub(crate) fn search(cache: &WalkCache, root: &Path, query: &str) -> Vec<FileSuggestion> {
    let paths = cache.paths(root);
    completion::rank_paths(&paths, query)
        .into_iter()
        .take(MAX_AT_RESULTS)
        .map(|s| FileSuggestion {
            value: escape_path(&s.value),
            label: s.label,
            matched: s.matched,
        })
        .collect()
}

/// Escapes the spaces in an inserted `@path` value with backslashes (qwen
/// `escapePath`, narrowed to the space the AT scan round-trips on). Kept beside
/// the search so the wire `value` arrives already-escaped; the composer's own
/// detection treats `\ ` as part of one pattern.
fn escape_path(path: &str) -> String {
    // Escape every space; the walk paths never contain a pre-existing backslash
    // escape, so a simple replace is faithful (and idempotent for our inputs).
    path.replace(' ', "\\ ")
}

/// Spawns the file search off the event loop (ADR-0011) and posts the result as
/// [`Event::FileSearchReady`] back through `tx` (the loop's injection channel),
/// echoing `generation` + `query` so the composer's guard can drop a stale
/// keystroke's result. Mirrors `model_command::run`'s spawn+post shape. A failed
/// or empty walk simply posts empty `suggestions` (the popup shows "no matches").
pub(crate) fn spawn(
    cache: WalkCache,
    root: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
    query: String,
    generation: u64,
) {
    tokio::spawn(async move {
        // Echo the searched pattern back on the fill (the composer's guard drops
        // a fill whose query no longer matches the live pattern); capture it
        // before `query` moves into the blocking closure.
        let query_echo = query.clone();
        // The walk is blocking IO; run it on the blocking pool so it never
        // stalls the runtime under a large tree.
        let suggestions = tokio::task::spawn_blocking(move || search(&cache, &root, &query))
            .await
            .unwrap_or_default();
        let _ = tx.send(Event::file_search_ready(
            generation,
            query_echo,
            suggestions,
        ));
    });
}

#[cfg(test)]
#[path = "../../tests/unit/ui/file_search.rs"]
mod tests;
