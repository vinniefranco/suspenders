//! `edit_file(file_path, old_string, new_string, replace_all?)`: replaces text
//! within a file - the faithful port of qwen v0.16.0
//! `packages/core/src/tools/edit.ts` (`EditTool` / `EditToolInvocation`) plus
//! its `editHelper.ts` normalization pipeline, narrowed to Suspenders' shape.
//!
//! Matching tries the exact literal `old_string` first, then falls back to
//! qwen's `normalizeEditStrings` pipeline ([`normalize`]): character-level
//! equivalence (curly quotes to straight, several dash variants to `-`, exotic
//! spaces to a normal space) and line-based matching that tolerates trailing
//! whitespace. When a relaxed match is found, the CANONICAL on-disk slice is
//! substituted back as the string to replace, so the write lands on real bytes.
//! For a pure deletion, `maybeAugmentOldStringForDeletion` consumes a trailing
//! newline so removing a whole line leaves no blank line behind.
//!
//! Occurrence counting is then EXACT on that (possibly canonicalized) string:
//! by default a single occurrence is replaced; more than one with `replace_all`
//! unset is refused with the count. An empty `old_string` CREATES a new file
//! (`isNewFile`); a non-empty `old_string` that matches zero times, that equals
//! `new_string`, or whose replacement leaves the content unchanged, is refused.
//! Before editing an existing file the model must have read it this session
//! (prior-read enforcement against the Run's file-read cache); a create does
//! not require a prior read.
//!
//! qwen's edit.ts also carries internal infrastructure this port does not
//! reproduce (out of the model-facing contract): BOM / encoding / line-ending
//! detection, git commit attribution, file-history backup, and hooks. The Jaro
//! closest-region QUOTING that an earlier Suspenders build added to the
//! not-found error was a bespoke extension and is removed - qwen's not-found
//! message carries no file excerpt. What DOES survive is the read-cache
//! recording: after a successful edit the Run's [`FileReadCache`] is stamped
//! (qwen's `recordWrite`) so a follow-up read_file/edit_file sees fresh content
//! rather than the tool's own write as a stale external change.
//!
//! [`FileReadCache`]: crate::tool::read_cache::FileReadCache

use crate::tool::path::{
    FileError, PathReject, file_error, resolve_absolute_in, unescape_and_trim,
};
use crate::tool::read_cache::ReadState;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct EditFile;

/// VERBATIM from qwen v0.16.0 `tools/edit.ts` (the description passed to the
/// `EditTool` constructor). `${ReadFileTool.Name}` is hardcoded to `read_file`.
/// The second line's leading indentation is preserved exactly as in qwen.
const DESCRIPTION: &str = "Replaces text within a file. By default, replaces a single occurrence. Set `replace_all` to true when you intend to modify every instance of `old_string`. This tool requires providing significant context around the change to ensure precise targeting. Always use the read_file tool to examine the file's current content before attempting a text replacement.

      The user has the ability to modify the `new_string` content. If modified, this will be stated in the response.

Expectation for required parameters:
1. `file_path` MUST be an absolute path; otherwise an error will be thrown.
2. `old_string` MUST be the exact literal text to replace (including all whitespace, indentation, newlines, and surrounding code etc.).
3. `new_string` MUST be the exact literal text to replace `old_string` with (also including all whitespace, indentation, newlines, and surrounding code etc.). Ensure the resulting code is correct and idiomatic.
4. NEVER escape `old_string` or `new_string`, that would break the exact literal text requirement.
**Important:** If ANY of the above are not satisfied, the tool will fail. CRITICAL for `old_string`: Must uniquely identify the single instance to change. Include at least 3 lines of context BEFORE and AFTER the target text, matching whitespace and indentation precisely. If this string matches multiple locations, or does not match exactly, the tool will fail.
**Multiple replacements:** Set `replace_all` to true when you want to replace every occurrence that matches `old_string`.";

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: DESCRIPTION.into(),
            // Schema property set + each description string are VERBATIM from
            // qwen edit.ts's schema block.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to modify. Must start with '/'."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact literal text to replace, preferably unescaped. For single replacements (default), include at least 3 lines of context BEFORE and AFTER the target text, matching whitespace and indentation precisely. If this string is not the exact literal text (i.e. you escaped it) or does not match exactly, the tool will fail."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The exact literal text to replace `old_string` with, preferably unescaped. Provide the EXACT text. Ensure the resulting code is correct and idiomatic."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences of old_string (default false)."
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // The text projection: the model-facing message, dropping the display
        // Artifact. `run_rich` (the Registry's dispatch path) keeps the diff.
        edit(input, ctx).map(|outcome| outcome.message)
    }

    async fn run_rich(
        &self,
        input: &Value,
        ctx: &ToolCtx,
    ) -> Result<crate::tool::ToolOutput, String> {
        // The file-editing tool knows the before/after content, so it computes
        // its own diff (ADR-0007's diff behavior, relocated here) and attaches
        // the `diff` display Artifact, which the Transcript store swaps for a
        // first-class Diff item.
        let outcome = edit(input, ctx)?;
        let output = crate::tool::ToolOutput::text(outcome.message);
        Ok(match outcome.diff {
            Some(diff) => output.with_artifact(crate::tools::file_diff::DIFF, diff),
            None => output,
        })
    }
}

