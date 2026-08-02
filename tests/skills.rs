use super::*;
use tempfile::TempDir;

fn write_skill(root: &Path, name: &str, content: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

// The disk-sourced skills only (project + user), filtering out the compile-time
// bundled set (`batch`, `stuck`), so a test that wrote N disk skills can assert
// on exactly those without the always-present bundled skills skewing the count.
fn disk_skills(mgr: &SkillManager) -> Vec<&Skill> {
    mgr.available()
        .iter()
        .filter(|s| s.level != SkillLevel::Bundled)
        .collect()
}

// ---- parse_skill_content ----

#[test]
fn parses_valid_frontmatter_and_trims_the_body() {
    let text = "---\nname: pdf\ndescription: Work with PDF files\n---\n\n  Body text here.  \n";
    let (fm, body) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "pdf");
    assert_eq!(fm.description, "Work with PDF files");
    assert_eq!(fm.when_to_use, None);
    assert_eq!(body, "Body text here.");
}

#[test]
fn when_to_use_is_optional_and_parsed_when_present() {
    let text = "---\nname: pdf\ndescription: Work with PDFs\nwhen_to_use: When asked about a PDF\n---\nbody";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.when_to_use.as_deref(), Some("When asked about a PDF"));
}

#[test]
fn missing_name_is_an_error() {
    let text = "---\ndescription: no name here\n---\nbody";
    assert_eq!(
        parse_skill_content(text),
        Err("Missing \"name\" in frontmatter".to_string())
    );
}

#[test]
fn missing_description_is_an_error() {
    let text = "---\nname: pdf\n---\nbody";
    assert_eq!(
        parse_skill_content(text),
        Err("Missing \"description\" in frontmatter".to_string())
    );
}

#[test]
fn empty_name_value_counts_as_missing() {
    let text = "---\nname:\ndescription: has desc\n---\nbody";
    assert!(parse_skill_content(text).is_err());
}

#[test]
fn missing_frontmatter_fences_is_an_error() {
    let text = "no frontmatter at all\njust a body";
    assert_eq!(
        parse_skill_content(text),
        Err("Invalid format: missing YAML frontmatter".to_string())
    );
}

#[test]
fn inert_and_honored_fields_load_together() {
    // A real qwen SKILL.md with the inert `model`/`allowedTools` alongside the
    // honored `priority` + `paths` must still load off name + description; the
    // inert fields are dropped, the honored ones are parsed.
    let text = "---\nname: office\ndescription: Office suite\nmodel: fast\npriority: 5\nallowedTools:\n  - read_file\n  - grep\npaths:\n  - src/**\n---\nbody";
    let (fm, body) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "office");
    assert_eq!(fm.description, "Office suite");
    assert_eq!(body, "body");
    // The honored fields ARE parsed (not ignored).
    assert_eq!(fm.priority, 5);
    assert_eq!(fm.paths, vec!["src/**".to_string()]);
}

#[test]
fn frontmatter_may_end_at_eof_without_a_body() {
    let text = "---\nname: pdf\ndescription: desc\n---";
    let (fm, body) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "pdf");
    assert_eq!(body, "");
}

#[test]
fn quoted_scalar_values_are_unquoted() {
    let text = "---\nname: \"pdf\"\ndescription: 'Work with PDFs'\n---\nbody";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "pdf");
    assert_eq!(fm.description, "Work with PDFs");
}

#[test]
fn a_colon_in_the_description_value_survives() {
    let text = "---\nname: pdf\ndescription: Ratio 16:9 handling\n---\nbody";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.description, "Ratio 16:9 handling");
}

// ---- validate_skill_name ----

#[test]
fn name_charset_rejects_structurally_unsafe_characters() {
    assert!(validate_skill_name("pdf").is_ok());
    assert!(validate_skill_name("ms-office:pdf").is_ok());
    assert!(validate_skill_name("skill_v1.2").is_ok());
    // Non-ASCII letters keep working (qwen's \p{L}\p{N} charset).
    assert!(validate_skill_name("日本語").is_ok());
    // Structurally unsafe: injection vectors and whitespace.
    assert!(validate_skill_name("bad name").is_err());
    assert!(validate_skill_name("bad/name").is_err());
    assert!(validate_skill_name("<script>").is_err());
    assert!(validate_skill_name("a&b").is_err());
}

