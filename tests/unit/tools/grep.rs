use super::*;
use tempfile::TempDir;

fn ctx(root: &Path) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), 10_000)
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    Grep.run(&input, ctx).await
}

#[test]
fn spec_requires_pattern_only_and_carries_the_verbatim_strings() {
    let spec = Grep.spec();
    assert_eq!(spec.name, "grep_search");
    assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    assert_eq!(
        spec.input_schema["properties"]["pattern"]["description"],
        json!(PATTERN_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["glob"]["description"],
        json!(GLOB_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["path"]["description"],
        json!(PATH_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["limit"]["description"],
        json!(LIMIT_DESCRIPTION)
    );
    assert!(
        spec.description
            .starts_with("A powerful search tool built on ripgrep")
    );
    assert!(
        spec.description
            .contains("Use Agent tool for open-ended searches")
    );
}

#[tokio::test]
async fn returns_matches_under_the_verbatim_header() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::write(
        tmp.path().join("lib/a.ex"),
        "defmodule A do\n  # needle here\nend\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("other.txt"), "nothing to see\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nlib/a.ex:2:  # needle here".into())
    );
}

#[tokio::test]
async fn plural_match_term_and_multiple_lines() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "foo1\nfoo2\nbar3\n").unwrap();

    assert_eq!(
        run(json!({"pattern": r"^foo\d$"}), &ctx(tmp.path())).await,
        Ok("Found 2 matches for pattern \"^foo\\d$\" in the workspace directory:\n---\na.txt:1:foo1\na.txt:2:foo2".into())
    );
}

#[tokio::test]
async fn search_is_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "Needle here\nNEEDLE too\n").unwrap();

    let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains("a.txt:1:Needle here"));
    assert!(out.contains("a.txt:2:NEEDLE too"));
    assert!(out.starts_with("Found 2 matches"));
}

#[cfg(unix)]
#[tokio::test]
async fn does_not_follow_symlinks() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("real.txt"), "needle inside\n").unwrap();
    symlink(".", tmp.path().join("loop")).unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "needle outside\n").unwrap();
    symlink(outside.path(), tmp.path().join("escape")).unwrap();
    symlink(
        outside.path().join("secret.txt"),
        tmp.path().join("linked.txt"),
    )
    .unwrap();

    let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains("real.txt:1:needle inside"));
    assert!(!out.contains("outside"));
}

#[tokio::test]
async fn searches_only_under_the_given_path_and_reports_it() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::create_dir_all(tmp.path().join("other")).unwrap();
    std::fs::write(tmp.path().join("lib/a.ex"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("other/b.ex"), "needle\n").unwrap();

    assert_eq!(
        run(
            json!({"pattern": "needle", "path": "lib"}),
            &ctx(tmp.path())
        )
        .await,
        Ok("Found 1 match for pattern \"needle\" in path \"lib\":\n---\na.ex:1:needle".into())
    );
}

#[tokio::test]
async fn path_may_be_a_single_file() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("single.txt"), "one needle\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle", "path": "single.txt"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in path \"single.txt\":\n---\nsingle.txt:1:one needle".into())
    );
}

