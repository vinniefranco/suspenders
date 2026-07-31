//! The one gitignore-aware file walk the read-only tools share. `glob` finds
//! files by name and `@path` completion enumerates the project tree; rather
//! than two "ignore" mechanisms drifting apart, both walk through
//! [`walk_files`], so a file the model can `glob` is exactly a file `@path`
//! offers, and both honor the SAME `.gitignore`.
//!
//! Built on `ignore` (the gitignore-aware walker ripgrep uses) so a real
//! `.gitignore` is respected without re-implementing its precedence rules. The
//! walk is confined the same way the tools were before: it never follows
//! symlinks (following could walk out of the root or loop forever), and it
//! always prunes the [`SKIP_DIRS`] vendored/build set even in a repo with no
//! `.gitignore`, so behavior is preserved on plain trees.
//!
//! Output is DETERMINISTIC: the single-threaded walker with a file-name sorter
//! yields the same stable, depth-first order every run. glob re-sorts its
//! matches by modification time, so this stable input order is what makes its
//! tie-breaks (equal-mtime files, and the alphabetical older-file bucket)
//! reproducible rather than being the final order itself.

use ignore::WalkBuilder;
use ignore::gitignore::Gitignore;
use std::path::{Path, PathBuf};

pub mod gitignore;
pub mod qwenignore;

/// The shared point query behind both ignore predicates ([`gitignore::is_ignored`]
/// and [`qwenignore::is_ignored`]): would `matcher` (already seeded from its
/// module's ignore file) ignore `abs`, matched as the path RELATIVE to `root`?
/// A path that is the root itself, or escapes it, is never ignored (qwen's
/// `relativePath === '' || relativePath.startsWith('..')` guard). The two
/// modules stay distinct - they seed different files into the matcher - but the
/// strip-prefix + empty-guard + `matched_path_or_any_parents` tail lives once
/// here so it cannot drift.
fn point_query(matcher: &Gitignore, root: &Path, abs: &Path, is_dir: bool) -> bool {
    let Ok(rel) = abs.strip_prefix(root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    // `matched_path_or_any_parents` so a directory pattern (`build/`) ignores a
    // file nested under it, matching gitignore semantics.
    matcher.matched_path_or_any_parents(rel, is_dir).is_ignore()
}

/// The vendored/build directories the read-only tools never descend into.
/// Public so the opening environment tree ([`crate::env_context`]) prunes the
/// SAME set the tools do, and the tree the model sees matches what glob would
/// walk. Pruned explicitly (not just via `.gitignore`) so a tree without a
/// gitignore still skips them.
pub const SKIP_DIRS: &[&str] = &[
    ".claude",
    ".git",
    "_build",
    "deps",
    "node_modules",
    ".direnv",
    ".nix-hex",
    ".nix-mix",
    ".elixir_ls",
];

/// Walks `root` recursively for regular files, returning ABSOLUTE paths in a
/// stable, depth-first sorted order. Gitignore-aware (respects `.gitignore`,
/// git global excludes, and parent gitignores), never follows symlinks, and
/// always prunes the [`SKIP_DIRS`] set even when no `.gitignore` names them.
/// Directories and symlinks are not in the output - only regular files. An
/// unreadable root yields no files (fail-open), never a panic.
pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder
        // Respect .gitignore / .git/info/exclude / parent + global gitignores,
        // but do NOT require a `.git` dir to be present - a bare tree with a
        // lone `.gitignore` is still honored.
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        // Keep walking dotfiles: glob matched hidden files (except SKIP_DIRS),
        // so opt out of `ignore`'s hide-dotfiles default and prune explicitly.
        .hidden(false)
        // Never follow symlinks - following could escape the root or loop.
        .follow_links(false)
        // Prune the vendored/build set by basename, even absent a .gitignore,
        // so SKIP_DIRS stays the single source of truth for that list.
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIRS.contains(&name))
        })
        // Single-threaded + file-name sort => deterministic depth-first order,
        // so glob's sorted-output contract holds with no post-sort.
        .sort_by_file_name(|a, b| a.cmp(b));

    builder
        .build()
        .filter_map(Result::ok)
        // Only regular files (a symlink is not a file with follow_links off,
        // so symlinks fall out here too, matching the old lstat skip).
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .map(ignore::DirEntry::into_path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Create an empty file at `root/rel`, making parent dirs as needed.
    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "").unwrap();
    }

    // Walk `root` and return the file paths relative to it, `/`-separated, so
    // assertions read as clean project-relative paths.
    fn walk_rel(root: &Path) -> Vec<String> {
        walk_files(root)
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walks_regular_files_in_sorted_depth_first_order() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "src/main.rs");
        touch(tmp.path(), "src/net/client.rs");
        touch(tmp.path(), "README.md");

        // Directories are not in the output; files come back sorted.
        assert_eq!(
            walk_rel(tmp.path()),
            vec!["README.md", "src/main.rs", "src/net/client.rs"]
        );
    }

    #[test]
    fn prunes_the_skip_dirs_set_without_a_gitignore() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "node_modules/dep.js");
        touch(tmp.path(), "keep.rs");

        assert_eq!(walk_rel(tmp.path()), vec!["keep.rs"]);
    }

    #[test]
    fn respects_a_gitignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        touch(tmp.path(), "ignored.txt");
        touch(tmp.path(), "kept.txt");

        let files = walk_rel(tmp.path());
        assert!(files.contains(&"kept.txt".to_string()));
        assert!(
            !files.contains(&"ignored.txt".to_string()),
            "a .gitignore'd file is not walked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "real.rs");

        // A symlink back to the root (a cycle) and one out to another dir; a
        // symlink is neither followed nor emitted as a file.
        symlink(".", tmp.path().join("loop")).unwrap();
        let outside = TempDir::new().unwrap();
        touch(outside.path(), "secret.rs");
        symlink(outside.path(), tmp.path().join("escape")).unwrap();

        assert_eq!(walk_rel(tmp.path()), vec!["real.rs"]);
    }

    #[test]
    fn an_unreadable_root_yields_no_files() {
        assert_eq!(
            walk_files(Path::new("/no/such/dir/anywhere")),
            Vec::<PathBuf>::new()
        );
    }
}
