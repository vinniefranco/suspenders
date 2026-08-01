
use super::*;
use tempfile::TempDir;

fn ctx(root: &std::path::Path) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), 10_000)
}

// ls REQUIRES an absolute path, so tests pass the tempdir's absolute path.
fn abs(tmp: &TempDir, sub: &str) -> String {
    if sub.is_empty() {
        tmp.path().to_string_lossy().into_owned()
    } else {
        tmp.path().join(sub).to_string_lossy().into_owned()
    }
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    ListFiles.run(&input, ctx).await
}

#[test]
fn spec_requires_absolute_path_and_carries_the_verbatim_strings() {
    let spec = ListFiles.spec();
    assert_eq!(spec.name, "list_directory");
    assert_eq!(spec.description, DESCRIPTION);
    assert_eq!(spec.input_schema["required"], json!(["path"]));
    assert_eq!(
        spec.input_schema["properties"]["path"]["description"],
        json!(PATH_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["ignore"]["description"],
        json!(IGNORE_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["ignore"]["items"]["type"],
        json!("string")
    );
    assert_eq!(
        spec.input_schema["properties"]["file_filtering_options"]["description"],
        json!(FILE_FILTERING_OPTIONS_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["file_filtering_options"]["properties"]["respect_git_ignore"]
            ["description"],
        json!(RESPECT_GIT_IGNORE_DESCRIPTION)
    );
    assert_eq!(
        spec.input_schema["properties"]["file_filtering_options"]["properties"]["respect_qwen_ignore"]
            ["description"],
        json!(RESPECT_QWEN_IGNORE_DESCRIPTION)
    );
}

#[tokio::test]
async fn lists_directories_first_then_alphabetical_with_dir_prefix_and_header() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("b_dir")).unwrap();
    std::fs::create_dir_all(tmp.path().join("a_dir")).unwrap();
    std::fs::write(tmp.path().join("z.txt"), "").unwrap();
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();

    let p = abs(&tmp, "");
    assert_eq!(
        run(json!({"path": p}), &ctx(tmp.path())).await,
        Ok(format!(
            "Listed 4 item(s) in {p}:\n---\n[DIR] a_dir\n[DIR] b_dir\na.txt\nz.txt"
        ))
    );
}

#[tokio::test]
async fn lists_a_subdirectory() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("sub/inner")).unwrap();
    std::fs::write(tmp.path().join("sub/file.txt"), "").unwrap();

    let p = abs(&tmp, "sub");
    assert_eq!(
        run(json!({"path": p}), &ctx(tmp.path())).await,
        Ok(format!(
            "Listed 2 item(s) in {p}:\n---\n[DIR] inner\nfile.txt"
        ))
    );
}

#[tokio::test]
async fn empty_directory_is_the_verbatim_message() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

    let p = abs(&tmp, "empty");
    assert_eq!(
        run(json!({"path": p}), &ctx(tmp.path())).await,
        Ok(format!("Directory {p} is empty."))
    );
}

#[tokio::test]
async fn missing_directory_is_the_verbatim_error() {
    let tmp = TempDir::new().unwrap();
    let p = abs(&tmp, "nope");
    assert_eq!(
        run(json!({"path": p.clone()}), &ctx(tmp.path())).await,
        Err(format!("Error: Directory not found or inaccessible: {p}"))
    );
}

#[tokio::test]
async fn a_file_target_is_the_verbatim_not_a_directory_error() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("f.txt"), "").unwrap();
    let p = abs(&tmp, "f.txt");
    assert_eq!(
        run(json!({"path": p.clone()}), &ctx(tmp.path())).await,
        Err(format!("Error: Path is not a directory: {p}"))
    );
}

#[tokio::test]
async fn a_relative_path_is_refused_with_qwens_verbatim_message() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        run(json!({"path": "sub"}), &ctx(tmp.path())).await,
        Err("Path must be absolute: sub".into())
    );
}

#[tokio::test]
async fn an_absolute_path_escaping_the_root_is_refused() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        run(json!({"path": "/etc"}), &ctx(tmp.path())).await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn a_missing_path_is_an_error() {
    let tmp = TempDir::new().unwrap();
    assert!(run(json!({}), &ctx(tmp.path())).await.is_err());
}

#[tokio::test]
async fn a_non_string_path_is_an_error() {
    let tmp = TempDir::new().unwrap();
    assert!(run(json!({"path": 42}), &ctx(tmp.path())).await.is_err());
}

#[tokio::test]
async fn ignore_globs_drop_matching_basenames() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("keep.rs"), "").unwrap();
    std::fs::write(tmp.path().join("skip.log"), "").unwrap();
    std::fs::write(tmp.path().join("also.log"), "").unwrap();

    let p = abs(&tmp, "");
    assert_eq!(
        run(json!({"path": p, "ignore": ["*.log"]}), &ctx(tmp.path())).await,
        Ok(format!("Listed 1 item(s) in {p}:\n---\nkeep.rs"))
    );
}

