//! At Expansion (ADR-0068): the submit-time pass that reads every `@<path>` At
//! Mention in a draft and turns it into content on the user Message - a faithful
//! port of qwen's `atCommandProcessor.ts` (`handleAtCommand`), cross-checked
//! against v0.16.0.
//!
//! The pipeline is qwen's, in four stages:
//!   1. [`parse_at_commands`] - PURE. Segment the query into ordered text and
//!      `@path` parts, finding each UNESCAPED `@`; a lone `@` is a text segment.
//!   2. [`resolve`] - IMPURE (stat / ignore). Resolve each `@path` against the
//!      Project Root, SKIP one that escapes it (the confinement predicate is
//!      structured so a temp-dir exception can be added for P5 clipboard
//!      staging), SKIP a git/qwen-ignored path (reported), SKIP a not-found path.
//!      A directory resolves to a spec that gets walked; a file is read directly.
//!   3. [`rebuild_query`] - PURE. Rebuild the residual text: a resolved `@path`
//!      re-emitted as `@resolvedSpec` with qwen's spacing, an unresolved one put
//!      back verbatim, text verbatim.
//!   4. read + assemble - IMPURE. Read every resolved spec via
//!      [`super::read_many_files`] and assemble the user turn as the residual text
//!      block FIRST, then the file content blocks (qwen's `processedQueryParts =
//!      [{text: initialQueryText}, ...parts]`).
//!
//! The product is a [`UserPrompt`] (first-class user content - text + media,
//! never a Tool Result) plus a [`ReadDisplay`] describing what was pulled in for
//! the transcript. When the draft has no At Mention the caller takes a fast path
//! and never calls this (no IO); [`expand`] itself still handles the no-mention
//! and nothing-resolved cases faithfully.

use std::path::{Path, PathBuf};

use crate::content::{ContentBlock, Modalities, UserPrompt};
use crate::tool::path::PathReject;

use super::read_many_files::{self, Spec};

/// One parsed segment of a draft (qwen's `AtCommandPart`): literal text, or an
/// `@path` mention (the `content` INCLUDES the leading `@`, matching qwen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    Text(String),
    AtPath(String),
}

/// Whether a draft contains an At Mention at all - the fast-path gate the submit
/// adapter checks before spawning At Expansion. An UNESCAPED `@` (qwen's
/// `query[i] === '@' && query[i-1] !== '\\'`) is a mention; a lone `@` counts
/// (it parses to an `@path` part that is treated as text), so the gate is simply
/// "is there an unescaped `@`". A draft with none takes the zero-IO text path.
pub(crate) fn has_at_mention(query: &str) -> bool {
    let bytes = query.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'@' && (i == 0 || bytes[i - 1] != b'\\'))
}

/// Parse a draft into ordered text / `@path` parts (qwen's `parseAllAtCommands`).
/// Finds each UNESCAPED `@`; a path ends at the first unescaped whitespace or
/// punctuation, with the same `.` special-case (a `.` terminates only when
/// followed by whitespace or end-of-string, so `file.txt` stays whole but
/// `file.txt.` at a sentence end drops the trailing dot). Each `@path`'s content
/// is `unescapePath`'d (backslash-escapes stripped). Empty/whitespace-only text
/// parts are filtered out, matching qwen.
pub(crate) fn parse_at_commands(query: &str) -> Vec<Part> {
    let chars: Vec<char> = query.chars().collect();
    let mut parts: Vec<Part> = Vec::new();
    let mut current = 0usize;

    while current < chars.len() {
        // Find the next unescaped '@' at or after `current`.
        let at_index =
            (current..chars.len()).find(|&i| chars[i] == '@' && (i == 0 || chars[i - 1] != '\\'));

        let Some(at_index) = at_index else {
            // No more '@': the rest is text.
            if current < chars.len() {
                parts.push(Part::Text(chars[current..].iter().collect()));
            }
            break;
        };

        // Text before the '@'.
        if at_index > current {
            parts.push(Part::Text(chars[current..at_index].iter().collect()));
        }

        // Scan the path body from just after '@'.
        let mut end = at_index + 1;
        let mut in_escape = false;
        while end < chars.len() {
            let c = chars[end];
            if in_escape {
                in_escape = false;
            } else if c == '\\' {
                in_escape = true;
            } else if is_path_terminator(c) {
                break;
            } else if c == '.' {
                // A '.' terminates only when followed by whitespace or EOS, so a
                // file extension stays part of the path but a sentence-ending dot
                // does not (qwen's fileUtils special-case).
                let next = chars.get(end + 1).copied();
                if next.is_none_or(char::is_whitespace) {
                    break;
                }
            }
            end += 1;
        }

        let raw_at_path: String = chars[at_index..end].iter().collect();
        // `unescapePath` strips backslash shell-escapes; the '@' is preserved.
        // suspenders' `unescape_and_trim` also trims, but the parser already
        // broke the path on whitespace, so there is nothing to trim.
        let at_path = crate::tool::path::unescape_and_trim(&raw_at_path);
        parts.push(Part::AtPath(at_path));
        current = end;
    }

    parts
        .into_iter()
        .filter(|p| !matches!(p, Part::Text(t) if t.trim().is_empty()))
        .collect()
}