#[test]
fn a_rejected_name_carries_the_verbatim_qwen_reason() {
    assert_eq!(
            validate_skill_name("bad name"),
            Err(
                "\"name\" must match /^[\\p{L}\\p{N}_:.-]+$/u (letters, digits, _, :, ., -); got \"bad name\""
                    .to_string()
            )
        );
}

#[test]
fn a_bad_name_makes_the_whole_skill_a_parse_error() {
    let text = "---\nname: bad name\ndescription: desc\n---\nbody";
    assert!(parse_skill_content(text).is_err());
}

// ---- SkillManager::discover ----

#[test]
fn discover_finds_a_well_formed_skill() {
    let tmp = TempDir::new().unwrap();
    write_skill(
        tmp.path(),
        "pdf",
        "---\nname: pdf\ndescription: Work with PDFs\n---\nbody",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    let disk = disk_skills(&mgr);
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].name, "pdf");
    assert_eq!(disk[0].base_dir, tmp.path().join("pdf"));
    assert!(mgr.failures().is_empty());
}

#[test]
fn discover_skips_a_directory_without_a_manifest() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
    write_skill(
        tmp.path(),
        "pdf",
        "---\nname: pdf\ndescription: desc\n---\nbody",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    let disk = disk_skills(&mgr);
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].name, "pdf");
    // The manifest-less dir is a silent skip, not a failure.
    assert!(mgr.failures().is_empty());
}

#[test]
fn discover_records_a_malformed_manifest_as_a_failure() {
    let tmp = TempDir::new().unwrap();
    write_skill(tmp.path(), "broken", "no frontmatter here");
    let mgr = SkillManager::discover(tmp.path(), None);
    assert!(disk_skills(&mgr).is_empty());
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.failures()[0].0, "broken");
    assert!(mgr.failures()[0].1.contains("missing YAML frontmatter"));
}

#[test]
fn discover_records_a_missing_required_field_as_a_failure() {
    let tmp = TempDir::new().unwrap();
    write_skill(tmp.path(), "noname", "---\ndescription: desc\n---\nbody");
    let mgr = SkillManager::discover(tmp.path(), None);
    assert!(disk_skills(&mgr).is_empty());
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.failures()[0].0, "noname");
}

#[test]
fn a_missing_root_is_a_silent_no_op() {
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::discover(&tmp.path().join("does-not-exist"), None);
    // No disk skills, no failures - only the always-present bundled set remains.
    assert!(disk_skills(&mgr).is_empty());
    assert!(mgr.failures().is_empty());
}

#[test]
fn project_shadows_user_on_a_name_collision() {
    let project = TempDir::new().unwrap();
    let user = TempDir::new().unwrap();
    write_skill(
        project.path(),
        "pdf",
        "---\nname: pdf\ndescription: PROJECT pdf\n---\nproject body",
    );
    write_skill(
        user.path(),
        "pdf",
        "---\nname: pdf\ndescription: USER pdf\n---\nuser body",
    );
    let mgr = SkillManager::discover(project.path(), Some(user.path()));
    // Only the project skill survives; the user one is shadowed, not failed.
    let disk = disk_skills(&mgr);
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].description, "PROJECT pdf");
    assert_eq!(disk[0].level, SkillLevel::Project);
    assert!(mgr.failures().is_empty());
}

#[test]
fn user_skills_load_when_not_shadowed() {
    let project = TempDir::new().unwrap();
    let user = TempDir::new().unwrap();
    write_skill(
        project.path(),
        "pdf",
        "---\nname: pdf\ndescription: PROJECT\n---\nb",
    );
    write_skill(
        user.path(),
        "xlsx",
        "---\nname: xlsx\ndescription: USER\n---\nb",
    );
    let mgr = SkillManager::discover(project.path(), Some(user.path()));
    let mut names: Vec<&str> = disk_skills(&mgr).iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["pdf", "xlsx"]);
}