#[tokio::test]
async fn ignore_globs_are_case_sensitive() {
    // qwen's shouldIgnore regex has no `i` flag (ls.ts:104-108): `*.LOG`
    // must NOT drop `run.log`, but `*.log` must.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("run.log"), "").unwrap();
    let p = abs(&tmp, "");

    // Wrong case: the file survives.
    assert_eq!(
        run(
            json!({"path": p.clone(), "ignore": ["*.LOG"]}),
            &ctx(tmp.path())
        )
        .await,
        Ok(format!("Listed 1 item(s) in {p}:\n---\nrun.log"))
    );
    // Right case: the file is dropped, leaving an empty listing.
    assert_eq!(
        run(
            json!({"path": p.clone(), "ignore": ["*.log"]}),
            &ctx(tmp.path())
        )
        .await,
        Ok(format!("Listed 0 item(s) in {p}:\n---\n"))
    );
}

#[tokio::test]
async fn a_padded_path_is_trimmed_before_validation_and_in_the_header() {
    // qwen unescapePath(path.trim()) (ls.ts:363): surrounding whitespace is
    // stripped, so a padded absolute path lists fine and the header echoes
    // the trimmed path.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("only.txt"), "").unwrap();
    let trimmed = abs(&tmp, "");
    let padded = format!("  {trimmed}  ");
    assert_eq!(
        run(json!({"path": padded}), &ctx(tmp.path())).await,
        Ok(format!("Listed 1 item(s) in {trimmed}:\n---\nonly.txt"))
    );
}

#[tokio::test]
async fn respects_gitignore_and_appends_the_git_ignored_suffix() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), "").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "").unwrap();

    let p = abs(&tmp, "");
    let out = run(json!({"path": p.clone()}), &ctx(tmp.path()))
        .await
        .unwrap();
    // The .gitignore itself is a dotfile child and is listed; the ignored
    // file is dropped and counted.
    assert!(out.starts_with(&format!("Listed 2 item(s) in {p}:\n---\n")));
    assert!(out.contains("kept.txt"));
    assert!(!out.contains("\nignored.txt"));
    assert!(out.ends_with("\n\n(1 git-ignored)"));
}

#[tokio::test]
async fn respects_qwenignore_and_appends_the_qwen_ignored_suffix() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "").unwrap();

    let p = abs(&tmp, "");
    let out = run(json!({"path": p.clone()}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.contains("kept.txt"));
    assert!(!out.contains("\nsecret.txt"));
    assert!(out.ends_with("\n\n(1 qwen-ignored)"));
}

#[tokio::test]
async fn file_filtering_options_can_disable_gitignore() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), "").unwrap();
    std::fs::write(tmp.path().join("kept.txt"), "").unwrap();

    let p = abs(&tmp, "");
    let out = run(
        json!({"path": p, "file_filtering_options": {"respect_git_ignore": false}}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    // With gitignore disabled, the ignored file is listed and no suffix appears.
    assert!(out.contains("ignored.txt"));
    assert!(!out.contains("git-ignored"));
}

#[tokio::test]
async fn both_ignored_counts_appear_together() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "g.txt\n").unwrap();
    std::fs::write(tmp.path().join(".qwenignore"), "q.txt\n").unwrap();
    std::fs::write(tmp.path().join("g.txt"), "").unwrap();
    std::fs::write(tmp.path().join("q.txt"), "").unwrap();
    std::fs::write(tmp.path().join("keep.txt"), "").unwrap();

    let p = abs(&tmp, "");
    let out = run(json!({"path": p}), &ctx(tmp.path())).await.unwrap();
    assert!(out.ends_with("\n\n(1 git-ignored, 1 qwen-ignored)"));
}

#[tokio::test]
async fn caps_at_the_entry_limit_with_the_verbatim_trailer() {
    let tmp = TempDir::new().unwrap();
    for n in 0..(MAX_ENTRY_COUNT + 5) {
        std::fs::write(tmp.path().join(format!("f{n:04}.txt")), "").unwrap();
    }
    let p = abs(&tmp, "");
    let out = run(json!({"path": p.clone()}), &ctx(tmp.path()))
        .await
        .unwrap();
    assert!(out.starts_with(&format!(
        "Listed {} item(s) in {p}:\n---\n",
        MAX_ENTRY_COUNT + 5
    )));
    assert!(out.ends_with("\n---\n[5 items truncated] ..."));
    let body = out.split("\n---\n").nth(1).unwrap();
    assert_eq!(body.split('\n').count(), MAX_ENTRY_COUNT);
}

#[tokio::test]
async fn a_single_truncated_entry_says_item_singular() {
    let tmp = TempDir::new().unwrap();
    for n in 0..(MAX_ENTRY_COUNT + 1) {
        std::fs::write(tmp.path().join(format!("f{n:04}.txt")), "").unwrap();
    }
    let p = abs(&tmp, "");
    let out = run(json!({"path": p}), &ctx(tmp.path())).await.unwrap();
    assert!(out.ends_with("\n---\n[1 item truncated] ..."));
}

#[tokio::test]
async fn a_non_string_ignore_element_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let p = abs(&tmp, "");
    assert!(
        run(json!({"path": p, "ignore": [42]}), &ctx(tmp.path()))
            .await
            .is_err()
    );
}