/// One edit's outcome: the model-facing message and the optional `diff` display
/// Artifact (a serialized [`crate::tools::file_diff::DiffArtifact`]). `run`
/// projects the message; `run_rich` also attaches the diff.
struct EditOutcome {
    message: String,
    diff: Option<Value>,
}

/// Decodes the input, resolves the path, and applies the edit, returning the
/// message and the diff Artifact. Shared by `run` and `run_rich`.
fn edit(input: &Value, ctx: &ToolCtx) -> Result<EditOutcome, String> {
    let raw_file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: edit requires a string \"file_path\"".to_string())?;
    // qwen's `unescapePath(params.file_path.trim())` (edit.ts): trim
    // surrounding whitespace and strip shell-escaping backslashes BEFORE the
    // absolute-path check and before the path is echoed back in a message.
    let file_path = unescape_and_trim(raw_file_path);
    let old_string = input
        .get("old_string")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: edit requires a string \"old_string\"".to_string())?;
    let new_string = input
        .get("new_string")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: edit requires a string \"new_string\"".to_string())?;
    // `replace_all` is optional; a missing / non-boolean value defaults to
    // false (qwen `params.replace_all ?? false`).
    let replace_all = input
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let abs = resolve(&file_path, ctx)?;
    apply_edit(
        EditPlan {
            abs: &abs,
            file_path: &file_path,
            diff_path: raw_file_path,
            old_string,
            new_string,
            replace_all,
        },
        ctx,
    )
}

/// The cohesive inputs one edit needs (Parameter Object): the resolved absolute
/// path `abs`, the model-supplied `file_path` (echoed in messages), the
/// `old_string`/`new_string` to swap, and the `replace_all` flag. A recurring
/// data clump made a first-class type so `apply_edit` takes the edit as one
/// argument plus the `ctx` it acts through.
struct EditPlan<'a> {
    abs: &'a std::path::Path,
    file_path: &'a str,
    /// The RAW model `file_path` input, echoed verbatim in the diff title (the
    /// path the display carries), distinct from the unescaped `file_path` the
    /// messages echo.
    diff_path: &'a str,
    old_string: &'a str,
    new_string: &'a str,
    replace_all: bool,
}

/// Resolve a model-supplied ABSOLUTE `file_path` and confine it to the Project
/// Root (or the trusted memory subtree), rendering qwen's verbatim
/// absolute-path message for a relative path (edit.ts `validateToolParamValues`)
/// and the shared confinement wording for an escape.
fn resolve(path: &str, ctx: &ToolCtx) -> Result<std::path::PathBuf, String> {
    resolve_absolute_in(path, &ctx.root, ctx.memory_root.as_deref()).map_err(
        |reject| match reject {
            // VERBATIM qwen edit.ts `validateToolParamValues`.
            PathReject::Relative => format!("File path must be absolute: {path}"),
            // qwen has no edit.ts message for a path outside the workspace (it asks
            // for confirmation via getDefaultPermission instead). Suspenders
            // confines every tool path to the Project Root, so an escape is a hard
            // refusal with the shared confinement wording.
            PathReject::Escapes => "path escapes project root".to_string(),
        },
    )
}

