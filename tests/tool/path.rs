
use super::*;
use std::fs;

struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "suspenders_tool_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        fs::create_dir_all(&path).unwrap();
        TmpDir { path }
    }

    fn ctx(&self) -> ToolCtx {
        ToolCtx::for_test(self.path.clone(), 4000)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---- with_path/3 ----

#[test]
fn with_path_resolves_relative_path_against_root() {
    let tmp = TmpDir::new();
    let ctx = tmp.ctx();
    let expected = tmp.path.join("sub/file.txt");

    let result = with_path("sub/file.txt", &ctx, |abs| {
        assert_eq!(abs, expected.as_path());
        Ok("resolved".to_string())
    });

    assert_eq!(result, Ok("resolved".to_string()));
}

#[test]
fn with_path_refuses_escaping_path_without_calling_fun() {
    let tmp = TmpDir::new();
    let ctx = tmp.ctx();

    let result = with_path("../../etc/passwd", &ctx, |_abs| {
        panic!("must not run");
    });

    assert_eq!(result, Err("path escapes project root".to_string()));
}

#[test]
fn with_path_non_string_is_not_applicable_but_resolve_rejects_escape() {
    // The Elixir "non-string path" case is enforced by resolve_path's
    // guard clause; in Rust the type system enforces `&str`, so the
    // remaining behavioral guarantee is the escape refusal above. A
    // resolve of a plainly-escaping path stands in for the structured error.
    let tmp = TmpDir::new();
    assert_eq!(
        resolve_path("/etc/passwd", &tmp.path),
        Err("path escapes project root".to_string())
    );
}

// ---- resolve_path_in: the trusted-memory allowance (P5, ADR-0062) ----

#[test]
fn without_a_memory_root_out_of_root_is_refused_unchanged() {
    // The Project-Root-only behavior is preserved when there is no memory.
    let root = Path::new("/proj");
    assert_eq!(
        resolve_path_in("/etc/passwd", root, None),
        Err("path escapes project root".to_string())
    );
}

#[test]
fn a_path_inside_the_memory_root_is_accepted() {
    let root = Path::new("/proj");
    let mem = Path::new("/data/projects/slug/memory");
    // An absolute path landing inside the trusted memory subtree resolves,
    // even though it is far outside the Project Root.
    assert_eq!(
        resolve_path_in("/data/projects/slug/memory/MEMORY.md", root, Some(mem)),
        Ok(PathBuf::from("/data/projects/slug/memory/MEMORY.md"))
    );
    // The memory root itself is inside.
    assert_eq!(
        resolve_path_in("/data/projects/slug/memory", root, Some(mem)),
        Ok(PathBuf::from("/data/projects/slug/memory"))
    );
}

#[test]
fn a_sibling_sharing_the_memory_string_prefix_is_still_refused() {
    // SECURITY: `<memory_root>-evil` shares the string prefix but not the
    // path-component boundary - the trailing-separator check refuses it.
    let root = Path::new("/proj");
    let mem = Path::new("/data/projects/slug/memory");
    assert_eq!(
        resolve_path_in("/data/projects/slug/memory-evil/x.md", root, Some(mem)),
        Err("path escapes project root".to_string())
    );
}

#[test]
fn a_filesystem_root_memory_boundary_does_not_widen_confinement() {
    // DEFENSE-IN-DEPTH: a memory boundary that normalized to `/` (or too
    // few components) is IGNORED - it must not open the whole filesystem.
    // `/etc/passwd` is outside the Project Root and would be "contained" by a
    // `/` boundary, so the guard is the only thing refusing it here.
    let root = Path::new("/proj");
    let fs_root = Path::new("/");
    assert_eq!(
        resolve_path_in("/etc/passwd", root, Some(fs_root)),
        Err("path escapes project root".to_string())
    );
    // A one-component boundary is below the minimum too.
    let shallow = Path::new("/data");
    assert_eq!(
        resolve_path_in("/etc/passwd", root, Some(shallow)),
        Err("path escapes project root".to_string())
    );
}

#[test]
fn a_dotdot_escape_out_of_the_memory_root_is_refused() {
    let root = Path::new("/proj");
    let mem = Path::new("/data/projects/slug/memory");
    // Climbing out of the memory subtree lands nowhere trusted.
    assert_eq!(
        resolve_path_in("/data/projects/slug/memory/../secret", root, Some(mem)),
        Err("path escapes project root".to_string())
    );
}

// ---- resolve_absolute_in: qwen's absolute-required contract ----

#[test]
fn resolve_absolute_refuses_a_relative_path() {
    // qwen's file tools require an absolute path; a relative one is a typed
    // Relative rejection the tool renders as its own verbatim message.
    let root = Path::new("/proj");
    assert_eq!(
        resolve_absolute_in("sub/file.txt", root, None),
        Err(PathReject::Relative)
    );
    assert_eq!(
        resolve_absolute_in("./file.txt", root, None),
        Err(PathReject::Relative)
    );
    // Empty string is relative, so it folds into Relative - callers need no
    // separate non-empty guard before the absolute check.
    assert_eq!(
        resolve_absolute_in("", root, None),
        Err(PathReject::Relative)
    );
}

#[test]
fn resolve_absolute_accepts_an_absolute_path_inside_the_root() {
    let root = Path::new("/proj");
    assert_eq!(
        resolve_absolute_in("/proj/src/lib.rs", root, None),
        Ok(PathBuf::from("/proj/src/lib.rs"))
    );
    // The root itself is contained.
    assert_eq!(
        resolve_absolute_in("/proj", root, None),
        Ok(PathBuf::from("/proj"))
    );
}

#[test]
fn resolve_absolute_refuses_an_absolute_path_outside_the_root() {
    let root = Path::new("/proj");
    assert_eq!(
        resolve_absolute_in("/etc/passwd", root, None),
        Err(PathReject::Escapes)
    );
    // A sibling sharing the string prefix but not the component boundary.
    assert_eq!(
        resolve_absolute_in("/proj-evil/x", root, None),
        Err(PathReject::Escapes)
    );
    // A `..` climb out of the root, detected lexically.
    assert_eq!(
        resolve_absolute_in("/proj/../etc/passwd", root, None),
        Err(PathReject::Escapes)
    );
}

#[test]
fn resolve_absolute_honors_the_trusted_memory_subtree() {
    let root = Path::new("/proj");
    let mem = Path::new("/data/projects/slug/memory");
    assert_eq!(
        resolve_absolute_in("/data/projects/slug/memory/MEMORY.md", root, Some(mem)),
        Ok(PathBuf::from("/data/projects/slug/memory/MEMORY.md"))
    );
    // A sibling of the memory root is still refused.
    assert_eq!(
        resolve_absolute_in("/data/projects/slug/memory-evil/x", root, Some(mem)),
        Err(PathReject::Escapes)
    );
    // A filesystem-root memory boundary does not widen confinement.
    assert_eq!(
        resolve_absolute_in("/etc/passwd", root, Some(Path::new("/"))),
        Err(PathReject::Escapes)
    );
}

// ---- unescape_and_trim ----

#[test]
fn unescape_and_trim_strips_surrounding_whitespace() {
    assert_eq!(unescape_and_trim("  /a/b  "), "/a/b");
    assert_eq!(unescape_and_trim("\t/a/b\n"), "/a/b");
}

#[test]
fn unescape_and_trim_removes_shell_escapes() {
    // A backslash before a shell-special char is dropped.
    assert_eq!(unescape_and_trim(r"/a/my\ dir"), "/a/my dir");
    assert_eq!(unescape_and_trim(r"/a/b\(1\)"), "/a/b(1)");
    assert_eq!(unescape_and_trim(r"/a/\*star"), "/a/*star");
}

#[test]
fn unescape_and_trim_leaves_non_escape_backslashes() {
    // A backslash before a non-special char (or at the end) is literal.
    assert_eq!(unescape_and_trim(r"/a/b\c"), r"/a/b\c");
    assert_eq!(unescape_and_trim(r"/a/b\"), r"/a/b\");
}

// ---- file_error/3 ----

#[test]
fn file_error_formats_the_posix_reason() {
    assert_eq!(
        file_error("write", "a.txt", FileError::Eacces),
        "could not write a.txt: eacces (permission denied)"
    );
}

#[test]
fn file_error_appends_closest_match_suggestions_on_enoent() {
    let tmp = TmpDir::new();
    fs::write(tmp.path.join("config.exs"), "").unwrap();
    let missing = tmp.path.join("confg.exs");
    let missing = missing.to_string_lossy().into_owned();

    let message = file_error("read", &missing, FileError::Enoent);
    assert!(message.contains(&format!("could not read {missing}: enoent")));
    assert!(message.contains("config.exs"));
}
