
use super::*;
use crate::tool::caps::Capabilities;
use crate::tool::read_cache::FileReadCache;
use std::sync::Arc;
use tempfile::TempDir;

// A ctx sharing a caller-supplied read cache, so a test can pre-populate the
// cache (record a prior read) and drive the enforcement.
fn ctx_with_cache(root: &std::path::Path, cache: Arc<FileReadCache>) -> ToolCtx {
    let caps = Capabilities::for_test_with_read_cache(cache);
    ToolCtx {
        root: root.to_path_buf(),
        result_cap: 100_000,
        command_timeout_ms: 120_000,
        input_modalities: crate::content::Modalities::default(),
        memory_root: None,
        session_dir: std::env::temp_dir(),
        caps,
    }
}

// Stat the file for its current (mtime, size) fingerprint.
fn fingerprint(abs: &std::path::Path) -> (u128, u64) {
    let meta = std::fs::metadata(abs).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    (mtime, meta.len())
}

// Write `body` to `name` under `root` and record a prior read of it in
// `cache`, so the read-before-edit gate is satisfied.
fn seed_read(
    root: &std::path::Path,
    cache: &FileReadCache,
    name: &str,
    body: &str,
) -> std::path::PathBuf {
    let abs = root.join(name);
    std::fs::write(&abs, body).unwrap();
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);
    abs
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    EditFile.run(&input, ctx).await
}

fn read(root: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}

// An absolute path to `rel` inside `root`, as a JSON string, since the tool
// now requires an absolute file_path (qwen contract).
fn abs(root: &std::path::Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().into_owned()
}

#[test]
fn spec_requires_file_path_old_string_new_string() {
    let spec = EditFile.spec();
    assert_eq!(spec.name, "edit");
    assert_eq!(
        spec.input_schema["required"],
        json!(["file_path", "old_string", "new_string"])
    );
    // The old relative `path` / `old_str` / `new_str` params are gone.
    assert!(spec.input_schema["properties"]["path"].is_null());
    assert!(spec.input_schema["properties"]["old_str"].is_null());
    assert!(spec.input_schema["properties"]["new_str"].is_null());
    // `replace_all` is present and optional (not in required).
    assert!(spec.input_schema["properties"]["replace_all"].is_object());
}

#[test]
fn description_is_the_verbatim_qwen_string_without_suspenders_additions() {
    let desc = EditFile.spec().description;
    assert!(
        desc.starts_with("Replaces text within a file. By default, replaces a single occurrence.")
    );
    assert!(desc.contains("Expectation for required parameters:"));
    assert!(desc.contains("**Multiple replacements:**"));
    // Interpolated ReadFileTool.Name is hardcoded.
    assert!(desc.contains("Always use the read_file tool"));
    // No suspenders-only additions survive.
    assert!(!desc.contains("relative to the project root"));
    assert!(!desc.contains("copied verbatim from read_file output"));
    assert!(!desc.contains("One change per call"));
    assert!(!desc.contains("whitespace normalized"));
    assert!(!desc.contains("write_file"));
}

// ---- single replacement (the default) ----

#[tokio::test]
async fn replaces_a_single_occurrence() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "foo bar baz");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let msg = run(
        json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "bar", "new_string": "qux"}),
        &ctx,
    )
    .await
    .unwrap();
    // The base update line plus qwen's edited-region snippet suffix.
    assert_eq!(
        msg,
        format!(
            "The file: {} has been updated. Showing lines 1-1 of 1 from the edited file:\n\n---\n\nfoo qux baz",
            abs(tmp.path(), "a.txt")
        )
    );
    assert_eq!(read(tmp.path(), "a.txt"), "foo qux baz");
}

#[tokio::test]
async fn matches_old_string_exactly_including_whitespace() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "code.ex", "def run do\n  :ok\nend\n");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            json!({"file_path": abs(tmp.path(), "code.ex"), "old_string": "  :ok\n", "new_string": "  :error\n"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "code.ex"), "def run do\n  :error\nend\n");
}

// ---- multiple-occurrence handling ----

