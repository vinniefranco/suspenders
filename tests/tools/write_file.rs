use super::*;
use tempfile::TempDir;

fn ctx(root: &std::path::Path) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), 10_000)
}

// An absolute path to `rel` inside `root`, as a JSON string, since the tool
// now requires an absolute file_path (qwen contract).
fn abs(root: &std::path::Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().into_owned()
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    WriteFile.run(&input, ctx).await
}

#[test]
fn spec_requires_file_path_and_content() {
    let spec = WriteFile.spec();
    assert_eq!(spec.name, "write_file");
    assert_eq!(
        spec.input_schema["required"],
        json!(["file_path", "content"])
    );
    // The old relative `path` param is gone.
    assert!(spec.input_schema["properties"]["path"].is_null());
    assert!(spec.input_schema["properties"]["file_path"].is_object());
}

#[test]
fn description_is_the_verbatim_qwen_string() {
    let spec = WriteFile.spec();
    let desc = spec.description;
    assert_eq!(
        desc,
        "Writes content to a specified file in the local filesystem. A request to create or generate a file does not establish that the target path is new. Unless the target's absence or current text contents have already been established in this session, you MUST use the read_file tool first; if the file does not exist, then create it. With prior-read enforcement enabled, blind overwrites are rejected. The file_path argument MUST be an absolute path. Always construct it by combining the project root with the file's relative path (e.g. project root '/path/to/project/' + relative 'foo/bar.txt' = '/path/to/project/foo/bar.txt'). If the user provides a relative path, resolve it against the project root first.\n\nThe user has the ability to modify `content`. If modified, this will be stated in the response."
    );
    // No suspenders-only additions.
    assert!(!desc.contains("Usage:"));
    assert!(!desc.contains("only creates new files"));
}

#[tokio::test]
async fn creates_a_new_file_with_the_verbatim_created_message() {
    let tmp = TempDir::new().unwrap();
    let target = abs(tmp.path(), "new.txt");
    let msg = run(
        json!({"file_path": &target, "content": "hello"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert_eq!(
        msg,
        format!("Successfully created and wrote to new file: {target}.")
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn a_padded_file_path_is_trimmed_before_validation() {
    // qwen unescapePath(file_path.trim()) (write-file.ts): surrounding
    // whitespace is stripped, so a padded absolute path writes fine (and is
    // echoed trimmed) rather than being rejected as relative.
    let tmp = TempDir::new().unwrap();
    let target = abs(tmp.path(), "only.txt");
    let padded = format!("  {target}  ");
    let msg = run(
        json!({"file_path": padded, "content": "body"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert_eq!(
        msg,
        format!("Successfully created and wrote to new file: {target}.")
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("only.txt")).unwrap(),
        "body"
    );
}

#[tokio::test]
async fn creates_missing_parent_directories() {
    let tmp = TempDir::new().unwrap();
    let target = abs(tmp.path(), "deeply/nested/dir/file.txt");
    let msg = run(
        json!({"file_path": &target, "content": "x"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert!(msg.starts_with("Successfully created and wrote to new file:"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("deeply/nested/dir/file.txt")).unwrap(),
        "x"
    );
}

#[tokio::test]
async fn overwrites_an_existing_file_with_the_verbatim_overwrite_message() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "old").unwrap();
    let target = abs(tmp.path(), "a.txt");

    let msg = run(
        json!({"file_path": &target, "content": "new"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap();
    assert_eq!(msg, format!("Successfully overwrote file: {target}."));
    // The overwrite actually replaced the bytes.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
        "new"
    );
}

#[tokio::test]
async fn empty_content_is_allowed() {
    let tmp = TempDir::new().unwrap();
    let target = abs(tmp.path(), "empty.txt");
    assert!(
        run(
            json!({"file_path": &target, "content": ""}),
            &ctx(tmp.path())
        )
        .await
        .is_ok()
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap(),
        ""
    );
}

#[tokio::test]
async fn writing_over_a_directory_is_an_error() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();
    let target = abs(tmp.path(), "somedir");

    let err = run(
        json!({"file_path": &target, "content": "x"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap_err();
    assert!(err.contains("directory"));
}

#[tokio::test]
async fn a_relative_path_is_the_verbatim_absolute_required_message() {
    let tmp = TempDir::new().unwrap();
    let err = run(
        json!({"file_path": "new.txt", "content": "x"}),
        &ctx(tmp.path()),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "File path must be absolute: new.txt");
}

#[tokio::test]
async fn paths_escaping_the_project_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    // An absolute path outside the root escapes.
    assert_eq!(
        run(
            json!({"file_path": "/tmp/absolute_escape.txt", "content": "x"}),
            &ctx(tmp.path())
        )
        .await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn writes_into_the_trusted_memory_root_outside_the_project_root() {
    // P5, ADR-0062: a memory-dir write is reachable through the shared path
    // seam, even though the memory root sits outside the Project Root.
    let proj = TempDir::new().unwrap();
    let mem = TempDir::new().unwrap();
    let mut ctx = ctx(proj.path());
    ctx.memory_root = Some(mem.path().to_path_buf());

    let target = mem.path().join("MEMORY.md");
    let msg = run(
        json!({"file_path": target.to_str().unwrap(), "content": "- [X](x.md) - hook"}),
        &ctx,
    )
    .await
    .unwrap();
    assert!(msg.starts_with("Successfully created and wrote to new file:"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "- [X](x.md) - hook"
    );
}

#[tokio::test]
async fn a_write_outside_both_the_project_and_memory_roots_is_refused() {
    let proj = TempDir::new().unwrap();
    let mem = TempDir::new().unwrap();
    let mut ctx = ctx(proj.path());
    ctx.memory_root = Some(mem.path().to_path_buf());

    // A sibling of the memory root that merely shares its string prefix.
    let evil = format!("{}-evil/x.md", mem.path().to_str().unwrap());
    assert_eq!(
        run(json!({"file_path": evil, "content": "x"}), &ctx).await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn a_successful_write_records_into_the_read_cache() {
    // qwen `recordWrite`: after a write, the path reads Fresh (the tool's own
    // write is not a stale external change) and is a full read the model has
    // "seen", so read_file's unchanged fast-path is eligible.
    use crate::tool::read_cache::ReadState;
    let tmp = TempDir::new().unwrap();
    let c = ctx(tmp.path());
    let target = abs(tmp.path(), "cached.txt");
    run(json!({"file_path": &target, "content": "body"}), &c)
        .await
        .unwrap();

    let path = std::path::Path::new(&target);
    let meta = std::fs::metadata(path).unwrap();
    assert_eq!(
        c.read_cache()
            .check(path, crate::tool::read_cache::Fingerprint::of(&meta)),
        ReadState::Fresh
    );
    assert!(c.read_cache().entry(path).unwrap().last_read_was_full);
}

#[tokio::test]
async fn missing_or_non_string_arguments_are_a_structured_error() {
    let tmp = TempDir::new().unwrap();
    let c = ctx(tmp.path());
    let target = abs(tmp.path(), "a.txt");
    assert!(
        crate::tools::execute("write_file", &json!({"file_path": target}), &c)
            .await
            .is_error
    );
    assert!(
        crate::tools::execute(
            "write_file",
            &json!({"file_path": abs(tmp.path(), "a.txt"), "content": 42}),
            &c
        )
        .await
        .is_error
    );
}
