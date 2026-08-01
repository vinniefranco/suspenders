
use super::*;

// The memory-subtree containment check lives in `tool::path` (its
// `resolve_path_in` tests), not here - this module owns only the paths.

// ---- prompt builder: VERBATIM content + no em-dashes ----

// The em-dash the house rule forbids (U+2014). The ported prompt must carry
// none of them.
const EM_DASH: char = '\u{2014}';

fn empty_prompt() -> String {
    build_managed_auto_memory_prompt("/mem", None)
}

#[test]
fn the_prompt_carries_no_em_dashes() {
    // A deviation from verbatim: every U+2014 became a hyphen (the other is
    // the U+2192 arrow that became `->`).
    let full = build_managed_auto_memory_prompt("/mem", Some("- [X](x.md) - hook"));
    assert!(
        !full.contains(EM_DASH),
        "ported prompt must not contain an em-dash (U+2014)"
    );
}

#[test]
fn the_prompt_contains_each_type_block() {
    let p = empty_prompt();
    assert!(p.contains("<name>user</name>"));
    assert!(p.contains("<name>feedback</name>"));
    assert!(p.contains("<name>project</name>"));
    assert!(p.contains("<name>reference</name>"));
}

#[test]
fn the_prompt_contains_the_frontmatter_example() {
    let p = empty_prompt();
    assert!(p.contains("type: {{user, feedback, project, reference}}"));
    assert!(p.contains("name: {{memory name}}"));
}

#[test]
fn the_prompt_interpolates_the_memory_dir_at_every_save_protocol_site() {
    let p = build_managed_auto_memory_prompt("/some/mem/dir", None);
    // The header line.
    assert!(p.contains("persistent, file-based memory system at `/some/mem/dir`"));
    // Step 2's index pointer + the "never write directly" clause.
    assert!(p.contains("add a pointer to that file in `/some/mem/dir/MEMORY.md`"));
    assert!(p.contains("Never write memory content directly into `/some/mem/dir/MEMORY.md`"));
    // The always-loaded truncation note.
    assert!(p.contains("`/some/mem/dir/MEMORY.md` is always loaded"));
    // The index heading.
    assert!(p.contains("## /some/mem/dir/MEMORY.md"));
}

#[test]
fn the_prompt_contains_the_two_step_save_protocol() {
    let p = empty_prompt();
    assert!(p.contains("Saving a memory is a two-step process:"));
    assert!(p.contains("**Step 1** - write the memory to its own file"));
    assert!(p.contains("**Step 2** - add a pointer to that file"));
}

#[test]
fn the_prompt_contains_what_not_to_save() {
    let p = empty_prompt();
    assert!(p.contains("## What NOT to save in memory"));
    assert!(p.contains("Git history, recent changes, or who-changed-what"));
}

#[test]
fn the_prompt_contains_recall_verification() {
    let p = empty_prompt();
    assert!(p.contains("## Before recommending from memory"));
    assert!(p.contains("is not the same as"));
}

#[test]
fn the_prompt_contains_the_persistence_vs_plans_guidance() {
    let p = empty_prompt();
    assert!(p.contains("## Memory and other forms of persistence"));
    assert!(p.contains("use a Plan rather than saving this information to memory"));
    assert!(p.contains("use tasks instead of saving to memory"));
}

#[test]
fn the_empty_index_shows_the_placeholder() {
    let p = empty_prompt();
    assert!(p.contains(
        "Your MEMORY.md is currently empty. When you save new memories, they will appear here."
    ));
}

#[test]
fn a_present_index_body_lands_in_the_prompt() {
    let p = build_managed_auto_memory_prompt(
        "/mem",
        Some("- [Testing](feedback_testing.md) - use a real db"),
    );
    assert!(p.contains("- [Testing](feedback_testing.md) - use a real db"));
    assert!(!p.contains("currently empty"));
}

// ---- MemoryStore::load (fail-open) ----
//
// These tests redirect the store to a temp base via SUSPENDERS_MEMORY_BASE_DIR
// and clear it afterward. Safe under nextest (process-per-test) and
// `--test-threads=1` (serial): each test owns the var for its body and spawns
// no threads.