/// The core of qwen's `calculateEdit` + `execute`, narrowed to the model
/// contract: read the file (or note its absence), decide the edit outcome,
/// enforce prior-read for an in-place edit, apply, write, and record.
fn apply_edit(plan: EditPlan<'_>, ctx: &ToolCtx) -> Result<EditOutcome, String> {
    let EditPlan {
        abs,
        file_path,
        diff_path,
        old_string,
        new_string,
        replace_all,
    } = plan;
    let current = read_current(abs, file_path)?;
    let is_new_file = old_string.is_empty() && current.is_none();

    let new_content = match &current {
        // ---- new-file creation (empty old_string, file absent) ----
        None if old_string.is_empty() => new_string.to_string(),

        // ---- editing a nonexistent file (old_string non-empty) ----
        None => {
            return Err(format!("File not found: {file_path}"));
        }

        // ---- in-place edit of an existing file ----
        Some(content) => {
            // Prior-read enforcement: the model must have read this file this
            // session before an in-place edit (qwen `checkPriorRead`). A create
            // (handled above) does not reach here, matching qwen's ENOENT
            // ok:true pre-read path.
            enforce_prior_read(ctx, abs, file_path)?;

            edited_content(content, old_string, new_string, replace_all, file_path)?
        }
    };

    match std::fs::write(abs, &new_content) {
        Ok(()) => {
            // Record the write into the Run's read cache (qwen `recordWrite`,
            // `cacheable = true` - an edit authored plain text). A follow-up
            // read_file/edit_file then sees a Fresh, full read rather than the
            // tool's own write as a stale external change.
            record_write(ctx, abs);
            // The diff Artifact from the exact before/after content the edit had
            // in hand: an in-place edit diffs old->new; a fresh create (no
            // `current`) is one all-added created-file diff.
            let diff =
                crate::tools::file_diff::artifact(current.as_deref(), &new_content, diff_path);
            Ok(EditOutcome {
                message: success_message(file_path, is_new_file, current.as_deref(), &new_content),
                diff,
            })
        }
        Err(err) => Err(file_error("write", file_path, FileError::from_io(&err))),
    }
}

/// Read the file's current content, or `None` when it does not exist (qwen's
/// `fileExists` + ENOENT-tolerant read). Any other I/O error is surfaced.
fn read_current(abs: &std::path::Path, file_path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(abs) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(file_error("read", file_path, FileError::from_io(&err))),
    }
}

