use super::*;
use crate::skills::SkillManager;
use tempfile::TempDir;

/// A manager over a temp dir seeded with the given `(name, description,
/// body)` skills, returned alongside the temp dir so the skill base dirs stay
/// on disk for the duration of the test.
fn manager_with(skills: &[(&str, &str, &str)]) -> (Arc<SkillManager>, TempDir) {
    let tmp = TempDir::new().unwrap();
    for (name, description, body) in skills {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }
    let mgr = SkillManager::discover(tmp.path(), None);
    (Arc::new(mgr), tmp)
}

fn ctx() -> ToolCtx {
    ToolCtx::for_test(std::env::temp_dir(), 100_000)
}

/// A manager over a temp dir seeded with `(dir_name, raw SKILL.md contents)`
/// entries, so a test can exercise frontmatter the simple `manager_with` helper
/// cannot express (`disable-model-invocation`, `paths:`). Returns the manager
/// alongside the temp dir (kept alive as the project root for activation).
fn manager_from_raw(skills: &[(&str, &str)]) -> (Arc<SkillManager>, TempDir) {
    let tmp = TempDir::new().unwrap();
    for (dir, content) in skills {
        let path = tmp.path().join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), content).unwrap();
    }
    let mgr = SkillManager::discover(tmp.path(), None);
    (Arc::new(mgr), tmp)
}

#[test]
fn spec_embeds_the_available_skills_catalog() {
    let (mgr, _tmp) = manager_with(&[("pdf", "Work with PDFs", "body")]);
    let tool = SkillTool::new(mgr);
    let spec = tool.spec();
    assert_eq!(spec.name, "skill");
    assert!(spec.description.contains("<available_skills>"));
    assert!(spec.description.contains("<name>\npdf\n</name>"));
    assert!(spec.description.contains("Work with PDFs"));
    // The scaffold is present verbatim.
    assert!(
        spec.description
            .starts_with("Execute a skill within the main conversation")
    );
    assert!(spec.description.contains("<skills_instructions>"));
}

#[test]
fn spec_escapes_xml_in_name_and_description() {
    // A parse-valid name can still contain characters that are XML-special
    // only via the description; use a description with `<`/`&` to prove the
    // escape.
    let (mgr, _tmp) = manager_with(&[("pdf", "A < B & C > D", "body")]);
    let tool = SkillTool::new(mgr);
    let spec = tool.spec();
    assert!(spec.description.contains("A &lt; B &amp; C &gt; D"));
    assert!(!spec.description.contains("A < B & C > D"));
}

#[test]
fn spec_shows_the_no_skills_text_when_the_catalog_is_empty() {
    let tool = SkillTool::new(Arc::new(SkillManager::default()));
    let spec = tool.spec();
    assert!(spec.description.contains(NO_SKILLS_TEXT));
    assert!(
        spec.description
            .contains(".suspenders/skills/ or ~/.config/suspenders/skills/")
    );
}

#[test]
fn always_load_is_true() {
    let tool = SkillTool::new(Arc::new(SkillManager::default()));
    assert!(tool.always_load());
    assert!(!tool.should_defer());
}

#[tokio::test]
async fn run_returns_the_body_wrapper_with_the_correct_base_dir() {
    let (mgr, tmp) = manager_with(&[("pdf", "Work with PDFs", "Do the PDF thing.")]);
    let base_dir = tmp.path().join("pdf");
    let tool = SkillTool::new(mgr);
    let out = tool.run(&json!({"skill": "pdf"}), &ctx()).await.unwrap();
    let expected = format!(
        "Base directory for this skill: {}\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\nDo the PDF thing.\n",
        base_dir.display()
    );
    assert_eq!(out, expected);
}

#[tokio::test]
async fn run_not_found_is_the_verbatim_message_with_available_names() {
    let (mgr, _tmp) = manager_with(&[("pdf", "PDFs", "b"), ("xlsx", "sheets", "b")]);
    let tool = SkillTool::new(mgr);
    let err = tool.run(&json!({"skill": "missing"}), &ctx()).await;
    // The available-names list is the MODEL-FACING catalog only (qwen parity),
    // here every discovered skill (none hidden/inactive) plus the always-present
    // bundled `batch`/`stuck`, alphabetically sorted.
    assert_eq!(
        err,
        Err("Skill \"missing\" not found. Available skills: batch, pdf, stuck, xlsx".to_string())
    );
}

#[tokio::test]
async fn run_empty_skill_is_the_verbatim_validate_message() {
    let tool = SkillTool::new(Arc::new(SkillManager::default()));
    let err = tool.run(&json!({"skill": "   "}), &ctx()).await;
    assert_eq!(
        err,
        Err("Parameter \"skill\" must be a non-empty string.".to_string())
    );
}

// ---- [M2] the (level) suffix on catalog descriptions ----