// ---- build_skill_llm_content ----

#[test]
fn build_skill_llm_content_is_verbatim() {
    let out = build_skill_llm_content(Path::new("/tmp/skills/pdf"), "Do the thing.");
    assert_eq!(
        out,
        "Base directory for this skill: /tmp/skills/pdf\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\nDo the thing.\n"
    );
}

// ---- escape_xml ----

#[test]
fn escape_xml_covers_all_five_metacharacters() {
    assert_eq!(
        escape_xml("<a & b> \"c\" 'd'"),
        "&lt;a &amp; b&gt; &quot;c&quot; &apos;d&apos;"
    );
}

// ---- priority ----

#[test]
fn priority_parses_as_an_integer() {
    let text = "---\nname: p\ndescription: d\npriority: 7\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.priority, 7);
}

#[test]
fn a_missing_or_invalid_priority_normalizes_to_zero() {
    // Missing.
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\n---\nb").unwrap();
    assert_eq!(fm.priority, 0);
    // Non-integer: the skill still loads (priority is a non-fatal ordering hint).
    let (fm, _) =
        parse_skill_content("---\nname: p\ndescription: d\npriority: high\n---\nb").unwrap();
    assert_eq!(fm.priority, 0);
    // Empty.
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\npriority:\n---\nb").unwrap();
    assert_eq!(fm.priority, 0);
}