/// Compute the new content of an existing file (qwen's `calculateEdit` content
/// arm + `applyReplacement`), in qwen's exact order:
///
/// 1. `normalize::edit_strings` - if the literal `old_string` is not present,
///    retry with the character- and line-level relaxed matcher and, on a hit,
///    substitute the CANONICAL on-disk slice back as the string to replace (and
///    trim `new_string`'s trailing newline if the relaxed match dropped a final
///    empty line);
/// 2. `normalize::augment_old_string_for_deletion` - for a pure deletion, extend
///    the string to replace by a trailing newline when the file has one, so the
///    removal leaves no blank line;
/// 3. EXACT `countOccurrences` on the (possibly canonicalized) string:
///    - `old_string` empty and the file exists -> attempted to create an
///      existing file (checks the ORIGINAL `old_string`, per qwen);
///    - 0 occurrences -> not found;
///    - >1 occurrences and `!replace_all` -> occurrence mismatch;
///    - final old == final new -> no change (old/new identical);
/// 4. apply the replacement, then a SECONDARY no-change guard: if the content is
///    unchanged (e.g. the normalized replacement was a no-op), refuse.
fn edited_content(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    file_path: &str,
) -> Result<String, String> {
    // (1) Relaxed-match canonicalization (qwen `normalizeEditStrings`).
    let normalize::Normalized {
        old_string: mut final_old,
        new_string: final_new,
    } = normalize::edit_strings(content, old_string, new_string);

    if old_string.is_empty() {
        // qwen `ATTEMPT_TO_CREATE_EXISTING_FILE` - keyed on the ORIGINAL
        // `old_string` being empty, before augmentation could grow it.
        return Err(format!("File already exists, cannot create: {file_path}"));
    }

    // (2) Deletion newline augmentation (qwen `maybeAugmentOldStringForDeletion`).
    final_old = normalize::augment_old_string_for_deletion(content, &final_old, &final_new);

    // (3) EXACT occurrence counting on the canonicalized string.
    let occurrences = count_occurrences(content, &final_old);
    if occurrences == 0 {
        // qwen `EDIT_NO_OCCURRENCE_FOUND` raw message.
        return Err(format!(
            "Failed to edit, 0 occurrences found for old_string in {file_path}. No edits made. The exact text in old_string was not found. Ensure you're not escaping content incorrectly and check whitespace, indentation, and context. Use read_file tool to verify."
        ));
    }
    if !replace_all && occurrences > 1 {
        // qwen `EDIT_EXPECTED_OCCURRENCE_MISMATCH` raw message.
        return Err(format!(
            "Failed to edit. Found {occurrences} occurrences for old_string in {file_path} but replace_all was not enabled."
        ));
    }
    if final_old == final_new {
        // qwen `EDIT_NO_CHANGE` raw message (old/new identical).
        return Err(format!(
            "No changes to apply. The old_string and new_string are identical in file: {file_path}"
        ));
    }

    // qwen's `applyReplacement` always replaces EVERY occurrence of the (by now
    // unique-or-replace_all) string via `safeLiteralReplace`; the `$`-escape it
    // does is a JS `String.replaceAll` template-substitution guard that Rust's
    // literal `str::replace` does not need.
    let new_content = content.replace(&final_old, &final_new);

    // (4) Secondary no-change guard (qwen edit.ts's post-apply check): a
    // normalized replacement can leave the file byte-identical even when
    // old != new; refuse with qwen's VERBATIM identical-content message.
    if new_content == content {
        return Err(format!(
            "No changes to apply. The new content is identical to the current content in file: {file_path}"
        ));
    }

    Ok(new_content)
}

/// Number of non-overlapping occurrences of `old_string` in `content` (qwen
/// `countOccurrences`).
fn count_occurrences(content: &str, old_string: &str) -> usize {
    if old_string.is_empty() {
        return 0;
    }
    content.matches(old_string).count()
}

/// The model-facing success line (qwen `llmSuccessMessageParts`): the base
/// create/update line, then the edited-region snippet suffix
/// (`extractEditSnippet`) joined with a single space.
fn success_message(
    file_path: &str,
    is_new_file: bool,
    old_content: Option<&str>,
    new_content: &str,
) -> String {
    let base = if is_new_file {
        format!("Created new file: {file_path} with provided content.")
    } else {
        format!("The file: {file_path} has been updated.")
    };
    match snippet::extract(old_content, new_content) {
        Some(snippet) => format!(
            "{base} Showing lines {}-{} of {} from the edited file:\n\n---\n\n{}",
            snippet.start_line, snippet.end_line, snippet.total_lines, snippet.content
        ),
        None => base,
    }
}

// ---- prior-read enforcement (qwen `checkPriorRead`, verb = editing) ---------

/// Enforce the read-before-edit contract for an in-place edit: the file must be
/// a Fresh cache entry the model has read this session. Unlike notebook_edit, an
/// edit does NOT require a FULL read (any prior read of the current bytes
/// clears it, matching qwen's `checkPriorRead` for `editing`); the mtime/size
/// drift check is the safety net. Returns the VERBATIM qwen rejection otherwise.
fn enforce_prior_read(ctx: &ToolCtx, abs: &std::path::Path, file_path: &str) -> Result<(), String> {
    let (mtime_ms, size) = stat_fingerprint(abs, file_path)?;
    let cache = ctx.read_cache();
    let state = cache.check(abs, mtime_ms, size);
    let read_this_session = cache
        .entry(abs)
        .is_some_and(|entry| entry.last_read_at.is_some());

    // Fresh AND read this session (not a write-only entry): the edit proceeds.
    if state == ReadState::Fresh && read_this_session {
        return Ok(());
    }

    // Read this session but the on-disk fingerprint drifted: re-read first.
    if state == ReadState::Stale {
        return Err(format!(
            "File {file_path} has been modified since you last read it (mtime or size changed). Re-read it with the read_file tool before editing it to ensure your changes are based on current content."
        ));
    }

    // Never read this session (Unknown): not read. The em-dash below is
    // VERBATIM qwen model-facing text (priorReadEnforcement.ts), not authored
    // prose, so the no-em-dash house rule does not apply - it is kept byte-exact
    // as \u{2014}.
    Err(format!(
        "File {file_path} has not been read in this session. Use the read_file tool first to load the current content (a partial read with offset / limit is fine \u{2014} you only need to have seen the bytes you intend to edit) before editing it."
    ))
}

