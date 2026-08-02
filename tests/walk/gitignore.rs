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
    assert!(!is_ignored(
        tmp.path(),
        &tmp.path().join("secret.txt"),
        false
    ));
}

#[test]
fn a_matching_pattern_is_ignored() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n*.log\n").unwrap();
    write(tmp.path(), "ignored.txt", "x");
    write(tmp.path(), "run.log", "x");
    write(tmp.path(), "kept.txt", "x");
    assert!(is_ignored(
        tmp.path(),
        &tmp.path().join("ignored.txt"),
        false
    ));
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
