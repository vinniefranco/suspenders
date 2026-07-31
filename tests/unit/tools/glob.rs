use super::*;
use std::time::Duration;
use tempfile::TempDir;

fn ctx(root: &Path) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), 10_000)
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    Glob.run(&input, ctx).await
}

// Set a file's mtime `secs` in the past, so the recency sort is testable.
fn age(path: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
}

// The absolute path a match is reported as, `/`-normalized.
fn abs(tmp: &TempDir, rel: &str) -> String {
    tmp.path().join(rel).to_string_lossy().replace('\\', "/")
}

#[test]
fn spec_requires_pattern_only_and_carries_the_verbatim_strings() {
    let spec = Glob.spec();
    assert_eq!(spec.name, "glob");
    assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    // Verbatim property descriptions (qwen glob.ts).
    assert_eq!(
        spec.input_schema["properties"]["pattern"]["description"],
        json!("The glob pattern to match files against")
    );
    assert_eq!(
        spec.input_schema["properties"]["path"]["description"],
        json!(PATH_DESCRIPTION)
    );
    assert!(
        spec.description
            .starts_with("Fast file pattern matching tool")
    );
    assert!(spec.description.contains("use the Agent tool instead"));
}

#[tokio::test]
async fn returns_absolute_paths_under_the_verbatim_header() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/net")).unwrap();
    std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
    std::fs::write(tmp.path().join("src/net/client.rs"), "").unwrap();
    std::fs::write(tmp.path().join("README.md"), "").unwrap();
    // Age both matches out of the recency window so the order is
    // deterministic (alphabetical by absolute path).
    age(&tmp.path().join("src/main.rs"), 2 * 24 * 60 * 60);
    age(&tmp.path().join("src/net/client.rs"), 2 * 24 * 60 * 60);

    let out = run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert_eq!(
        out,
        format!(
            "Found 2 file(s) matching \"**/*.rs\" in the workspace directory, \
             sorted by modification time (newest first):\n---\n{}\n{}",
            abs(&tmp, "src/main.rs"),
            abs(&tmp, "src/net/client.rs"),
        )
    );
}

#[tokio::test]
async fn recent_files_sort_newest_first_before_older_files() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("old.rs"), "").unwrap();
    std::fs::write(tmp.path().join("newer.rs"), "").unwrap();
    std::fs::write(tmp.path().join("newest.rs"), "").unwrap();
    // Two recent files (inside 24h) and one old file (well outside).
    age(&tmp.path().join("newest.rs"), 60); // 1 min ago
    age(&tmp.path().join("newer.rs"), 3600); // 1 h ago
    age(&tmp.path().join("old.rs"), 10 * 24 * 60 * 60); // 10 days ago

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    let listed = out.split("\n---\n").nth(1).unwrap();
    assert_eq!(
        listed,
        format!(
            "{}\n{}\n{}",
            abs(&tmp, "newest.rs"),
            abs(&tmp, "newer.rs"),
            abs(&tmp, "old.rs"),
        )
    );
}

#[tokio::test]
async fn old_files_sort_alphabetically_by_absolute_path() {
    let tmp = TempDir::new().unwrap();
    for name in ["c.rs", "a.rs", "b.rs"] {
        std::fs::write(tmp.path().join(name), "").unwrap();
        age(&tmp.path().join(name), 5 * 24 * 60 * 60);
    }

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    let listed = out.split("\n---\n").nth(1).unwrap();
    assert_eq!(
        listed,
        format!(
            "{}\n{}\n{}",
            abs(&tmp, "a.rs"),
            abs(&tmp, "b.rs"),
            abs(&tmp, "c.rs")
        ),
    );
}

#[tokio::test]
async fn matching_is_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Main.RS"), "").unwrap();

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "Main.RS")));
}

#[tokio::test]
async fn single_star_stays_within_a_path_segment() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/inner")).unwrap();
    std::fs::write(tmp.path().join("src/a.rs"), "").unwrap();
    std::fs::write(tmp.path().join("src/inner/c.rs"), "").unwrap();
    age(&tmp.path().join("src/a.rs"), 5 * 24 * 60 * 60);

    let out = run(json!({"pattern": "src/*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "src/a.rs")));
    assert!(!out.contains("inner"));
}

#[tokio::test]
async fn a_slash_free_glob_matches_the_basename_at_any_depth() {
    // ripgrep/gitignore semantics: `*.rs` (no `/`) matches at ANY depth,
    // so it finds a deeply nested file, not just a top-level one.
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
    std::fs::write(tmp.path().join("src/nested/a.rs"), "").unwrap();
    age(&tmp.path().join("src/nested/a.rs"), 5 * 24 * 60 * 60);

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "src/nested/a.rs")));
}

#[tokio::test]
async fn question_mark_matches_exactly_one_character() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a1.txt"), "").unwrap();
    std::fs::write(tmp.path().join("a10.txt"), "").unwrap();

    let out = run(json!({"pattern": "a?.txt"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "a1.txt")));
    assert!(!out.contains("a10.txt"));
}

#[tokio::test]
async fn a_character_class_matches_the_enumerated_characters() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();
    std::fs::write(tmp.path().join("c.txt"), "").unwrap();

    let out = run(json!({"pattern": "[ab].txt"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "a.txt")));
    assert!(!out.contains("c.txt"));
}

#[tokio::test]
async fn searches_only_under_the_given_path_and_reports_it_within() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::create_dir_all(tmp.path().join("other")).unwrap();
    std::fs::write(tmp.path().join("lib/a.ex"), "").unwrap();
    std::fs::write(tmp.path().join("other/b.ex"), "").unwrap();

    let out = run(json!({"pattern": "*.ex", "path": "lib"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "lib/a.ex")));
    assert!(!out.contains("other/b.ex"));
    assert!(out.contains(&format!("within {}", tmp.path().join("lib").display())));
}

