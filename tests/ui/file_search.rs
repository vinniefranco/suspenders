
use super::*;
use tempfile::TempDir;

fn touch(root: &Path, rel: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, "").unwrap();
}

#[test]
fn an_empty_query_returns_the_walk_in_order_capped() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "README.md");
    touch(tmp.path(), "src/main.rs");
    let cache = WalkCache::new();
    let out = search(&cache, tmp.path(), "");
    let labels: Vec<_> = out.iter().map(|s| s.label.clone()).collect();
    assert_eq!(labels, vec!["README.md", "src/main.rs"]);
    // label == unescaped path; value is escaped (no spaces here, so equal).
    assert_eq!(out[0].value, "README.md");
}

#[test]
fn a_query_fuzzy_filters_and_paths_are_repo_relative() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "README.md");
    touch(tmp.path(), "src/composer.rs");
    let cache = WalkCache::new();
    let out = search(&cache, tmp.path(), "composer");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, "src/composer.rs");
    assert!(out[0].matched.is_some(), "the query is highlighted");
}

#[test]
fn an_accepted_path_with_a_space_is_backslash_escaped() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "my notes.md");
    let cache = WalkCache::new();
    let out = search(&cache, tmp.path(), "");
    assert_eq!(out[0].label, "my notes.md");
    assert_eq!(
        out[0].value, "my\\ notes.md",
        "spaces are escaped for insert"
    );
}

#[test]
fn the_cache_reuses_the_walk_within_its_ttl() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "a.rs");
    let cache = WalkCache::new();
    assert_eq!(search(&cache, tmp.path(), "").len(), 1);
    // A file created AFTER the first walk is not seen until the TTL lapses
    // (proves the cache is consulted, not re-walked every call).
    touch(tmp.path(), "b.rs");
    assert_eq!(
        search(&cache, tmp.path(), "").len(),
        1,
        "still the cached one-file walk"
    );
}
