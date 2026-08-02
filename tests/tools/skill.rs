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
    assert_eq!(
        err,
        Err("Skill \"missing\" not found. Available skills: pdf, xlsx".to_string())
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