/// The path-body terminators qwen breaks a `@path` on: whitespace and the
/// `,;!?()[]{}` punctuation set (the `.` case is handled separately by the
/// caller). Matches qwen's `/[,\s;!?()[\]{}]/`.
fn is_path_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}')
}

/// The shortest length qwen infers a paste as a drag/drop path (text-buffer's
/// `minLengthToInferAsDragDrop`): a paste under this is always literal text (a
/// 1-2 char path is not a plausible file drop).
const MIN_LEN_TO_INFER_AS_DRAG_DROP: usize = 3;

/// Rewrite a bracketed-paste payload to an At Mention when it is a dragged/pasted
/// file path (ADR-0068 P4), a faithful port of qwen text-buffer's `insert(ch,
/// {paste})` path detection (`tryExtractFilePaths` + `isValidPath`), scoped to the
/// SINGLE-path case: a terminal delivers a file drop as pasted (often
/// quoted/escaped) path text, and qwen rewrites a valid one to `@<path> ` in place.
///
/// Returns `Some(rewritten)` - the `@<escaped-path> ` (trailing space, matching
/// qwen) - when the payload, trimmed and [`unescape_and_trim`]'d, is a SINGLE
/// existing file; `None` (insert the payload literally) otherwise. The predicate,
/// verbatim from qwen:
///   * a payload shorter than [`MIN_LEN_TO_INFER_AS_DRAG_DROP`] is never a drop;
///   * a MULTI-LINE payload (more than one non-blank line after CRLF normalizing)
///     is literal - a single dropped path is one line;
///   * the single line is stripped of one layer of surrounding `'…'` quotes (a
///     drag/drop often quotes the path), then [`unescape_and_trim`]'d;
///   * `is_file` (qwen's `isValidPath`: exists AND is a regular file) decides -
///     a directory or a non-existent path is literal.
///
/// `root` relativizes the mention so At Expansion (P3) resolves it: a path inside
/// the Project Root is emitted root-relative (matching how `@path` completion
/// inserts, `file_search::relative_display`); a path outside stays verbatim (qwen
/// inserts the pasted path as-is - At Expansion then skips an out-of-root mention).
/// `is_file` is injected (qwen injects `isValidPath`) so the rewrite is PURE and
/// testable without touching disk; the `ui.rs` edge supplies the real `fs` check.
pub(crate) fn rewrite_paste(
    payload: &str,
    root: &Path,
    is_file: impl Fn(&Path) -> bool,
) -> Option<String> {
    if payload.len() < MIN_LEN_TO_INFER_AS_DRAG_DROP {
        return None;
    }
    // Normalize CRLF/CR to LF and keep only non-blank lines (qwen's
    // `tryExtractFilePaths` line filter). A single dropped path is exactly one.
    let normalized = payload.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().filter(|l| !l.trim().is_empty());
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return None;
    };

    // A drag/drop often wraps the path in single quotes; strip one layer (qwen's
    // `/^'(.*)'$/` unquote) before un-escaping the shell escapes.
    let trimmed = line.trim();
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(trimmed);
    let unescaped = crate::tool::path::unescape_and_trim(unquoted);
    if unescaped.is_empty() {
        return None;
    }

    // qwen's `isValidPath`: exists AND is a regular file. A directory / missing
    // path falls through to a literal paste.
    if !is_file(Path::new(&unescaped)) {
        return None;
    }

    // Relativize to the Project Root so At Expansion resolves it (P3 resolves a
    // mention against the root); a path outside the root stays verbatim (qwen
    // inserts the dropped path as-is). Then re-escape spaces so the AT scan
    // round-trips (qwen's `escapePath`), matching `@path` completion's insert.
    let mention = relativize(&unescaped, root);
    Some(format!("@{} ", escape_path_spaces(&mention)))
}