#[tokio::test]
async fn glob_filters_the_walked_files() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();

    let out = run(
        json!({"pattern": "needle", "glob": "*.rs"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "Found 1 match for pattern \"needle\" in the workspace directory (filter: \"*.rs\"):\n---\na.rs:1:needle"
    );
}

#[tokio::test]
async fn glob_can_cross_directories_with_double_star() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/net")).unwrap();
    std::fs::write(tmp.path().join("src/net/client.rs"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("top.txt"), "needle\n").unwrap();

    let out = run(
        json!({"pattern": "needle", "glob": "**/*.rs"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert!(out.contains("src/net/client.rs:1:needle"));
    assert!(!out.contains("top.txt"));
}

#[tokio::test]
async fn a_slash_free_glob_filter_matches_at_any_depth() {
    // ripgrep --glob semantics: a slash-free `*.rs` filters files at ANY
    // depth, so a deeply nested match survives the filter.
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
    std::fs::write(tmp.path().join("src/nested/a.rs"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();

    let out = run(
        json!({"pattern": "needle", "glob": "*.rs"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert!(out.contains("src/nested/a.rs:1:needle"));
    assert!(!out.contains("b.txt"));
}

#[tokio::test]
async fn an_empty_matching_regex_does_not_count_a_phantom_trailing_line() {
    // A file ending in "\n" must NOT report a phantom match on the empty
    // segment `split('\n')` produces after the final newline: `^` over
    // "a\nb\n" is 2 matches (the two real lines), not 3.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("f.txt"), "a\nb\n").unwrap();

    let out = run(json!({"pattern": "^"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(
        out.starts_with("Found 2 matches for pattern \"^\" in the workspace directory:\n---\n"),
        "got: {out}"
    );
    assert!(out.contains("f.txt:1:a"));
    assert!(out.contains("f.txt:2:b"));
    assert!(!out.contains("f.txt:3:"));
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
        std::fs::write(tmp.path().join(dir).join("match.txt"), "needle\n").unwrap();
    }
    std::fs::write(tmp.path().join("visible.txt"), "needle\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nvisible.txt:1:needle".into())
    );
}

#[tokio::test]
async fn respects_a_gitignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "needle\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nkept.txt:1:needle".into())
    );
}

#[tokio::test]
async fn respects_a_qwenignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "needle\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nkept.txt:1:needle".into())
    );
}

#[tokio::test]
async fn a_qwenignore_anchors_to_the_project_root_not_the_search_dir() {
    // A ROOT-ANCHORED `.qwenignore` pattern (`/build/`) ignores only the
    // top-level `build/`, not a nested `sub/build/`. Searching under
    // `path: "sub"`, the ignore anchors to the PROJECT ROOT (ripGrep.ts
    // loads .qwenignore from the workspace dirs): if it anchored to the
    // search dir instead, `/build/` would over-match `sub/build/`.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "/build/\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("build")).unwrap();
    std::fs::create_dir_all(tmp.path().join("sub/build")).unwrap();
    std::fs::write(tmp.path().join("build/top.txt"), "needle\n").unwrap();
    std::fs::write(tmp.path().join("sub/build/nested.txt"), "needle\n").unwrap();

    let out = run(
        json!({"pattern": "needle", "path": "sub"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    // Root-anchored: the nested build survives (it is not the top-level one).
    assert!(out.contains("build/nested.txt:1:needle"));

    // Cross-check: over the project root, the top-level build IS ignored.
    let root_out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(!root_out.contains("build/top.txt"));
    assert!(root_out.contains("sub/build/nested.txt:1:needle"));
}

#[tokio::test]
async fn skips_binary_files() {
    let tmp = TempDir::new().unwrap();
    let mut bin = vec![0u8, 1, 2];
    bin.extend_from_slice(b"needle\n");
    std::fs::write(tmp.path().join("binary.dat"), bin).unwrap();
    std::fs::write(tmp.path().join("text.txt"), "needle\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\ntext.txt:1:needle".into())
    );
}

#[tokio::test]
async fn limit_caps_matches_with_the_verbatim_trailer() {
    let tmp = TempDir::new().unwrap();
    let lines = (1..=10)
        .map(|n| format!("needle {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path().join("many.txt"), format!("{lines}\n")).unwrap();

    let out = run(json!({"pattern": "needle", "limit": 4}), &ctx(tmp.path()))
        .await
        .unwrap();
    // Header shows the REAL total (10); the body is capped at 4 lines; the
    // trailer names the 6 omitted lines.
    assert!(
        out.starts_with(
            "Found 10 matches for pattern \"needle\" in the workspace directory:\n---\n"
        )
    );
    assert!(out.ends_with("\n---\n[6 lines truncated] ..."));
    let body = out.split("\n---\n").nth(1).unwrap();
    assert_eq!(body.split('\n').count(), 4);
    assert!(body.starts_with("many.txt:1:needle 1"));
}

#[tokio::test]
async fn a_single_truncated_line_says_line_singular() {
    let tmp = TempDir::new().unwrap();
    let lines = (1..=3)
        .map(|n| format!("needle {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path().join("f.txt"), format!("{lines}\n")).unwrap();

    let out = run(json!({"pattern": "needle", "limit": 2}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.ends_with("\n---\n[1 line truncated] ..."));
}

#[tokio::test]
async fn no_matches_is_the_verbatim_message() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "nothing\n").unwrap();

    assert_eq!(
        run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
        Ok("No matches found for pattern \"needle\" in the workspace directory.".into())
    );
}

#[tokio::test]
async fn no_matches_message_includes_path_and_filter_clauses() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::write(tmp.path().join("lib/a.rs"), "nothing\n").unwrap();

    assert_eq!(
        run(
            json!({"pattern": "needle", "path": "lib", "glob": "*.rs"}),
            &ctx(tmp.path())
        )
        .await,
        Ok("No matches found for pattern \"needle\" in path \"lib\" (filter: \"*.rs\").".into())
    );
}

#[tokio::test]
async fn an_invalid_regex_is_rejected_with_the_verbatim_message() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("code.ex"), "def foo(bar) do\n").unwrap();

    let err = run(json!({"pattern": "foo(bar"}), &ctx(tmp.path()))
        .await
        .unwrap_err();
    assert!(
        err.starts_with("Invalid regular expression pattern: foo(bar. Error:"),
        "got: {err}"
    );
}

#[tokio::test]
async fn zero_matches_from_a_valid_regex_is_a_real_answer() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x+y\n").unwrap();

    // `x+y` is a valid regex (one-or-more x, then y) and does not match the
    // literal `x+y`; qwen returns the no-matches message, never a fallback.
    assert_eq!(
        run(json!({"pattern": "x+y"}), &ctx(tmp.path())).await,
        Ok("No matches found for pattern \"x+y\" in the workspace directory.".into())
    );
}

#[tokio::test]
async fn missing_search_path_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let err = run(
        json!({"pattern": "x", "path": "no_such_dir"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap_err();
    assert!(err.contains("enoent"));
}

#[tokio::test]
async fn paths_escaping_the_project_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        run(
            json!({"pattern": "root", "path": "../.."}),
            &ctx(tmp.path())
        )
        .await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn missing_or_non_string_pattern_is_a_structured_error() {
    let tmp = TempDir::new().unwrap();
    let c = ctx(tmp.path());
    assert!(
        crate::tools::execute("grep_search", &json!({}), &c)
            .await
            .is_error
    );
    assert!(
        crate::tools::execute("grep_search", &json!({"pattern": 42}), &c)
            .await
            .is_error
    );
}
