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
fn no_qwenignore_ignores_nothing() {
    let tmp = TempDir::new().unwrap();
    write(tmp.path(), "secret.txt", "x");
    assert!(!is_ignored(tmp.path(), &tmp.path().join("secret.txt")));
}

#[test]
fn a_matching_pattern_is_ignored() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n*.key\n").unwrap();
    write(tmp.path(), "secret.txt", "x");
    write(tmp.path(), "id.key", "x");
    write(tmp.path(), "kept.txt", "x");
    assert!(is_ignored(tmp.path(), &tmp.path().join("secret.txt")));
    assert!(is_ignored(tmp.path(), &tmp.path().join("id.key")));
    assert!(!is_ignored(tmp.path(), &tmp.path().join("kept.txt")));
}

#[test]
fn comments_and_blank_lines_are_skipped() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "# a comment\n\nbuild/\n").unwrap();
    write(tmp.path(), "build/out.o", "x");
    write(tmp.path(), "src/main.rs", "x");
    assert!(is_ignored(tmp.path(), &tmp.path().join("build/out.o")));
    assert!(!is_ignored(tmp.path(), &tmp.path().join("src/main.rs")));
}

#[test]
fn the_root_itself_is_not_ignored() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "*\n").unwrap();
    assert!(!is_ignored(tmp.path(), tmp.path()));
}
