//! The `.qwenignore` check - a confined port of qwen v0.16.0
//! `utils/qwenIgnoreParser.ts` (`QwenIgnoreParser.isIgnored`). The shared
//! ignore matcher two tools lean on: read_file rejects a `file_path` that
//! matches a `.qwenignore` pattern at the Project Root with qwen's verbatim
//! message, and glob filters its walked matches through the same predicate
//! (qwen's `respectQwenIgnore` default, on for both). Living beside
//! [`super::walk_files`] keeps the two ignore mechanisms (`.gitignore` via the
//! walker, `.qwenignore` via this matcher) in one place.
//!
//! qwen loads `<root>/.qwenignore` once, splits it into non-empty non-comment
//! patterns, and matches the path RELATIVE TO THE PROJECT ROOT, `/`-normalized,
//! with the `ignore` npm package's gitignore semantics. Both callers pass the
//! Project Root as `root` here (glob anchors ignores to the project root even
//! when its search dir is a subdir, glob.ts:159), so the `/build/`-style
//! root-anchored patterns resolve against the project root, not the caller's
//! working subtree. Suspenders reuses the `ignore` crate (the same walker
//! `walk.rs` builds on) to get the same precedence rules without
//! re-implementing them: a one-file `GitignoreBuilder` seeded from
//! `<root>/.qwenignore` is exactly a `.qwenignore` matcher.
//!
//! Absent `.qwenignore`, nothing is ignored (qwen's `patterns.length === 0`
//! short-circuit), so the common case pays only a `metadata` miss.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Whether `abs` (an absolute, root-confined path) matches a `.qwenignore`
/// pattern at `root` (qwen `shouldQwenIgnoreFile` / `QwenIgnoreParser.isIgnored`).
/// A missing or empty `.qwenignore`, or a path outside `root`, is never ignored.
pub(crate) fn is_ignored(root: &Path, abs: &Path) -> bool {
    match load(root) {
        // The ROOT-RELATIVE strip-prefix + empty-guard + match tail is shared
        // with `gitignore` (they seed different files into the matcher).
        // `is_dir = false`: read_file/grep/ls only reach here for a regular
        // file target (a directory pattern still matches a file nested under it,
        // e.g. `build/out.o`, via `matched_path_or_any_parents`).
        Some(matcher) => super::point_query(&matcher, root, abs, false),
        None => false,
    }
}

/// Build the `.qwenignore` matcher for `root`, or `None` when there is no
/// `.qwenignore` (or it has no usable patterns). The `ignore` crate's
/// `GitignoreBuilder` parses the same gitignore syntax the qwen `ignore` npm
/// package does, so precedence (negation, anchoring, directory patterns) matches
/// without a hand-rolled parser.
fn load(root: &Path) -> Option<Gitignore> {
    let file = root.join(".qwenignore");
    if !file.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(root);
    // `add` returns Some(err) on a read failure; a broken .qwenignore is treated
    // as "no patterns" rather than failing the read outright.
    if builder.add(&file).is_some() {
        return None;
    }
    let matcher = builder.build().ok()?;
    // No patterns => qwen's `patterns.length === 0` short-circuit: not ignored.
    if matcher.num_ignores() == 0 && matcher.num_whitelists() == 0 {
        return None;
    }
    Some(matcher)
}

#[cfg(test)]
#[path = "../../tests/walk/qwenignore.rs"]
mod tests;
