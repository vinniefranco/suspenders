//! `skill` - the ONE tool that surfaces + invokes disk-discovered skills
//! (P2c, ADR-0058).
//!
//! Not one tool per skill: a single [`SkillTool`] holds an `Arc` of the
//! [`SkillManager`](crate::skills::SkillManager) and builds its description
//! dynamically, embedding an `<available_skills>` catalog with one
//! `<skill><name/><description/><location/></skill>` entry per discovered skill
//! (names + descriptions XML-escaped). That catalog IS the surfacing mechanism -
//! the model reads the skill list off the tool's own description - so the tool is
//! always on the wire list (`always_load` true, never deferred). Invoking it with
//! a `skill` name returns that skill's body wrapped by
//! [`build_skill_llm_content`](crate::skills::build_skill_llm_content), so the
//! model gets the skill's instructions plus its base directory for absolute-path
//! resolution.
//!
//! The description scaffold (`Execute a skill within the main conversation` +
//! `<skills_instructions>` + `<available_skills>`) is ported VERBATIM from qwen
//! v0.16.0's `tools/skill.ts`; the empty-catalog wording points at Suspenders'
//! project `.suspenders/skills/` and user `~/.config/suspenders/skills/` (the XDG
//! config home, per [`crate::session::default_user_skills_dir`]) conventions (not
//! qwen's `.qwen/skills/`). The validate-empty and not-found messages are
//! qwen-verbatim.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::skills::{SkillManager, build_skill_llm_content, escape_xml};
use crate::tool::{Tool, ToolCtx, ToolSpec};

/// The empty-catalog description text (qwen's "no skills configured" wording,
/// adapted to Suspenders' conventions: the project-level `.suspenders/skills/`
/// and the user-level `~/.config/suspenders/skills/` under the XDG config home,
/// per [`crate::session::default_user_skills_dir`]).
const NO_SKILLS_TEXT: &str = "No skills are currently configured. Skills can be created by adding directories with SKILL.md files to .suspenders/skills/ or ~/.config/suspenders/skills/.";

/// The `skill` tool: a stateful tool (like [`crate::mcp::adapter::McpTool`])
/// holding the Session's discovered-skill catalog. Registered once in
/// `init_agent` over the `Arc<SkillManager>` discovery produced.
pub struct SkillTool {
    manager: Arc<SkillManager>,
}

impl SkillTool {
    /// Builds the tool over a shared [`SkillManager`]. The Agent discovers skills
    /// once at launch and hands the `Arc` here.
    pub fn new(manager: Arc<SkillManager>) -> Self {
        SkillTool { manager }
    }

    /// The `<available_skills>` body: one `<skill>` entry per discovered skill
    /// (name + description XML-escaped, the `when_to_use` appended to the
    /// description as qwen does), or the empty-catalog text. The `<location>` is
    /// the skill's base directory so the model can resolve absolute paths before
    /// even invoking. Built fresh for every `spec()` call off the manager's
    /// current catalog.
    fn skill_descriptions(&self) -> String {
        let skills = self.manager.available();
        if skills.is_empty() {
            return NO_SKILLS_TEXT.to_string();
        }
        skills
            .iter()
            .map(|skill| {
                // qwen: `${description}${whenToUse ? ` - ${whenToUse}` : ''}` (qwen's
                // em-dash rendered as a hyphen, per the house hyphens-everywhere rule).
                let desc = match &skill.when_to_use {
                    Some(w) => format!("{} - {}", escape_xml(&skill.description), escape_xml(w)),
                    None => escape_xml(&skill.description),
                };
                format!(
                    "<skill>\n<name>\n{}\n</name>\n<description>\n{}\n</description>\n<location>\n{}\n</location>\n</skill>",
                    escape_xml(&skill.name),
                    desc,
                    skill.base_dir.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        // The description scaffold, VERBATIM from qwen's `tools/skill.ts`
        // `baseDescription`, with the live `<available_skills>` block spliced in.
        let description = format!(
            "Execute a skill within the main conversation

<skills_instructions>
When users ask you to perform tasks, check if any of the available skills below can help complete the task more effectively. Skills provide specialized capabilities and domain knowledge.

How to invoke:
- Use this tool with the skill name only (no arguments)
- Examples:
  - `skill: \"pdf\"` - invoke the pdf skill
  - `skill: \"xlsx\"` - invoke the xlsx skill
  - `skill: \"ms-office-suite:pdf\"` - invoke using fully qualified name

Important:
- When a skill is relevant, you must invoke this tool IMMEDIATELY as your first action
- NEVER just announce or mention a skill in your text response without actually calling this tool
- This is a BLOCKING REQUIREMENT: invoke the relevant Skill tool BEFORE generating any other response about the task
- Only use skills listed in <available_skills> below
- Do not invoke a skill that is already running
- Do not use this tool for built-in CLI commands (like /help, /clear, etc.)
- When executing scripts or loading referenced files, ALWAYS resolve absolute paths from skill's base directory. Examples:
  - `bash scripts/init.sh` -> `bash /path/to/skill/scripts/init.sh`
  - `python scripts/helper.py` -> `python /path/to/skill/scripts/helper.py`
  - `reference.md` -> `/path/to/skill/reference.md`
</skills_instructions>

<available_skills>
{}
</available_skills>",
            self.skill_descriptions()
        );

        ToolSpec {
            name: "skill".to_string(),
            description,
            // qwen's initial schema: a single required `skill` string, no
            // additional properties.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "The skill name (no arguments). E.g., \"pdf\" or \"xlsx\"",
                    },
                },
                "required": ["skill"],
                "additionalProperties": false,
            }),
        }
    }

    /// Always on the wire list: the tool's own description carries the
    /// `<available_skills>` catalog, so the model can only learn a skill exists by
    /// seeing this tool. Hiding it (deferral) would hide the catalog (ADR-0058).
    fn always_load(&self) -> bool {
        true
    }

    async fn run(&self, input: &Value, _ctx: &ToolCtx) -> Result<String, String> {
        // Validate the `skill` param the way qwen's `validateToolParams` does: a
        // non-empty string. (Suspenders' schema validation guarantees the field
        // is present + string-typed; the non-empty check is skill-specific.)
        let name = input
            .get("skill")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Parameter \"skill\" must be a non-empty string.".to_string())?;

        match self.manager.find(name) {
            Some(skill) => Ok(build_skill_llm_content(&skill.base_dir, &skill.body)),
            None => {
                // qwen's not-found message, with the available-names list.
                let names: Vec<&str> = self
                    .manager
                    .available()
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect();
                Err(format!(
                    "Skill \"{name}\" not found. Available skills: {}",
                    names.join(", ")
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
}