#[tokio::test]
async fn two_occurrences_without_replace_all_is_the_verbatim_mismatch_error() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "foo bar foo");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "foo", "new_string": "baz"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "Failed to edit. Found 2 occurrences for old_string in {file_path} but replace_all was not enabled."
        )
    );
    // The file is untouched on the error path.
    assert_eq!(read(tmp.path(), "a.txt"), "foo bar foo");
}

#[tokio::test]
async fn replace_all_replaces_every_occurrence() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "foo bar foo");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "foo", "new_string": "baz", "replace_all": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "baz bar baz");
}

// ---- not found ----

#[tokio::test]
async fn zero_occurrences_is_the_verbatim_not_found_error() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "actual content");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "imaginary", "new_string": "x"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "Failed to edit, 0 occurrences found for old_string in {file_path}. No edits made. The exact text in old_string was not found. Ensure you're not escaping content incorrectly and check whitespace, indentation, and context. Use read_file tool to verify."
        )
    );
    assert_eq!(read(tmp.path(), "a.txt"), "actual content");
}

// ---- no-change ----

#[tokio::test]
async fn identical_old_and_new_string_is_the_verbatim_no_change_error() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "keep same keep");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "same", "new_string": "same"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "No changes to apply. The old_string and new_string are identical in file: {file_path}"
        )
    );
}

// ---- normalization fallback (qwen normalizeEditStrings) ----

#[tokio::test]
async fn a_curly_quote_in_old_string_matches_a_straight_quote_on_disk() {
    // The literal old_string is absent (curly apostrophe), but qwen's
    // character-equivalence pass maps it to a straight quote, matches, and
    // substitutes the canonical on-disk slice so the write lands on real
    // bytes.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "it's here");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            // U+2019 RIGHT SINGLE QUOTATION MARK in old_string.
            json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "it\u{2019}s here", "new_string": "it is here"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "it is here");
}

#[tokio::test]
async fn an_em_dash_in_old_string_matches_a_hyphen_on_disk() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "a - b");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            // U+2014 EM DASH in old_string maps to ASCII hyphen-minus.
            json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "a \u{2014} b", "new_string": "a plus b"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "a plus b");
}

#[tokio::test]
async fn line_matching_tolerates_trailing_whitespace_on_disk() {
    // The file line has trailing spaces the model did not send; qwen's
    // line-based trimEnd pass matches and rewrites the canonical (trailing-
    // whitespace-bearing) slice.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "x = 1   \ny = 2\n");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "x = 1\ny = 2", "new_string": "x = 3\ny = 2"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "x = 3\ny = 2\n");
}

// ---- deletion newline augmentation (qwen maybeAugmentOldStringForDeletion) ----

#[tokio::test]
async fn deleting_a_whole_line_consumes_its_trailing_newline() {
    // new_string is empty and old_string lacks a trailing newline, but the
    // file has old_string + "\n": the augmentation grows old_string by the
    // newline so no blank line is left behind.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "keep\nDROP\nkeep\n");
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
        json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "DROP", "new_string": ""}),
        &ctx,
    )
    .await
    .unwrap();
    // The whole "DROP\n" line is gone - no lingering blank line.
    assert_eq!(read(tmp.path(), "a.txt"), "keep\nkeep\n");
}

// ---- no-change after normalization (qwen's identical guards) ----

#[tokio::test]
async fn a_replacement_that_normalizes_to_a_no_op_is_a_no_change_error() {
    // old_string (curly quote) and new_string (straight quote) differ as raw
    // strings, but normalization canonicalizes old_string to the on-disk
    // "it's here", which equals new_string, so the edit is a no-op. qwen's
    // primary identical guard (final old == final new) catches it first with
    // its verbatim message. The secondary post-apply identical-content guard
    // is defensive backup that the primary guard shadows here.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "it's here");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
            json!({"file_path": &file_path, "old_string": "it\u{2019}s here", "new_string": "it's here"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "No changes to apply. The old_string and new_string are identical in file: {file_path}"
        )
    );
    assert_eq!(read(tmp.path(), "a.txt"), "it's here");
}

// ---- edited-region snippet suffix (qwen extractEditSnippet) ----