#[test]
fn the_catalog_sorts_by_priority_desc_then_name() {
    let tmp = TempDir::new().unwrap();
    // Distinct priorities plus a tie to exercise the alphabetical tiebreak.
    write_skill(tmp.path(), "low", "---\nname: low\ndescription: d\npriority: 1\n---\nb");
    write_skill(
        tmp.path(),
        "high",
        "---\nname: high\ndescription: d\npriority: 9\n---\nb",
    );
    write_skill(
        tmp.path(),
        "mid_b",
        "---\nname: mid_b\ndescription: d\npriority: 5\n---\nb",
    );
    write_skill(
        tmp.path(),
        "mid_a",
        "---\nname: mid_a\ndescription: d\npriority: 5\n---\nb",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    let order: Vec<&str> = disk_skills(&mgr).iter().map(|s| s.name.as_str()).collect();
    // 9, then the two 5s alphabetically (mid_a before mid_b), then 1.
    assert_eq!(order, vec!["high", "mid_a", "mid_b", "low"]);
}

// ---- argument-hint ----

#[test]
fn argument_hint_parses_when_present() {
    let text = "---\nname: commit\ndescription: d\nargument-hint: '<message>'\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.argument_hint.as_deref(), Some("<message>"));
}

#[test]
fn argument_hint_is_none_when_absent() {
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\n---\nb").unwrap();
    assert_eq!(fm.argument_hint, None);
}

// ---- disable-model-invocation ----

#[test]
fn disable_model_invocation_parses_the_flag() {
    let text = "---\nname: p\ndescription: d\ndisable-model-invocation: true\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert!(fm.disable_model_invocation);
    // Absent defaults to false.
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\n---\nb").unwrap();
    assert!(!fm.disable_model_invocation);
}

#[test]
fn a_disable_model_invocation_skill_is_dropped_from_the_catalog() {
    let tmp = TempDir::new().unwrap();
    write_skill(
        tmp.path(),
        "hidden",
        "---\nname: hidden\ndescription: d\ndisable-model-invocation: true\n---\nb",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    // It IS discovered (available), but NOT in the model-facing catalog.
    assert!(mgr.available().iter().any(|s| s.name == "hidden"));
    assert!(!mgr.catalog().iter().any(|s| s.name == "hidden"));
}

// ---- paths ----

#[test]
fn paths_parses_a_glob_list() {
    let text = "---\nname: p\ndescription: d\npaths:\n  - src/**\n  - '*.rs'\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.paths, vec!["src/**".to_string(), "*.rs".to_string()]);
}

#[test]
fn an_absent_or_empty_paths_is_a_plain_skill() {
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\n---\nb").unwrap();
    assert!(fm.paths.is_empty());
    // `paths:` with no value is unconditional, not a parse error.
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\npaths:\n---\nb").unwrap();
    assert!(fm.paths.is_empty());
}

#[test]
fn a_root_escaping_paths_glob_is_dropped() {
    // Absolute and `..`-escaping globs are project-unscoped and dropped, so the
    // skill loads as unconditional rather than carrying a glob that can never
    // match a project-relative path.
    let text = "---\nname: p\ndescription: d\npaths:\n  - /etc/passwd\n  - ../secret\n  - src/**\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    assert_eq!(fm.paths, vec!["src/**".to_string()]);
}

// ---- hooks ----

#[test]
fn a_nested_hooks_block_parses_to_the_hook_value_shape() {
    let text = "---\nname: p\ndescription: d\nhooks:\n  PreToolUse:\n    - matcher: edit\n      hooks:\n        - type: command\n          command: echo hi\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    let hooks = fm.hooks.expect("hooks parsed");
    // The mapping is the same event -> definitions shape the HookManager consumes.
    let defs = hooks
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .expect("PreToolUse definitions");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].get("matcher").and_then(|v| v.as_str()), Some("edit"));
}

#[test]
fn a_malformed_hooks_block_fails_open() {
    // A `hooks:` block that will not parse as YAML leaves the skill loading off
    // name + description with `hooks == None` (fail-open, ADR-0058).
    let text = "---\nname: p\ndescription: d\nhooks:\n  - : : : not valid\n---\nb";
    let (fm, _) = parse_skill_content(text).unwrap();
    // The skill still loaded; hooks may be None (malformed) - the point is no panic
    // and no dropped skill.
    assert_eq!(fm.name, "p");
}

#[test]
fn no_hooks_block_yields_none() {
    let (fm, _) = parse_skill_content("---\nname: p\ndescription: d\n---\nb").unwrap();
    assert_eq!(fm.hooks, None);
}

// ---- conditional activation ----

#[test]
fn a_conditional_skill_is_hidden_until_activated_then_sticky() {
    let tmp = TempDir::new().unwrap();
    write_skill(
        tmp.path(),
        "rusty",
        "---\nname: rusty\ndescription: d\npaths:\n  - src/**\n---\nb",
    );
    let mgr = SkillManager::discover(tmp.path(), None);

    // Discovered but NOT in the catalog before any file is touched.
    assert!(mgr.available().iter().any(|s| s.name == "rusty"));
    assert!(!mgr.catalog().iter().any(|s| s.name == "rusty"));

    // A touch of a NON-matching path does not activate it.
    let none = mgr.activate_by_path(Path::new("docs/readme.md"), tmp.path());
    assert!(none.is_empty());
    assert!(!mgr.catalog().iter().any(|s| s.name == "rusty"));

    // A touch of a MATCHING path activates it, and it appears in the catalog.
    let newly = mgr.activate_by_path(Path::new("src/lib.rs"), tmp.path());
    assert_eq!(newly, vec!["rusty".to_string()]);
    assert!(mgr.catalog().iter().any(|s| s.name == "rusty"));

    // Sticky: a second matching touch reports nothing newly activated (already
    // active), and it stays in the catalog.
    let again = mgr.activate_by_path(Path::new("src/other.rs"), tmp.path());
    assert!(again.is_empty());
    assert!(mgr.catalog().iter().any(|s| s.name == "rusty"));
}

#[test]
fn activation_accepts_an_absolute_path_under_the_project_root() {
    let tmp = TempDir::new().unwrap();
    write_skill(
        tmp.path(),
        "rusty",
        "---\nname: rusty\ndescription: d\npaths:\n  - src/**\n---\nb",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    // The touched path arrives as an absolute path (as read_file/edit report it);
    // it is resolved relative to the project root before matching.
    let abs = tmp.path().join("src").join("main.rs");
    let newly = mgr.activate_by_path(&abs, tmp.path());
    assert_eq!(newly, vec!["rusty".to_string()]);
}

#[test]
fn a_path_outside_the_project_root_never_activates() {
    let tmp = TempDir::new().unwrap();
    write_skill(
        tmp.path(),
        "rusty",
        "---\nname: rusty\ndescription: d\npaths:\n  - '**/*.rs'\n---\nb",
    );
    let mgr = SkillManager::discover(tmp.path(), None);
    // An absolute path clearly outside the project root is project-unscoped.
    let outside = Path::new("/somewhere/else/foo.rs");
    let newly = mgr.activate_by_path(outside, tmp.path());
    assert!(newly.is_empty());
    assert!(!mgr.catalog().iter().any(|s| s.name == "rusty"));
}

// ---- bundled skills ----

#[test]
fn the_bundled_skills_load_at_the_bundled_level() {
    // No disk roots at all: only the compile-time bundled set is present.
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::discover(&tmp.path().join("nope"), None);
    let bundled: Vec<&str> = mgr
        .available()
        .iter()
        .filter(|s| s.level == SkillLevel::Bundled)
        .map(|s| s.name.as_str())
        .collect();
    assert!(bundled.contains(&"batch"));
    assert!(bundled.contains(&"stuck"));
    // Both are model-invocable (no disable-model-invocation), so both catalog.
    assert!(mgr.catalog().iter().any(|s| s.name == "batch"));
    assert!(mgr.catalog().iter().any(|s| s.name == "stuck"));
    assert!(mgr.failures().is_empty());
}

#[test]
fn a_project_or_user_skill_shadows_a_same_named_bundled_skill() {
    let project = TempDir::new().unwrap();
    let user = TempDir::new().unwrap();
    // A project `batch` shadows the bundled `batch`; a user `stuck` shadows the
    // bundled `stuck`. The shadowed bundled copies are dropped silently.
    write_skill(
        project.path(),
        "batch",
        "---\nname: batch\ndescription: PROJECT batch\n---\nproject body",
    );
    write_skill(
        user.path(),
        "stuck",
        "---\nname: stuck\ndescription: USER stuck\n---\nuser body",
    );
    let mgr = SkillManager::discover(project.path(), Some(user.path()));

    let batch = mgr.find("batch").expect("batch present");
    assert_eq!(batch.level, SkillLevel::Project);
    assert_eq!(batch.description, "PROJECT batch");

    let stuck = mgr.find("stuck").expect("stuck present");
    assert_eq!(stuck.level, SkillLevel::User);
    assert_eq!(stuck.description, "USER stuck");

    // Neither name appears twice (the bundled copy was shadowed, not duplicated).
    assert_eq!(
        mgr.available().iter().filter(|s| s.name == "batch").count(),
        1
    );
    assert_eq!(
        mgr.available().iter().filter(|s| s.name == "stuck").count(),
        1
    );
    assert!(mgr.failures().is_empty());
}

#[test]
fn the_bundled_batch_skill_references_the_agent_tool_not_task() {
    // The audit: qwen's bundled batch says `task`; suspenders renamed it to
    // `agent`. Confirm the embedded body cites the real suspenders tool.
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::discover(&tmp.path().join("nope"), None);
    let batch = mgr.find("batch").expect("bundled batch");
    assert!(batch.body.contains("`agent`"));
    // And does NOT tell the model to call a `task` tool that does not exist here.
    assert!(!batch.body.contains("`task`"));
}

// ---- malformed new fields fail open ----

#[test]
fn malformed_new_fields_still_load_the_skill() {
    // A junk `priority`, a `paths:` with a bad shape, and a broken `hooks:` block
    // all together: the skill still loads off its name + description (fail-open,
    // ADR-0058), with the honored fields degraded to their empty defaults.
    let text = "---\nname: survivor\ndescription: still here\npriority: not-a-number\npaths: not-a-list\nhooks: also-not-a-block\n---\nbody";
    let (fm, body) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "survivor");
    assert_eq!(fm.description, "still here");
    assert_eq!(body, "body");
    assert_eq!(fm.priority, 0);
    assert!(fm.paths.is_empty());
    assert_eq!(fm.hooks, None);
}