/// Stat the file for its current `(mtime_ms, size)` fingerprint. A missing stat
/// here means we cannot verify the prior read, so the edit is refused - never
/// silently allowed.
fn stat_fingerprint(abs: &std::path::Path, file_path: &str) -> Result<(u128, u64), String> {
    let meta = std::fs::metadata(abs)
        .map_err(|err| file_error("read", file_path, FileError::from_io(&err)))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok((mtime_ms, meta.len()))
}

/// Record the write back into the read cache (qwen `recordWrite`, `cacheable =
/// true`): the model authored the current bytes, so a follow-up read_file/
/// edit_file sees a Fresh, full read rather than its own write as a stale
/// external change. A stat miss is dropped - the write already succeeded, and
/// the next read re-stats.
fn record_write(ctx: &ToolCtx, abs: &std::path::Path) {
    if let Ok((mtime_ms, size)) = stat_fingerprint(abs, &abs.to_string_lossy()) {
        ctx.read_cache()
            .record_write(abs.to_path_buf(), mtime_ms, size, true);
    }
}

// ---- normalization pipeline (qwen editHelper.ts) ----------------------------

/// The faithful port of qwen v0.16.0 `utils/editHelper.ts`'s edit-string
/// normalization: `normalizeEditStrings` + `maybeAugmentOldStringForDeletion`
/// and the `findMatchedSlice` matcher they lean on.
///
/// The pipeline stays deterministic and progressively relaxes: literal
/// substring, then character-equivalence (curly quotes / dashes / exotic
/// spaces), then line-based matching that tolerates trailing whitespace. A
/// relaxed hit returns the CANONICAL on-disk slice so the caller replaces real
/// bytes, not the model's approximation.
mod normalize {
    /// qwen `normalizeEditStrings`' return: the (possibly canonicalized) strings.
    pub struct Normalized {
        pub old_string: String,
        pub new_string: String,
    }

    /// qwen `NormalizedEditStrings`-producing `normalizeEditStrings`: when the
    /// literal `old_string` is not found verbatim but a relaxed match is,
    /// substitute the on-disk slice as `old_string` and (if the relaxed match
    /// dropped a trailing empty line) trim `new_string`'s trailing newline. An
    /// empty `old_string` is returned untouched (it is the new-file sentinel).
    pub fn edit_strings(file_content: &str, old_string: &str, new_string: &str) -> Normalized {
        if old_string.is_empty() {
            return Normalized {
                old_string: old_string.to_string(),
                new_string: new_string.to_string(),
            };
        }
        match find_matched_slice(file_content, old_string) {
            Some(matched) => Normalized {
                old_string: matched.slice,
                new_string: adjust_new_string_for_trailing_line(
                    new_string,
                    matched.removed_trailing_final_empty_line,
                ),
            },
            None => Normalized {
                old_string: old_string.to_string(),
                new_string: new_string.to_string(),
            },
        }
    }

    /// qwen `maybeAugmentOldStringForDeletion`: for a pure deletion
    /// (`new_string == ""`) where `old_string` has no trailing newline but the
    /// file holds `old_string + "\n"`, grow `old_string` by that newline so the
    /// removal does not leave a blank line behind.
    pub fn augment_old_string_for_deletion(
        file_content: &str,
        old_string: &str,
        new_string: &str,
    ) -> String {
        if old_string.is_empty() || !new_string.is_empty() || old_string.ends_with('\n') {
            return old_string.to_string();
        }
        let candidate = format!("{old_string}\n");
        if file_content.contains(&candidate) {
            candidate
        } else {
            old_string.to_string()
        }
    }