#[tokio::test]
async fn skips_git_build_deps_and_friends() {
    let tmp = TempDir::new().unwrap();
    for dir in [
        ".git",
        "_build",
        "deps",
        "node_modules",
        ".direnv",
        ".nix-hex",
        ".nix-mix",
        ".elixir_ls",
    ] {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        std::fs::write(tmp.path().join(dir).join("hidden.rs"), "").unwrap();
    }
    std::fs::write(tmp.path().join("visible.rs"), "").unwrap();

    let out = run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "visible.rs")));
    assert!(!out.contains("hidden.rs"));
}

#[tokio::test]
async fn respects_a_gitignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), "").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "").unwrap();

    let out = run(json!({"pattern": "**/*.txt"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "kept.txt")));
    assert!(!out.contains("ignored.txt"));
}

#[tokio::test]
async fn respects_a_qwenignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "").unwrap();

    let out = run(json!({"pattern": "**/*.txt"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "kept.txt")));
    assert!(!out.contains("secret.txt"));
}

#[tokio::test]
async fn a_qwenignore_anchors_to_the_project_root_not_the_search_dir() {
    // A ROOT-ANCHORED `.qwenignore` pattern (`/build/`) ignores only the
    // top-level `build/`, not a nested `sub/build/`. Searching under
    // `path: "sub"`, the ignore must anchor to the PROJECT ROOT (glob.ts:159):
    // if it anchored to the search dir instead, `/build/` would over-match
    // `sub/build/` and wrongly drop the nested file.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "/build/\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("build")).unwrap();
    std::fs::create_dir_all(tmp.path().join("sub/build")).unwrap();
    std::fs::write(tmp.path().join("build/top.txt"), "").unwrap();
    std::fs::write(tmp.path().join("sub/build/nested.txt"), "").unwrap();

    let out = run(
        json!({"pattern": "**/*.txt", "path": "sub"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    // Root-anchored: the nested build survives (it is not the top-level one).
    assert!(out.contains(&abs(&tmp, "sub/build/nested.txt")));

    // Cross-check the same pattern DOES ignore the top-level build when the
    // search covers the project root, proving the pattern itself is live.
    let root_out = run(json!({"pattern": "**/*.txt"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(!root_out.contains("build/top.txt"));
    assert!(root_out.contains(&abs(&tmp, "sub/build/nested.txt")));
}

#[cfg(unix)]
#[tokio::test]
async fn does_not_follow_symlinks() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("real.rs"), "").unwrap();
    symlink(".", tmp.path().join("loop")).unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "").unwrap();
    symlink(outside.path(), tmp.path().join("escape")).unwrap();

    let out = run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains(&abs(&tmp, "real.rs")));
    assert!(!out.contains("secret.rs"));
}

#[tokio::test]
async fn caps_the_list_at_one_hundred_with_the_verbatim_trailer() {
    let tmp = TempDir::new().unwrap();
    for n in 0..150 {
        let name = format!("f{n:03}.rs");
        std::fs::write(tmp.path().join(&name), "").unwrap();
        // Age them all out of recency so the alphabetical order is stable
        // and the cap is deterministic.
        age(&tmp.path().join(&name), 5 * 24 * 60 * 60);
    }

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.starts_with("Found 150 file(s) matching \"*.rs\" in the workspace directory"));
    assert!(out.ends_with("\n---\n[50 files truncated] ..."));
    let listed = out.split("\n---\n").nth(1).unwrap();
    assert_eq!(listed.split('\n').count(), 100);
}

#[tokio::test]
async fn a_single_truncated_file_says_file_singular() {
    let tmp = TempDir::new().unwrap();
    for n in 0..101 {
        let name = format!("f{n:03}.rs");
        std::fs::write(tmp.path().join(&name), "").unwrap();
        age(&tmp.path().join(&name), 5 * 24 * 60 * 60);
    }

    let out = run(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.ends_with("\n---\n[1 file truncated] ..."));
}

#[tokio::test]
async fn zero_matches_is_the_verbatim_no_files_message() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();

    assert_eq!(
        run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
        Ok("No files found matching pattern \"**/*.rs\" in the workspace directory".into())
    );
}

#[tokio::test]
async fn an_invalid_glob_pattern_is_a_tool_error_not_a_panic() {
    let tmp = TempDir::new().unwrap();
    let err = run(json!({"pattern": "["}), &ctx(tmp.path()))
        .await
        .unwrap_err();
    assert!(err.contains("invalid glob pattern"));
}

#[tokio::test]
async fn a_missing_search_path_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let err = run(
        json!({"pattern": "*.rs", "path": "no_such_dir"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap_err();
    assert!(err.contains("glob"));
}

#[tokio::test]
async fn paths_escaping_the_project_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        run(
            json!({"pattern": "*.rs", "path": "../.."}),
            &ctx(tmp.path())
        )
        .await,
        Err("path escapes project root".into())
    );
    assert_eq!(
        run(json!({"pattern": "*.rs", "path": "/etc"}), &ctx(tmp.path())).await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn missing_or_non_string_pattern_is_a_structured_error() {
    let tmp = TempDir::new().unwrap();
    let c = ctx(tmp.path());
    assert!(crate::tools::execute("glob", &json!({}), &c).await.is_error);
    assert!(
        crate::tools::execute("glob", &json!({"pattern": 42}), &c)
            .await
            .is_error
    );
}

#[tokio::test]
async fn non_string_path_is_an_error() {
    let tmp = TempDir::new().unwrap();
    assert!(
        run(json!({"pattern": "*.rs", "path": 42}), &ctx(tmp.path()))
            .await
            .is_err()
    );
}
