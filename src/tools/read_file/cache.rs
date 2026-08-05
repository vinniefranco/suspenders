//! read_file's read-cache recording and the `file_unchanged` fast-path (F6,
//! ADR-0060) - the `(mtime, size)` fingerprint a successful read leaves in the
//! Run's file-read cache (so a later `notebook_edit` can verify the model saw
//! the bytes it is editing), plus the qwen fast-path that serves an "unchanged"
//! placeholder instead of re-reading a file already fully read this Run. Split
//! out of `read_file.rs` so the dispatch module does not carry the stat/record
//! plumbing inline.

use crate::content::ResultBlock;
use crate::tool::read_cache::Fingerprint;
use crate::tool::{ToolCtx, ToolOutput};

/// Record a successful read of `abs` into the Run's file-read cache. Stats the
/// file for its [`Fingerprint`]; a stat failure here is silently dropped (the
/// read already succeeded, and a later check simply reads `Unknown` / `Stale`
/// and asks the model to re-read - never a false pass). `full` is whether the
/// read saw the whole current content; `cacheable` is whether the result is a
/// single Text block.
pub(super) fn record_read(ctx: &ToolCtx, abs: &std::path::Path, full: bool, cacheable: bool) {
    if let Ok(fingerprint) = Fingerprint::stat(abs) {
        ctx.read_cache()
            .record_read(abs.to_path_buf(), fingerprint, full, cacheable);
    }
}

/// qwen's `file_unchanged` fast-path (read-file.ts): when the model asks for a
/// FULL read (no offset / limit / pages) of a file it already fully read this
/// Run and the file has not changed on disk since, return a placeholder that
/// quotes the prior read back rather than re-emitting the bytes. `None` means
/// the fast-path did not fire (re-read normally).
///
/// The placeholder is faithful to qwen's `unchangedResult`: it is explicit that
/// the full content was provided earlier in this conversation, and instructs the
/// model to re-read with explicit offset/limit if it cannot retrieve that prior
/// content (e.g. after compaction) or suspects an out-of-band mutation.
pub(super) fn unchanged_placeholder(
    ctx: &ToolCtx,
    source: &super::reader::SourceFile<'_>,
) -> Option<ToolOutput> {
    let fingerprint = Fingerprint::stat(source.abs).ok()?;
    if ctx
        .read_cache()
        .is_unchanged_full_read(source.abs, fingerprint)
    {
        Some(ToolOutput::text(unchanged_message(source.display_name())))
    } else {
        None
    }
}

/// The VERBATIM qwen `file_unchanged` placeholder text (read-file.ts
/// `unchangedResult`), with `display_name` (the basename) standing in for
/// qwen's shortened relative display path.
fn unchanged_message(display_name: &str) -> String {
    // VERBATIM from qwen read-file.ts `unchangedResult` (including its em-dash);
    // a model-facing tool string, so it is copied byte-for-byte rather than
    // house-styled.
    format!(
        "[File {display_name} unchanged since last read in this session \u{2014} \
the full content was provided earlier in this conversation. \
If you cannot retrieve that prior content (e.g. after context \
compaction) or you suspect the file was modified outside the read/edit \
tools (shell command, MCP tool, another process), re-read with \
explicit offset/limit to fetch current content.]"
    )
}

/// Whether the output is a single Text block - qwen's `cacheable` (plain text
/// vs. a media / native-PDF payload the mutating tools cannot alter as text).
pub(super) fn is_single_text_block(output: &ToolOutput) -> bool {
    matches!(output.blocks.as_slice(), [ResultBlock::Text { .. }])
}
