//! History — persistent prompt history across sessions.
//!
//! Each row is one submitted prompt. The store is size-capped (a wrap log):
//! the oldest entries are discarded once the file exceeds the cap, so history
//! stays bounded without unbounded growth.
//!
//! Callers are the UI at mount (read the log into the Transcript) and on every
//! successful submit (append the prompt). The UI view owns the lifecycle:
//! [`open`] on mount, [`close`] on exit.
//!
//! baud backs this with Erlang's `:disk_log` (a two-file wrap log at ~100 kB
//! each). Rust has no `:disk_log`, so this port keeps the same *contract* — a
//! bounded, append-only, order-preserving, crash-tolerant prompt ring — over a
//! single newline-delimited file trimmed to the cap. Prompts are line-oriented
//! user text, so one prompt per line round-trips without escaping; a torn tail
//! is dropped on read, never load-bearing.

use std::io::Write;
use std::path::Path;

/// The combined cap across the wrap log (~200 kB, matching baud's 2×100 kB).
const MAX_BYTES: usize = 200_000;

/// A handle to an opened history store: just its path. Reads and appends
/// re-open the file so a crash between calls never loses committed rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub path: String,
}

/// Opens (or creates) the history store at `path`. Safe to call multiple times
/// per session. Returns the path on success, or an error string on failure to
/// create the parent directory.
pub fn open(path: &str) -> Result<History, String> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Touch the file so a subsequent read of a fresh log succeeds.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    Ok(History { path: path.to_string() })
}

impl History {
    /// Closes the history store. A no-op here (no held handle); present to
    /// match the lifecycle contract. Returns `()`.
    pub fn close(&self) {}

    /// Reads all entries from oldest to newest. Returns an empty list when the
    /// store is empty or on any error (the history ring starts fresh).
    pub fn read(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => content.lines().map(|l| l.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Appends one prompt. Does not deduplicate or cap the *count* — that is
    /// the in-memory Transcript's job — but trims the oldest rows to keep the
    /// file under the byte cap (the wrap). Silently ignores errors so a full
    /// disk or corrupted store never crashes the UI.
    pub fn append(&self, text: &str) {
        let mut rows = self.read();
        rows.push(text.to_string());

        // Wrap: drop oldest rows until the serialized size fits the cap.
        while serialized_len(&rows) > MAX_BYTES && rows.len() > 1 {
            rows.remove(0);
        }

        let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            let _ = f.write_all(body.as_bytes());
        }
    }
}

fn serialized_len(rows: &[String]) -> usize {
    rows.iter().map(|r| r.len() + 1).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> History {
        let path = dir.path().join("nested/history.log");
        open(&path.to_string_lossy()).unwrap()
    }

    #[test]
    fn open_creates_the_parent_directory_and_a_fresh_store_reads_empty() {
        let tmp = TempDir::new().unwrap();
        let h = store(&tmp);
        assert_eq!(h.read(), Vec::<String>::new());
    }

    #[test]
    fn append_then_read_returns_oldest_to_newest() {
        let tmp = TempDir::new().unwrap();
        let h = store(&tmp);
        h.append("first prompt");
        h.append("second prompt");
        h.append("third prompt");

        assert_eq!(
            h.read(),
            vec![
                "first prompt".to_string(),
                "second prompt".to_string(),
                "third prompt".to_string(),
            ]
        );
    }

    #[test]
    fn append_does_not_deduplicate() {
        let tmp = TempDir::new().unwrap();
        let h = store(&tmp);
        h.append("same");
        h.append("same");
        assert_eq!(h.read(), vec!["same".to_string(), "same".to_string()]);
    }

    #[test]
    fn reading_a_missing_store_yields_an_empty_list() {
        let h = History {
            path: "/nonexistent/dir/history.log".to_string(),
        };
        assert_eq!(h.read(), Vec::<String>::new());
    }

    #[test]
    fn the_wrap_discards_the_oldest_entries_past_the_cap() {
        let tmp = TempDir::new().unwrap();
        let h = store(&tmp);

        // One long-lived marker, then enough bulk to blow past the cap.
        h.append("OLDEST");
        let bulk = "x".repeat(10_000);
        for _ in 0..30 {
            h.append(&bulk);
        }
        h.append("NEWEST");

        let rows = h.read();
        // The newest survives; the oldest was wrapped out; the file stays bounded.
        assert_eq!(rows.last().unwrap(), "NEWEST");
        assert!(!rows.contains(&"OLDEST".to_string()));
        assert!(serialized_len(&rows) <= MAX_BYTES);
    }

    #[test]
    fn open_is_idempotent_and_preserves_existing_rows() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("history.log");
        let p = path.to_string_lossy().into_owned();

        let h = open(&p).unwrap();
        h.append("kept");

        let reopened = open(&p).unwrap();
        assert_eq!(reopened.read(), vec!["kept".to_string()]);
    }

    #[test]
    fn close_is_a_noop_that_leaves_the_store_readable() {
        let tmp = TempDir::new().unwrap();
        let h = store(&tmp);
        h.append("kept");
        h.close();
        assert_eq!(h.read(), vec!["kept".to_string()]);
    }
}