/// The mention text for an unescaped dropped path: root-relative (`/`-separated)
/// when it sits inside the Project Root - the form At Expansion resolves and the
/// form `@path` completion inserts - else the path verbatim (qwen inserts the
/// dropped path unchanged; At Expansion's confinement then skips an out-of-root
/// one).
fn relativize(unescaped: &str, root: &Path) -> String {
    Path::new(unescaped)
        .strip_prefix(root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| unescaped.to_string())
}

/// Re-escapes the spaces in a mention value with backslashes (qwen `escapePath`,
/// narrowed to the space the AT scan round-trips on - the same narrowing
/// `ui::file_search::escape_path` applies to a completion row). Keeps the
/// composer's `@path` detection treating `\ ` as one pattern.
fn escape_path_spaces(path: &str) -> String {
    path.replace(' ', "\\ ")
}

/// Why an At Mention was skipped (qwen's `onDebugMessage` skip reasons), reported
/// on the [`ReadDisplay`] so the operator sees what was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skip {
    /// A RELATIVE `@path` climbed out of the Project Root (BUG 1: only a relative
    /// mention is root-confined; an absolute mention the user typed is honored
    /// as-is, so this reason no longer covers a user's own out-of-root file).
    OutsideWorkspace,
    /// The path matched `.gitignore` / `.qwenignore`.
    Ignored(IgnoreReason),
    /// The path did not exist (qwen's ENOENT arm).
    NotFound,
}

/// Which ignore file matched (qwen's `git` / `qwen` / `both`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreReason {
    Git,
    Qwen,
    Both,
}

/// The outcome of the resolve pass: the specs to read (files + directories, in
/// draft order), a map from each ORIGINAL `@path` (including the `@`) to its
/// resolved spec label for the query rebuild, and the skipped mentions (with
/// reasons) for the display.
#[derive(Debug, Clone, Default)]
struct ResolvedMentions {
    specs: Vec<Spec>,
    /// `(original_at_path, resolved_spec_label)` for the query rebuild.
    resolved: Vec<(String, String)>,
    skipped: Vec<(String, Skip)>,
}

impl ResolvedMentions {
    /// The resolved spec label for an original `@path`, if it resolved.
    fn resolved_spec(&self, original: &str) -> Option<&str> {
        self.resolved
            .iter()
            .find(|(orig, _)| orig == original)
            .map(|(_, spec)| spec.as_str())
    }
}

/// A description of what At Expansion pulled in, for the transcript (qwen's
/// per-file "Read File" / "Read Directory" tool-call cards, rendered here as
/// honest info lines - At Expansion produces USER content, not a Tool Call, so
/// the read display is informational, never a fabricated tool_result).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReadDisplay {
    /// The mentions read, each `(label, is_directory)` (qwen's `filesRead`).
    pub read: Vec<(String, bool)>,
    /// The mentions skipped, each `(label, reason)`.
    pub skipped: Vec<(String, Skip)>,
    /// The per-spec read errors, each `(label, message)`.
    pub errors: Vec<(String, String)>,
}

impl ReadDisplay {
    /// Whether there is anything to show (a read, a skip, or an error).
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.skipped.is_empty() && self.errors.is_empty()
    }
}

