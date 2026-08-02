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