    /// qwen `MatchedSliceResult`.
    struct MatchedSlice {
        slice: String,
        removed_trailing_final_empty_line: bool,
    }

    /// qwen `findMatchedSlice`: literal `indexOf`, then character-normalized
    /// `indexOf`, then `findLineBasedMatch`. Returns the CANONICAL slice from the
    /// ORIGINAL `haystack` in every case.
    fn find_matched_slice(haystack: &str, needle: &str) -> Option<MatchedSlice> {
        if needle.is_empty() {
            return None;
        }

        // Literal match: the on-disk slice IS the needle.
        if let Some(byte_idx) = haystack.find(needle) {
            return Some(MatchedSlice {
                slice: haystack[byte_idx..byte_idx + needle.len()].to_string(),
                removed_trailing_final_empty_line: false,
            });
        }

        // Character-equivalence match: normalize both, find the char index, then
        // cut the ORIGINAL haystack at [idx, idx + needle_char_len). The map is
        // 1:1 on chars, so char indices line up between original and normalized.
        let haystack_chars: Vec<char> = haystack.chars().collect();
        let normalized_haystack: String = haystack_chars
            .iter()
            .map(|c| normalize_basic_char(*c))
            .collect();
        let normalized_needle: String = needle.chars().map(normalize_basic_char).collect();
        if let Some(char_idx) = char_index_of(&normalized_haystack, &normalized_needle) {
            let needle_char_len = needle.chars().count();
            let slice: String = haystack_chars[char_idx..char_idx + needle_char_len]
                .iter()
                .collect();
            return Some(MatchedSlice {
                slice,
                removed_trailing_final_empty_line: false,
            });
        }

        find_line_based_match(haystack, needle)
    }

    /// qwen `findLineBasedMatch`: line-window the haystack against the needle's
    /// lines, trying identity, then `trimEnd`, then `normalizeBasicCharacters +
    /// trimEnd` per line. If the needle's last line is empty and no match is
    /// found, retry with that trailing empty line dropped
    /// (`removedTrailingFinalEmptyLine = true`).
    fn find_line_based_match(haystack: &str, needle: &str) -> Option<MatchedSlice> {
        let index = build_line_index(haystack);
        let pattern_lines: Vec<&str> = needle.split('\n').collect();
        let ends_with_newline = needle.ends_with('\n');
        if pattern_lines.is_empty() {
            return None;
        }

        if let Some(idx) = attempt_match(&index.lines, &pattern_lines) {
            return Some(MatchedSlice {
                slice: slice_from_lines(&index, idx, pattern_lines.len(), ends_with_newline),
                removed_trailing_final_empty_line: false,
            });
        }

        if pattern_lines.last() == Some(&"") {
            let trimmed = &pattern_lines[..pattern_lines.len() - 1];
            if trimmed.is_empty() {
                return None;
            }
            if let Some(idx) = attempt_match(&index.lines, trimmed) {
                return Some(MatchedSlice {
                    slice: slice_from_lines(&index, idx, trimmed.len(), false),
                    removed_trailing_final_empty_line: true,
                });
            }
        }
        None
    }

    /// One line-comparison pass in qwen's `attemptMatch` relaxation ladder: the
    /// identity pass, the `trimEnd` pass, then the `normalizeBasicCharacters +
    /// trimEnd` pass. A first-class enum (rather than a function-pointer table)
    /// so each transform is reached through a plain method call.
    #[derive(Clone, Copy)]
    enum LinePass {
        Identity,
        TrimEnd,
        Normalize,
    }

    impl LinePass {
        /// The three passes in relaxation order.
        const LADDER: [LinePass; 3] = [LinePass::Identity, LinePass::TrimEnd, LinePass::Normalize];

        /// Apply this pass' transform to one line.
        fn apply(self, value: &str) -> String {
            match self {
                LinePass::Identity => value.to_string(),
                LinePass::TrimEnd => trim_end(value).to_string(),
                LinePass::Normalize => normalize_line_for_comparison(value),
            }
        }
    }

    /// qwen's `attemptMatch`: the identity and `trimEnd` passes, then the
    /// character-normalizing + `trimEnd` pass, tried in order.
    fn attempt_match(lines: &[&str], pattern: &[&str]) -> Option<usize> {
        LinePass::LADDER
            .into_iter()
            .find_map(|pass| seek_sequence_with_transform(lines, pattern, pass))
    }