/// The confinement predicate for an At Mention (ADR-0068, BUG 1). An At Mention
/// is USER input, not a model tool call, so a user who types their OWN absolute
/// path is honored regardless of the Project Root - the Project-Root confinement
/// exists to bound the MODEL's tools, not the user's explicit mentions (a
/// DELIBERATE DIVERGENCE from qwen, which skips out-of-workspace `@paths`; the
/// user complained about the skip, so we honor their mention instead - they could
/// paste the file's contents themselves anyway). The rule:
///
///   * an ABSOLUTE `@path` resolves AS-IS (lexically normalized), honored no
///     matter where it sits. A non-existent absolute path still falls through to
///     the not-found skip downstream (the `stat` in [`resolve`]).
///   * a RELATIVE `@path` still resolves against the Project Root (the `@path`
///     completion convention) and is refused if it climbs out - the same boundary
///     read_file's tools use, kept for the model-facing completion form.
///
/// The `temp_dir` (P5) exception is preserved for the clipboard staging path,
/// though an absolute clipboard mention now also passes via the absolute arm;
/// keeping the exception documents the intent and costs nothing. Returns the
/// resolved absolute path, or the [`PathReject`] the caller maps to a skip.
fn confine(
    path_name: &str,
    root: &Path,
    memory: Option<&Path>,
    temp_dir: Option<&Path>,
) -> Result<PathBuf, PathReject> {
    // An ABSOLUTE `@path` the user typed is honored as-is (BUG 1): normalize it
    // lexically (`..`/`.` collapse, no FS touch) and accept it wherever it points.
    // A non-existent one is still caught by the not-found stat in `resolve`.
    let candidate = Path::new(path_name);
    if candidate.is_absolute() {
        return Ok(crate::tool::path::normalize_lexical(candidate));
    }

    // P5 exception (preserved): a RELATIVE `@path` that resolves under the global
    // clipboard temp dir passes even though it is outside the Project Root - the
    // clipboard Attachment staging path. Uses the SAME lexical-containment
    // discipline as the Project-Root check.
    if let Some(temp) = temp_dir
        && let Ok(abs) = crate::tool::path::resolve_path_in(path_name, temp, None)
    {
        return Ok(abs);
    }

    // A RELATIVE mention resolves against the Project Root (qwen's
    // `path.resolve(dir, pathName)`) and is refused if it climbs out (the same
    // boundary read_file's tools use).
    crate::tool::path::resolve_path_in(path_name, root, memory).map_err(|_| PathReject::Escapes)
}

/// Resolve each `@path` part against the Project Root (qwen's `handleAtCommand`
/// resolve loop): skip a lone `@`, a path outside the workspace, a git/qwen
/// ignored path (reported), and a not-found path; stat a resolved one to learn
/// whether it is a directory (walked) or a file (read directly).
fn resolve(
    parts: &[Part],
    root: &Path,
    memory: Option<&Path>,
    temp_dir: Option<&Path>,
) -> ResolvedMentions {
    let mut out = ResolvedMentions::default();
    for part in parts {
        let Part::AtPath(original) = part else {
            continue;
        };
        // A lone '@' is treated as text (put back verbatim in the rebuild).
        if original == "@" {
            continue;
        }
        let path_name = original.strip_prefix('@').unwrap_or(original).to_string();

        // Confinement first (qwen's workspace check, plus the P5 temp-dir
        // exception for a staged clipboard image).
        let Ok(abs) = confine(&path_name, root, memory, temp_dir) else {
            out.skipped
                .push((path_name.clone(), Skip::OutsideWorkspace));
            continue;
        };

        // Ignore filtering (qwen's gitIgnored / qwenIgnored). Directory-ness is
        // not known yet; the point queries treat the path as a file, which is
        // what matters for a mention (a directory pattern like `build/` still
        // matches via `matched_path_or_any_parents`).
        if let Some(reason) = ignore_reason(root, &abs) {
            out.skipped.push((path_name.clone(), Skip::Ignored(reason)));
            continue;
        }

        // Stat to resolve file vs directory vs not-found (qwen's `fs.stat`).
        match std::fs::metadata(&abs) {
            Ok(meta) => {
                let is_dir = meta.is_dir();
                out.specs.push(Spec {
                    abs,
                    is_dir,
                    display: path_name.clone(),
                });
                // qwen's `atPathToResolvedSpecMap`: the resolved spec is the
                // `pathName` itself (the relative label), re-emitted as
                // `@pathName` in the rebuilt query.
                out.resolved.push((original.clone(), path_name));
            }
            Err(_) => {
                out.skipped.push((path_name, Skip::NotFound));
            }
        }
    }
    out
}

/// Which ignore file (if any) matches a resolved path (qwen's gitIgnored /
/// qwenIgnored / both). The point queries answer `.gitignore` and `.qwenignore`
/// against the Project Root.
fn ignore_reason(root: &Path, abs: &Path) -> Option<IgnoreReason> {
    let git = crate::walk::gitignore::is_ignored(root, abs, false);
    let qwen = crate::walk::qwenignore::is_ignored(root, abs);
    match (git, qwen) {
        (true, true) => Some(IgnoreReason::Both),
        (true, false) => Some(IgnoreReason::Git),
        (false, true) => Some(IgnoreReason::Qwen),
        (false, false) => None,
    }
}