#[tokio::test]
async fn the_success_message_appends_the_edited_region_snippet() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    // Ten lines so the changed line 5 gets a bounded 4-line context window.
    let body = (1..=10)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    seed_read(tmp.path(), &cache, "a.txt", &body);
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let msg = run(
        json!({"file_path": &file_path, "old_string": "line5", "new_string": "CHANGED"}),
        &ctx,
    )
    .await
    .unwrap();
    // Change is on line 5; context is 4 lines each side -> lines 1-9 of 10.
    assert!(
            msg.starts_with(&format!(
                "The file: {file_path} has been updated. Showing lines 1-9 of 10 from the edited file:\n\n---\n\n"
            )),
            "unexpected message: {msg}"
        );
    assert!(msg.contains("CHANGED"));
    // The unchanged line 10 is beyond the context window, so it is elided.
    assert!(!msg.contains("line10"));
}

// ---- empty old_string = create a new file ----

#[tokio::test]
async fn empty_old_string_creates_a_new_file() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "fresh.txt");
    let msg = run(
        json!({"file_path": &file_path, "old_string": "", "new_string": "hello\nworld\n"}),
        &ctx,
    )
    .await
    .unwrap();
    // A new file quotes its whole content as the snippet.
    assert_eq!(
        msg,
        format!(
            "Created new file: {file_path} with provided content. Showing lines 1-3 of 3 from the edited file:\n\n---\n\nhello\nworld\n"
        )
    );
    assert_eq!(read(tmp.path(), "fresh.txt"), "hello\nworld\n");
}

#[tokio::test]
async fn a_padded_file_path_is_trimmed_before_validation_and_in_the_message() {
    // qwen unescapePath(file_path.trim()) (edit.ts): surrounding whitespace
    // is stripped, so a padded absolute path edits fine (and is echoed
    // trimmed in the message) rather than being rejected as relative.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "fresh.txt");
    let padded = format!("  {file_path}  ");
    let msg = run(
        json!({"file_path": padded, "old_string": "", "new_string": "hello\nworld\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(
        msg,
        format!(
            "Created new file: {file_path} with provided content. Showing lines 1-3 of 3 from the edited file:\n\n---\n\nhello\nworld\n"
        )
    );
    assert_eq!(read(tmp.path(), "fresh.txt"), "hello\nworld\n");
}

#[tokio::test]
async fn empty_old_string_on_an_existing_file_is_the_verbatim_create_conflict_error() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    seed_read(tmp.path(), &cache, "a.txt", "content");
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "", "new_string": "x"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!("File already exists, cannot create: {file_path}")
    );
    assert_eq!(read(tmp.path(), "a.txt"), "content");
}

// ---- editing a nonexistent file (non-empty old_string) ----

#[tokio::test]
async fn editing_a_missing_file_with_a_non_empty_old_string_is_not_found() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "nope.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "a", "new_string": "b"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err, format!("File not found: {file_path}"));
}

// ---- prior-read enforcement ----

#[tokio::test]
async fn editing_an_unread_existing_file_is_the_verbatim_prior_read_rejection() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "foo bar baz").unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "bar", "new_string": "qux"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "File {file_path} has not been read in this session. Use the read_file tool first to load the current content (a partial read with offset / limit is fine \u{2014} you only need to have seen the bytes you intend to edit) before editing it."
        )
    );
    // The unread file is left untouched.
    assert_eq!(read(tmp.path(), "a.txt"), "foo bar baz");
}