fn with_temp_memory_base<T>(f: impl FnOnce(&str) -> T) -> T {
    let base = std::env::temp_dir().join(format!(
        "suspenders_mem_store_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    // SAFETY: process-per-test / serial single-threaded.
    unsafe {
        std::env::remove_var("SUSPENDERS_MEMORY_LOCAL");
        std::env::set_var("SUSPENDERS_MEMORY_BASE_DIR", &base);
    }
    // A project root with no `.git` ancestor keeps the resolver deterministic
    // (it slugs the root itself under the temp base).
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let out = f(&project.to_string_lossy());
    // SAFETY: process-per-test / serial single-threaded.
    unsafe { std::env::remove_var("SUSPENDERS_MEMORY_BASE_DIR") };
    let _ = std::fs::remove_dir_all(&base);
    out
}

#[test]
fn load_absent_index_is_none_and_the_suffix_shows_the_placeholder() {
    with_temp_memory_base(|project| {
        let store = MemoryStore::load(project);
        assert_eq!(store.index, None);
        let suffix = store.prompt_suffix();
        // The suffix leads with the `\n\n---\n\n` join.
        assert!(suffix.starts_with("\n\n---\n\n"));
        assert!(suffix.contains("Your MEMORY.md is currently empty."));
    });
}

#[test]
fn load_with_a_memory_md_puts_the_index_body_in_the_suffix() {
    with_temp_memory_base(|project| {
        let store = MemoryStore::load(project);
        // Scaffold and write an index the way the model would.
        store.ensure_scaffold().unwrap();
        std::fs::write(
            index_path(&memory_root(project)),
            "- [Testing](feedback_testing.md) - use a real db",
        )
        .unwrap();

        let reloaded = MemoryStore::load(project);
        assert_eq!(
            reloaded.index.as_deref(),
            Some("- [Testing](feedback_testing.md) - use a real db")
        );
        let suffix = reloaded.prompt_suffix();
        assert!(suffix.contains("- [Testing](feedback_testing.md) - use a real db"));
        assert!(!suffix.contains("currently empty"));
    });
}

#[test]
fn an_empty_memory_md_is_treated_as_absent_fail_open() {
    with_temp_memory_base(|project| {
        let store = MemoryStore::load(project);
        store.ensure_scaffold().unwrap();
        // A whitespace-only file is fail-open empty.
        std::fs::write(index_path(&memory_root(project)), "   \n\n").unwrap();

        let reloaded = MemoryStore::load(project);
        assert_eq!(reloaded.index, None);
        assert!(reloaded.prompt_suffix().contains("currently empty"));
    });
}

#[test]
fn ensure_scaffold_mkdirs_the_memory_dir_only() {
    with_temp_memory_base(|project| {
        let store = MemoryStore::load(project);
        store.ensure_scaffold().unwrap();
        assert!(std::path::Path::new(&store.memory_dir).is_dir());
        // No meta.json / extract-cursor (the deferred pipeline is dropped).
        assert!(
            !std::path::Path::new(&store.memory_dir)
                .join("meta.json")
                .exists()
        );
    });
}

// ---- truncate ----

#[test]
fn under_cap_index_passes_through_untouched() {
    let body = "- [A](a.md) - one\n- [B](b.md) - two";
    assert_eq!(truncate_managed_auto_memory_index(body), body);
}

#[test]
fn over_the_line_cap_emits_the_warning_footer() {
    let body: String = (0..250)
        .map(|i| format!("- [E{i}](e{i}.md) - hook"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = truncate_managed_auto_memory_index(&body);
    assert!(out.contains("> WARNING: MEMORY.md is"));
    assert!(out.contains("250 lines (limit: 200)"));
    // Only the first 200 lines survive.
    assert!(out.contains("- [E0](e0.md)"));
    assert!(!out.contains("- [E249](e249.md)"));
}

#[test]
fn over_the_byte_cap_emits_the_warning_footer_naming_kb() {
    // A handful of very long lines: under the 200-line cap but over 25 KB.
    let long = "x".repeat(5_000);
    let body: String = (0..10)
        .map(|i| format!("- [L{i}]({long})"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = truncate_managed_auto_memory_index(&body);
    assert!(out.contains("> WARNING: MEMORY.md is"));
    assert!(out.contains("KB (limit: 24.4 KB) - index entries are too long"));
}
