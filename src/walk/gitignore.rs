//! The per-file `.gitignore` check - a confined port of qwen v0.16.0
//! `utils/gitIgnoreParser.ts` (`GitIgnoreParser.isIgnored`) as used by
//! `FileDiscoveryService.shouldGitIgnoreFile`. The sibling of
//! [`super::qwenignore`]: where that matcher answers `.qwenignore`, this one
//! answers `.gitignore` plus `.git/info/exclude`, both anchored to the Project
//! Root. `list_directory` filters each direct child through this predicate
//! (qwen's default `respectGitIgnore`, ls.ts:209-212), reporting a count of the
//! entries it drops.
//!
//! Unlike [`super::walk_files`], which is a RECURSIVE gitignore-aware walk, this
//! is a POINT query: "would git ignore this one path, given the root's ignore
//! files?" ls needs the point query because it lists a flat `readdir` of one
//! directory's direct children and filters that list, rather than walking a
//! tree. qwen's parser walks up ancestor `.gitignore` files too; suspenders'
//! tools operate confined to the Project Root, so seeding the matcher from the
//! root's `.gitignore` and `.git/info/exclude` (the two qwen always loads first)
//! covers the confined case.
//!
//! Reuses the `ignore` crate's `GitignoreBuilder` (the same engine
//! [`super::walk_files`] and [`super::qwenignore`] build on) so the gitignore
//! precedence rules (negation, anchoring, directory patterns) match without a
//! hand-rolled parser. Absent any ignore file, nothing is ignored, so the common
//! case pays only two `is_file` misses.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Whether `abs` (an absolute, root-confined path) matches a `.gitignore` (or
/// `.git/info/exclude`) pattern at `root` (qwen `shouldGitIgnoreFile` /
/// `GitIgnoreParser.isIgnored`). `is_dir` distinguishes a directory child from a
/// file child so a directory-only pattern (`build/`) matches the `build`
/// directory entry itself. A missing ignore file, or a path outside `root`, is
/// never ignored.
pub(crate) fn is_ignored(root: &Path, abs: &Path, is_dir: bool) -> bool {
    match load(root) {
        // The ROOT-RELATIVE strip-prefix + empty-guard + match tail is shared
        // with `qwenignore` (they seed different files into the matcher).
        Some(matcher) => super::point_query(&matcher, root, abs, is_dir),
        None => false,
    }
}

/// Build the `.gitignore` matcher for `root`, seeded from `<root>/.gitignore`
/// and `<root>/.git/info/exclude` (the two ignore files qwen's
/// `GitIgnoreParser.isIgnored` loads for the project root), or `None` when
/// neither file contributes a usable pattern.
fn load(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let mut added = false;

    let gitignore = root.join(".gitignore");
    if gitignore.is_file() && builder.add(&gitignore).is_none() {
        added = true;
    }

    let exclude = root.join(".git").join("info").join("exclude");
    if exclude.is_file() && builder.add(&exclude).is_none() {
        added = true;
    }

    if !added {
        return None;
    }
    let matcher = builder.build().ok()?;
    // No usable patterns => nothing is ignored (qwen's empty-parser case).
    if matcher.num_ignores() == 0 && matcher.num_whitelists() == 0 {
        return None;
    }
    Some(matcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn no_gitignore_ignores_nothing() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "secret.txt", "x");
        assert!(!is_ignored(tmp.path(), &tmp.path().join("secret.txt"), false));
    }

    #[test]
    fn a_matching_pattern_is_ignored() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n*.log\n").unwrap();
        write(tmp.path(), "ignored.txt", "x");
        write(tmp.path(), "run.log", "x");
        write(tmp.path(), "kept.txt", "x");
        assert!(is_ignored(tmp.path(), &tmp.path().join("ignored.txt"), false));
        assert!(is_ignored(tmp.path(), &tmp.path().join("run.log"), false));
        assert!(!is_ignored(tmp.path(), &tmp.path().join("kept.txt"), false));
    }

    #[test]
    fn a_directory_pattern_ignores_the_directory_entry() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "build/\n").unwrap();
        write(tmp.path(), "build/out.o", "x");
        // The directory entry itself matches the `build/` directory pattern.
        assert!(is_ignored(tmp.path(), &tmp.path().join("build"), true));
    }

    #[test]
    fn the_git_info_exclude_file_is_honored() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), ".git/info/exclude", "local.txt\n");
        write(tmp.path(), "local.txt", "x");
        write(tmp.path(), "kept.txt", "x");
        assert!(is_ignored(tmp.path(), &tmp.path().join("local.txt"), false));
        assert!(!is_ignored(tmp.path(), &tmp.path().join("kept.txt"), false));
    }

    #[test]
    fn the_root_itself_is_not_ignored() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*\n").unwrap();
        assert!(!is_ignored(tmp.path(), tmp.path(), true));
    }
}