#[tokio::test]
async fn spec_description_carries_the_level_suffix() {
    // qwen tools/skill.ts:174 appends ` (${skill.level})` to each catalog entry's
    // description. A disk skill under the project root is `(project)`.
    let (mgr, _tmp) = manager_with(&[("pdf", "Work with PDFs", "b")]);
    let tool = SkillTool::new(mgr);
    let spec = tool.spec();
    assert!(spec.description.contains("Work with PDFs (project)"));
    // The when_to_use hint sits BEFORE the level suffix, as qwen orders them.
    let (mgr2, _tmp2) = manager_from_raw(&[(
        "excel",
        "---\nname: excel\ndescription: Sheets\nwhen_to_use: For spreadsheets\n---\nb",
    )]);
    let spec2 = SkillTool::new(mgr2).spec();
    assert!(
        spec2
            .description
            .contains("Sheets - For spreadsheets (project)")
    );
    // The bundled skills carry `(bundled)`.
    assert!(spec.description.contains("(bundled)"));
}

// ---- [H3] the model can only invoke a model-invocable, active skill ----

#[tokio::test]
async fn run_cannot_invoke_a_disable_model_invocation_skill() {
    // A `disable-model-invocation` skill is not in the model-facing catalog, so
    // the model cannot invoke it, and its name must NOT leak into the not-found
    // "Available skills:" list (which is the catalog only).
    let (mgr, _tmp) = manager_from_raw(&[
        (
            "secret",
            "---\nname: secret\ndescription: hidden\ndisable-model-invocation: true\n---\nsecret body",
        ),
        ("pdf", "---\nname: pdf\ndescription: PDFs\n---\nb"),
    ]);
    let tool = SkillTool::new(mgr);

    let err = tool.run(&json!({"skill": "secret"}), &ctx()).await;
    let msg = err.unwrap_err();
    // Generic not-found (it is not pending-conditional), and the hidden name is
    // absent from the available list.
    assert!(msg.starts_with("Skill \"secret\" not found. Available skills:"));
    assert!(!msg.contains("secret,") && !msg.ends_with("secret"));
    // The visible ones are still listed.
    assert!(msg.contains("pdf"));
}

#[tokio::test]
async fn run_cannot_invoke_a_not_yet_activated_conditional_skill() {
    // A conditional (`paths:`) skill is hidden until a matching file is touched.
    // Before activation the model gets qwen's DISTINCT path-gated message, not the
    // generic not-found.
    let (mgr, _tmp) = manager_from_raw(&[(
        "rusty",
        "---\nname: rusty\ndescription: Rust helper\npaths:\n  - src/**\n---\nrust body",
    )]);
    let tool = SkillTool::new(mgr);

    let err = tool.run(&json!({"skill": "rusty"}), &ctx()).await;
    assert_eq!(
        err,
        Err(
            "Skill \"rusty\" is gated by path-based activation (paths: frontmatter) and is not yet available. Access a file matching its paths patterns first to activate it."
                .to_string()
        )
    );
}

#[tokio::test]
async fn run_can_invoke_a_conditional_skill_after_activation() {
    // Once a matching file activates the conditional skill, the model CAN invoke
    // it and gets its body wrapper.
    let (mgr, tmp) = manager_from_raw(&[(
        "rusty",
        "---\nname: rusty\ndescription: Rust helper\npaths:\n  - src/**\n---\nrust body",
    )]);
    // Touch a matching path (the manager is shared through the Arc).
    let newly = mgr.activate_by_path(std::path::Path::new("src/lib.rs"), tmp.path());
    assert_eq!(newly, vec!["rusty".to_string()]);

    let base_dir = tmp.path().join("rusty");
    let tool = SkillTool::new(mgr);
    let out = tool.run(&json!({"skill": "rusty"}), &ctx()).await.unwrap();
    let expected = format!(
        "Base directory for this skill: {}\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\nrust body\n",
        base_dir.display()
    );
    assert_eq!(out, expected);
}

#[tokio::test]
async fn run_not_found_list_omits_hidden_and_inactive_names() {
    // The not-found "Available skills:" list is the model-facing catalog: a hidden
    // (`disable-model-invocation`) skill and a not-yet-activated conditional skill
    // are both absent.
    let (mgr, _tmp) = manager_from_raw(&[
        (
            "hidden",
            "---\nname: hidden\ndescription: d\ndisable-model-invocation: true\n---\nb",
        ),
        (
            "gated",
            "---\nname: gated\ndescription: d\npaths:\n  - src/**\n---\nb",
        ),
        ("pdf", "---\nname: pdf\ndescription: d\n---\nb"),
    ]);
    let tool = SkillTool::new(mgr);

    let msg = tool
        .run(&json!({"skill": "nope"}), &ctx())
        .await
        .unwrap_err();
    assert!(msg.starts_with("Skill \"nope\" not found. Available skills:"));
    assert!(!msg.contains("hidden"));
    assert!(!msg.contains("gated"));
    assert!(msg.contains("pdf"));
}

#[test]
fn spec_location_is_the_skill_base_dir() {
    let (mgr, tmp) = manager_with(&[("pdf", "PDFs", "b")]);
    let base = tmp.path().join("pdf");
    let tool = SkillTool::new(mgr);
    let spec = tool.spec();
    assert!(
        spec.description
            .contains(&format!("<location>\n{}\n</location>", base.display()))
    );
}