#[tokio::test]
async fn editing_a_stale_file_is_the_verbatim_modified_since_rejection() {
    let tmp = TempDir::new().unwrap();
    let abs_path = tmp.path().join("a.txt");
    std::fs::write(&abs_path, "foo bar baz").unwrap();
    let cache = Arc::new(FileReadCache::new());
    // Record a read at a DIFFERENT fingerprint than the file now has.
    cache.record_read(abs_path.clone(), 1, 1, true, true);
    let ctx = ctx_with_cache(tmp.path(), cache);

    let file_path = abs(tmp.path(), "a.txt");
    let err = run(
        json!({"file_path": &file_path, "old_string": "bar", "new_string": "qux"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "File {file_path} has been modified since you last read it (mtime or size changed). Re-read it with the read_file tool before editing it to ensure your changes are based on current content."
        )
    );
}

#[tokio::test]
async fn read_file_then_edit_shares_the_cache_and_passes() {
    // Drive read_file first (which records into the shared cache), then edit:
    // the read-before-edit contract is satisfied through the shared cache.
    let tmp = TempDir::new().unwrap();
    let abs_path = tmp.path().join("a.txt");
    std::fs::write(&abs_path, "foo bar baz").unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    // read_file now takes an absolute `file_path` (qwen contract).
    crate::tools::read_file::ReadFile
        .run_rich(&json!({"file_path": abs_path.to_string_lossy()}), &ctx)
        .await
        .unwrap();

    run(
        json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "bar", "new_string": "qux"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "foo qux baz");
}

// ---- read cache recording after a successful edit ----

#[tokio::test]
async fn a_successful_edit_records_the_write_so_a_second_edit_passes() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let abs_path = seed_read(tmp.path(), &cache, "a.txt", "foo bar baz");
    let ctx = ctx_with_cache(tmp.path(), Arc::clone(&cache));

    run(
        json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "bar", "new_string": "qux"}),
        &ctx,
    )
    .await
    .unwrap();

    // The write was recorded: the path reads Fresh at its new fingerprint, so
    // a second edit (which the model authored) passes without a re-read.
    let (mtime2, size2) = fingerprint(&abs_path);
    assert_eq!(cache.check(&abs_path, mtime2, size2), ReadState::Fresh);
    run(
        json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": "qux", "new_string": "zzz"}),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(read(tmp.path(), "a.txt"), "foo zzz baz");
}

#[tokio::test]
async fn a_created_file_records_the_write_so_an_immediate_edit_passes() {
    // Empty old_string creates a file; the model authored the bytes, so a
    // follow-up in-place edit passes without an explicit read (qwen's
    // recordWrite-seeds-prior-read note).
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    run(
            json!({"file_path": abs(tmp.path(), "made.txt"), "old_string": "", "new_string": "alpha beta"}),
            &ctx,
        )
        .await
        .unwrap();
    run(
            json!({"file_path": abs(tmp.path(), "made.txt"), "old_string": "beta", "new_string": "gamma"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(read(tmp.path(), "made.txt"), "alpha gamma");
}

// ---- path handling ----

#[tokio::test]
async fn a_relative_file_path_is_the_verbatim_absolute_required_message() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);
    let err = run(
        json!({"file_path": "a.txt", "old_string": "a", "new_string": "b"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err, "File path must be absolute: a.txt");
}

#[tokio::test]
async fn paths_escaping_the_project_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);
    assert_eq!(
        run(
            json!({"file_path": "/etc/passwd", "old_string": "root", "new_string": "x"}),
            &ctx
        )
        .await,
        Err("path escapes project root".into())
    );
}

#[tokio::test]
async fn edits_a_file_inside_the_trusted_memory_root() {
    // P5, ADR-0062: edit_file reaches a memory file through the shared path
    // seam, outside the Project Root, so the model can update its memory.
    let proj = TempDir::new().unwrap();
    let mem = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let mut c = ctx_with_cache(proj.path(), Arc::clone(&cache));
    c.memory_root = Some(mem.path().to_path_buf());

    let abs_path = mem.path().join("user.md");
    std::fs::write(&abs_path, "type: user\nold body").unwrap();
    let (mtime, size) = fingerprint(&abs_path);
    cache.record_read(abs_path.clone(), mtime, size, true, true);

    run(
            json!({"file_path": abs_path.to_str().unwrap(), "old_string": "old body", "new_string": "new body"}),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&abs_path).unwrap(),
        "type: user\nnew body"
    );
}

#[tokio::test]
async fn missing_or_non_string_arguments_are_a_structured_error() {
    let tmp = TempDir::new().unwrap();
    let c = ctx_with_cache(tmp.path(), Arc::new(FileReadCache::new()));
    assert!(
        crate::tools::execute("edit", &json!({"file_path": abs(tmp.path(), "a.txt")}), &c)
            .await
            .is_error
    );
    assert!(
        crate::tools::execute(
            "edit",
            &json!({"file_path": abs(tmp.path(), "a.txt"), "old_string": 1, "new_string": 2}),
            &c
        )
        .await
        .is_error
    );
}