/// Rebuild the residual query text (qwen's `initialQueryText` loop): text parts
/// verbatim, a resolved `@path` re-emitted as `@resolvedSpec` and an unresolved
/// one put back verbatim, with qwen's spacing rules, then trimmed. `resolution`
/// supplies the resolved-spec lookup.
fn rebuild_query(parts: &[Part], resolution: &ResolvedMentions) -> String {
    let mut text = String::new();
    for (i, part) in parts.iter().enumerate() {
        match part {
            Part::Text(t) => text.push_str(t),
            Part::AtPath(content) => {
                let resolved = resolution.resolved_spec(content);
                // qwen: add a space if the previous part was text (not ending in
                // a space) or a resolved @path, so adjacent parts do not run
                // together.
                if i > 0 && !text.is_empty() && !text.ends_with(' ') {
                    let prev = &parts[i - 1];
                    let prev_glues = matches!(prev, Part::Text(_))
                        || matches!(prev, Part::AtPath(p) if resolution.resolved_spec(p).is_some());
                    if prev_glues {
                        text.push(' ');
                    }
                }
                if let Some(spec) = resolved {
                    text.push('@');
                    text.push_str(spec);
                } else {
                    // Unresolved (lone @ / skipped path): put the original back,
                    // ensuring a separating space if it is not the first element.
                    if i > 0
                        && !text.is_empty()
                        && !text.ends_with(' ')
                        && !content.starts_with(' ')
                    {
                        text.push(' ');
                    }
                    text.push_str(content);
                }
            }
        }
    }
    text.trim().to_string()
}

/// Run At Expansion over `query` (ADR-0068): parse the At Mentions, resolve them
/// against `root` (confined; ignored / not-found / outside-workspace skipped),
/// rebuild the residual text, read every resolved spec via
/// [`super::read_many_files`], and assemble the [`UserPrompt`] as the residual
/// text block FIRST then the file content blocks. `memory` is the trusted memory
/// subtree the shared confinement honors; `temp_dir` is the global clipboard temp
/// dir the P5 confinement exception admits (so a staged clipboard image's temp
/// file, outside the Project Root, resolves); `modalities` gates media on the
/// captured Model. Returns the prompt and a [`ReadDisplay`] for the transcript.
///
/// Faithful to qwen's fallbacks: no At Mention -> the original query as one text
/// block; nothing resolved -> the (possibly modified) residual text, no file read.
pub(crate) async fn expand(
    query: &str,
    root: &Path,
    memory: Option<&Path>,
    temp_dir: Option<&Path>,
    modalities: Modalities,
) -> (UserPrompt, ReadDisplay) {
    let parts = parse_at_commands(query);
    let has_at_path = parts.iter().any(|p| matches!(p, Part::AtPath(_)));

    // No At Mention at all: the original query verbatim (qwen's early return).
    if !has_at_path {
        return (UserPrompt::from(query.to_string()), ReadDisplay::default());
    }

    let resolution = resolve(&parts, root, memory, temp_dir);
    let residual = rebuild_query(&parts, &resolution);

    // Nothing resolved to read: proceed with the (possibly modified) residual
    // text, no file read (qwen's `pathSpecsToRead.length === 0` fallback). A
    // lone-@ / all-invalid draft that rebuilt to nothing falls back to the
    // original query, matching qwen.
    if resolution.specs.is_empty() {
        let text = if residual.is_empty() {
            query.to_string()
        } else {
            residual
        };
        let display = ReadDisplay {
            read: Vec::new(),
            skipped: resolution.skipped,
            errors: Vec::new(),
        };
        return (UserPrompt::from(text), display);
    }

    // Read every resolved spec (qwen's `readManyFiles`), then assemble: the
    // residual text block first, then the file content blocks in spec order.
    let batch = read_many_files::read(&resolution.specs, root, modalities).await;

    let display = build_display(&resolution, &batch);

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(batch.blocks.len() + 1);
    blocks.push(ContentBlock::text(residual));
    blocks.extend(batch.blocks);

    (UserPrompt::from_blocks(blocks), display)
}

/// Build the read display from the resolution and the batch outcome (qwen's
/// per-file cards + ignored-paths report): the specs read (label + is_directory),
/// the skipped mentions (with reasons), and the per-spec read errors.
fn build_display(resolution: &ResolvedMentions, batch: &read_many_files::BatchRead) -> ReadDisplay {
    let read = batch
        .reads
        .iter()
        .filter(|r| r.error.is_none())
        .map(|r| (r.display.clone(), r.is_dir))
        .collect();
    let errors = batch
        .reads
        .iter()
        .filter_map(|r| r.error.clone().map(|e| (r.display.clone(), e)))
        .collect();
    ReadDisplay {
        read,
        skipped: resolution.skipped.clone(),
        errors,
    }
}

#[cfg(test)]
#[path = "../../tests/tools/at_expansion.rs"]
mod tests;
