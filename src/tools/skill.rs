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
#[path = "../../tests/tools/skill.rs"]
mod tests;
