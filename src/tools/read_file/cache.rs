//! read_file's read-cache recording (F6, ADR-0060) - the `(mtime, size)`
//! fingerprint stamp a successful read leaves in the Run's file-read cache, so a
//! later `notebook_edit` can verify the model saw the bytes it is editing. Split
//! out of `read_file.rs` so the dispatch module does not carry the stat/record
//! plumbing inline.

use crate::content::ResultBlock;
use crate::tool::{ToolCtx, ToolOutput};

/// Record a successful read of `abs` into the Run's file-read cache. Stats the
/// file for the `(mtime, size)` fingerprint; a stat failure here is silently
/// dropped (the read already succeeded, and a later check simply reads
/// `Unknown` / `Stale` and asks the model to re-read - never a false pass).
/// `full` is whether the read saw the whole current content; `cacheable` is
/// whether the result is a single Text block.
pub(super) fn record_read(ctx: &ToolCtx, abs: &std::path::Path, full: bool, cacheable: bool) {
    if let Some((mtime_ms, size)) = stat_fingerprint(abs) {
        ctx.read_cache()
            .record_read(abs.to_path_buf(), mtime_ms, size, full, cacheable);
    }
}

/// The `(mtime_ms, size)` fingerprint of `abs`, or `None` if it cannot be
/// stat'd. mtime is milliseconds since the epoch (qwen `stats.mtimeMs`).
fn stat_fingerprint(abs: &std::path::Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(abs).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some((mtime_ms, meta.len()))
}

/// Whether the output is a single Text block - qwen's `cacheable` (plain text
/// vs. a media / native-PDF payload the mutating tools cannot alter as text).
pub(super) fn is_single_text_block(output: &ToolOutput) -> bool {
    matches!(output.blocks.as_slice(), [ResultBlock::Text { .. }])
}
