use super::*;
use tempfile::TempDir;

fn write_skill(root: &Path, name: &str, content: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
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
fn unknown_and_list_fields_are_parsed_and_ignored() {
    // A real qwen SKILL.md with allowedTools (a list), model, priority, and
    // paths must still load off name + description.
    let text = "---\nname: office\ndescription: Office suite\nmodel: fast\npriority: 5\nallowedTools:\n  - read_file\n  - grep\npaths:\n  - src/**\n---\nbody";
    let (fm, body) = parse_skill_content(text).unwrap();
    assert_eq!(fm.name, "office");
    assert_eq!(fm.description, "Office suite");
    assert_eq!(body, "body");
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
    assert_eq!(mgr.available().len(), 1);
    assert_eq!(mgr.available()[0].name, "pdf");
    assert_eq!(mgr.available()[0].base_dir, tmp.path().join("pdf"));
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
    assert_eq!(mgr.available().len(), 1);
    assert_eq!(mgr.available()[0].name, "pdf");
    // The manifest-less dir is a silent skip, not a failure.
    assert!(mgr.failures().is_empty());
}

#[test]
fn discover_records_a_malformed_manifest_as_a_failure() {
    let tmp = TempDir::new().unwrap();
    write_skill(tmp.path(), "broken", "no frontmatter here");
    let mgr = SkillManager::discover(tmp.path(), None);
    assert!(mgr.available().is_empty());
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.failures()[0].0, "broken");
    assert!(mgr.failures()[0].1.contains("missing YAML frontmatter"));
}

#[test]
fn discover_records_a_missing_required_field_as_a_failure() {
    let tmp = TempDir::new().unwrap();
    write_skill(tmp.path(), "noname", "---\ndescription: desc\n---\nbody");
    let mgr = SkillManager::discover(tmp.path(), None);
    assert!(mgr.available().is_empty());
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.failures()[0].0, "noname");
}

#[test]
fn a_missing_root_is_a_silent_no_op() {
    let tmp = TempDir::new().unwrap();
    let mgr = SkillManager::discover(&tmp.path().join("does-not-exist"), None);
    assert!(mgr.available().is_empty());
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
    assert_eq!(mgr.available().len(), 1);
    assert_eq!(mgr.available()[0].description, "PROJECT pdf");
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
    let names: Vec<&str> = mgr.available().iter().map(|s| s.name.as_str()).collect();
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