    /// qwen `seekSequenceWithTransform`: first window index where every
    /// pass-transformed pattern line equals the transformed haystack line.
    fn seek_sequence_with_transform(
        lines: &[&str],
        pattern: &[&str],
        pass: LinePass,
    ) -> Option<usize> {
        if pattern.is_empty() {
            return Some(0);
        }
        if pattern.len() > lines.len() {
            return None;
        }
        'outer: for i in 0..=(lines.len() - pattern.len()) {
            for (p, pat) in pattern.iter().enumerate() {
                if pass.apply(lines[i + p]) != pass.apply(pat) {
                    continue 'outer;
                }
            }
            return Some(i);
        }
        None
    }

    /// qwen `buildLineIndex`'s output as a first-class value: the text, its
    /// `split('\n')` lines, and the byte offset each line starts at (`offsets`
    /// has `lines.len() + 1` entries; the last is the content length). Groups the
    /// `(text, lines, offsets)` clump `slice_from_lines` needed as three separate
    /// positional arguments into one Parameter Object.
    struct LineIndex<'a> {
        text: &'a str,
        lines: Vec<&'a str>,
        offsets: Vec<usize>,
    }

    /// qwen `buildLineIndex`.
    fn build_line_index(text: &str) -> LineIndex<'_> {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut offsets = vec![0usize; lines.len() + 1];
        let mut cursor = 0usize;
        for (i, line) in lines.iter().enumerate() {
            offsets[i] = cursor;
            cursor += line.len();
            if i < lines.len() - 1 {
                cursor += 1; // the '\n' that split() removed.
            }
        }
        offsets[lines.len()] = text.len();
        LineIndex {
            text,
            lines,
            offsets,
        }
    }

    /// qwen `sliceFromLines`: reconstruct the original bytes for
    /// `[start_line, start_line + line_count)`, optionally including the newline
    /// after the final line. Takes the [`LineIndex`] Parameter Object so its
    /// `(text, lines, offsets)` data clump travels as one argument.
    fn slice_from_lines(
        index: &LineIndex<'_>,
        start_line: usize,
        line_count: usize,
        include_trailing_newline: bool,
    ) -> String {
        if line_count == 0 {
            return if include_trailing_newline {
                "\n".to_string()
            } else {
                String::new()
            };
        }
        let start_index = index.offsets.get(start_line).copied().unwrap_or(0);
        let last_line_index = start_line + line_count - 1;
        let last_line_start = index.offsets.get(last_line_index).copied().unwrap_or(0);
        let mut end_index =
            last_line_start + index.lines.get(last_line_index).map_or(0, |l| l.len());

        if include_trailing_newline {
            if let Some(next_line_start) = index.offsets.get(start_line + line_count).copied() {
                end_index = next_line_start;
            } else if index.text.ends_with('\n') {
                end_index = index.text.len();
            }
        }
        index.text[start_index..end_index].to_string()
    }

    /// qwen `adjustNewStringForTrailingLine`.
    fn adjust_new_string_for_trailing_line(
        new_string: &str,
        removed_trailing_line: bool,
    ) -> String {
        if removed_trailing_line {
            remove_trailing_newline(new_string).to_string()
        } else {
            new_string.to_string()
        }
    }

    /// qwen `removeTrailingNewline`: strip a single trailing CRLF, LF, or CR.
    fn remove_trailing_newline(text: &str) -> &str {
        if let Some(stripped) = text.strip_suffix("\r\n") {
            stripped
        } else if let Some(stripped) = text.strip_suffix('\n') {
            stripped
        } else if let Some(stripped) = text.strip_suffix('\r') {
            stripped
        } else {
            text
        }
    }

    /// qwen `normalizeLineForComparison`: `normalizeBasicCharacters` then
    /// `trimEnd`.
    fn normalize_line_for_comparison(value: &str) -> String {
        let normalized: String = value.chars().map(normalize_basic_char).collect();
        trim_end(&normalized).to_string()
    }

    /// JS `String.prototype.trimEnd`: trim trailing whitespace only. Rust's
    /// `trim_end` trims the same Unicode whitespace set for the characters we
    /// care about here (ASCII spaces/tabs and the mapped exotic spaces once
    /// normalized).
    fn trim_end(value: &str) -> &str {
        value.trim_end()
    }

    /// First CHAR index of `needle` in `haystack` (JS `indexOf` on strings whose
    /// normalization is char-for-char 1:1, so a char index is well-defined).
    fn char_index_of(haystack: &str, needle: &str) -> Option<usize> {
        let byte_idx = haystack.find(needle)?;
        Some(haystack[..byte_idx].chars().count())
    }

    /// qwen `UNICODE_EQUIVALENT_MAP`: curly quotes to straight, several dash
    /// variants to ASCII hyphen-minus, and exotic spaces to a normal space.
    fn normalize_basic_char(c: char) -> char {
        match c {
            // Hyphen / dash variations -> ASCII hyphen-minus.
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Curly single quotes -> straight apostrophe.
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Curly double quotes -> straight double quote.
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Whitespace variants -> normal space.
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        }
    }
}

// ---- edited-region snippet (qwen editHelper.ts extractEditSnippet) ----------

/// The faithful port of qwen v0.16.0 `utils/editHelper.ts` `extractEditSnippet`:
/// the changed region of the NEW content plus a few context lines, echoed back
/// in the success message so the model sees what landed.
mod snippet {
    /// Number of context lines kept on each side of the change (qwen
    /// `SNIPPET_CONTEXT_LINES`).
    const CONTEXT_LINES: usize = 4;
    /// Above this many changed lines, no snippet is produced (qwen
    /// `SNIPPET_MAX_LINES`).
    const MAX_LINES: usize = 1000;

    /// qwen `EditSnippetResult` (1-indexed line bounds + total + content).
    pub struct Snippet {
        pub start_line: usize,
        pub end_line: usize,
        pub total_lines: usize,
        pub content: String,
    }

    /// qwen `extractEditSnippet`: for a new file (`old_content == None`) the
    /// whole content is the snippet; otherwise diff old vs new from both ends to
    /// find the changed region and quote it with context. Returns `None` when
    /// there is no meaningful change (identical / empty) or the region is too
    /// large.
    pub fn extract(old_content: Option<&str>, new_content: &str) -> Option<Snippet> {
        let new_lines: Vec<&str> = new_content.split('\n').collect();
        let total_lines = new_lines.len();

        let Some(old_content) = old_content else {
            return Some(Snippet {
                start_line: 1,
                end_line: total_lines,
                total_lines,
                content: new_content.to_string(),
            });
        };

        if old_content == new_content || new_content.is_empty() {
            return None;
        }

        let old_lines: Vec<&str> = old_content.split('\n').collect();

        // First differing line from the start.
        let min_length = old_lines.len().min(new_lines.len());
        let mut first_diff = 0usize;
        while first_diff < min_length && old_lines[first_diff] == new_lines[first_diff] {
            first_diff += 1;
        }

        // First differing line from the end (i64 so both cursors can pass below
        // first_diff without underflow, matching qwen's `>= firstDiffLine`).
        let mut old_end = old_lines.len() as i64 - 1;
        let mut new_end = new_lines.len() as i64 - 1;
        let first_diff_i = first_diff as i64;
        while old_end >= first_diff_i
            && new_end >= first_diff_i
            && old_lines[old_end as usize] == new_lines[new_end as usize]
        {
            old_end -= 1;
            new_end -= 1;
        }

        // Changed region in the new content, 1-indexed.
        let change_start = first_diff + 1;
        let change_end = (new_end + 1) as usize;

        if change_end.saturating_sub(change_start) > MAX_LINES {
            return None;
        }

        let snippet_start = change_start.saturating_sub(CONTEXT_LINES).max(1);
        let snippet_end = (change_end + CONTEXT_LINES).min(total_lines);
        let content = new_lines[snippet_start - 1..snippet_end].join("\n");

        Some(Snippet {
            start_line: snippet_start,
            end_line: snippet_end,
            total_lines,
            content,
        })
    }
}

#[cfg(test)]
#[path = "../../tests/tools/edit_file.rs"]
mod tests;
