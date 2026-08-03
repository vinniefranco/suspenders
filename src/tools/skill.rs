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
//! a `skill` name that is in the MODEL-INVOCABLE catalog (not
//! `disable-model-invocation` AND currently active) returns that skill's body
//! wrapped by [`build_skill_llm_content`](crate::skills::build_skill_llm_content),
//! so the model gets the skill's instructions plus its base directory for
//! absolute-path resolution. A hidden or not-yet-activated skill cannot be invoked
//! (qwen's `validateToolParams` gating): a pending conditional skill gets qwen's
//! distinct path-gated message, and the not-found list is the model-facing catalog
//! only, never leaking hidden/inactive names.
//!
//! The `<skills_instructions>` body is ported from qwen v0.21.4's
//! `tools/skill.ts` `SKILL_TOOL_DESCRIPTION` (the "can help" wording and the
//! `mcp-prompt`/`args` invoke example are verbatim). One line deviates from
//! qwen: qwen's "Available skills are listed in `<system-reminder>` messages"
//! is rewritten to point at the `<available_skills>` catalog Suspenders splices
//! into this description, because Suspenders does not surface skills via
//! `<system-reminder>` deltas (see the architectural divergence note below);
//! naming `<system-reminder>` here would misdirect the model to a channel that
//! carries nothing. The empty-catalog wording points at Suspenders' project
//! `.suspenders/skills/` and user `~/.config/suspenders/skills/` (the XDG config
//! home, per [`crate::session::default_user_skills_dir`]) conventions (not qwen's
//! `.qwen/skills/`). The validate-empty and not-found messages are qwen-verbatim.
//!
//! ARCHITECTURAL DIVERGENCE (deliberately retained this pass): qwen v0.21.4 made
//! `SKILL_TOOL_DESCRIPTION` STATIC - the live skill catalog reaches the model via
//! `<available_skills>` startup-prelude snapshots and per-turn `<system-reminder>`
//! deltas, NOT the tool description, so skill changes never mutate the
//! prompt-cache-prefixing tools block. Suspenders still splices a live
//! `<available_skills>` XML block into this description (the surfacing mechanism,
//! ADR-0058). Migrating to qwen's static-description + system-reminder delta model
//! is a separate architectural change and is out of scope here.

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

    /// The `<available_skills>` body: one `<skill>` entry per catalog skill (a
    /// conditional skill only after activation, and never a
    /// `disable-model-invocation` skill), name + description XML-escaped, the
    /// `when_to_use` appended to the description as qwen does, or the empty-catalog
    /// text. The `<location>` is the skill's base directory so the model can
    /// resolve absolute paths before even invoking. Built fresh for every `spec()`
    /// call off the manager's current catalog.
    fn skill_descriptions(&self) -> String {
        // The model-facing CATALOG, not every discovered skill (ADR-0058): a
        // conditional (`paths:`) skill is excluded until a touched file activates
        // it, and a `disable-model-invocation` skill is dropped entirely. Built
        // fresh per `spec()` call, so a newly-activated skill appears next turn.
        let skills = self.manager.catalog();
        if skills.is_empty() {
            return NO_SKILLS_TEXT.to_string();
        }
        skills
            .iter()
            .map(|skill| {
                // qwen (tools/skill.ts:174): `${description}${whenToUse ? ` - ${whenToUse}`
                // : ''} (${skill.level})` (qwen's em-dash rendered as a hyphen, per the
                // house hyphens-everywhere rule). The trailing `(level)` names the source
                // (project/user/bundled) so the model can tell them apart.
                let level = skill.level.label();
                let desc = match &skill.when_to_use {
                    Some(w) => format!(
                        "{} - {} ({level})",
                        escape_xml(&skill.description),
                        escape_xml(w)
                    ),
                    None => format!("{} ({level})", escape_xml(&skill.description)),
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
    // Read (qwen skill.ts:131 `Kind.Read`): ALLOWED in plan mode. qwen kinds the
    // `skill` tool as Read (it surfaces skill guidance), so it is read-only.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Read
    }

    fn spec(&self) -> ToolSpec {
        // The description scaffold, VERBATIM from qwen's `tools/skill.ts`
        // `baseDescription`, with the live `<available_skills>` block spliced in.
        let description = format!(
            "Execute a skill within the main conversation

<skills_instructions>
When users ask you to perform tasks, check if any of the available skills can help complete the task more effectively. Skills provide specialized capabilities and domain knowledge.

How to invoke:
- Use this tool with the skill name only (no arguments)
- Examples:
  - `skill: \"pdf\"` - invoke the pdf skill
  - `skill: \"xlsx\"` - invoke the xlsx skill
  - `skill: \"ms-office-suite:pdf\"` - invoke using fully qualified name
  - `skill: \"mcp-prompt\", args: \"topic\"` - invoke a model-invocable command with arguments

Important:
- Available skills are listed in the <available_skills> section below; only use skills listed there.
- When a skill is relevant, you must invoke this tool IMMEDIATELY as your first action
- NEVER just announce or mention a skill in your text response without actually calling this tool
- This is a BLOCKING REQUIREMENT: invoke the relevant Skill tool BEFORE generating any other response about the task
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
            // qwen's initial schema (tools/skill.ts:110-125): a required `skill`
            // string plus an optional `args` string (for model-invocable slash
            // commands), no additional properties.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "The skill or command name. E.g., \"pdf\" or \"xlsx\"",
                    },
                    "args": {
                        "type": "string",
                        "description": "Optional arguments for model-invocable slash commands.",
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

        // Resolve against the MODEL-INVOCABLE set (the same filtered catalog the
        // <available_skills> block surfaces): not `disable-model-invocation` AND
        // currently active. A hidden or not-yet-activated skill is never invocable
        // through this tool (qwen's `validateToolParams`, tools/skill.ts:251-290).
        if let Some(skill) = self.manager.find_invocable(name) {
            return Ok(build_skill_llm_content(&skill.base_dir, &skill.body));
        }

        // A known-but-pending conditional skill gets qwen's DISTINCT path-gated
        // message (tools/skill.ts:278-280), not the generic not-found, so the model
        // learns it exists but must touch a matching file to unlock it.
        if self.manager.is_pending_conditional(name) {
            return Err(format!(
                "Skill \"{name}\" is gated by path-based activation (paths: frontmatter) and is not yet available. Access a file matching its paths patterns first to activate it."
            ));
        }

        // qwen's not-found message. The available-names list is the MODEL-FACING
        // catalog only (tools/skill.ts:282-289), so a hidden/inactive skill name
        // is never leaked to the model.
        let names: Vec<&str> = self
            .manager
            .catalog()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        if names.is_empty() {
            return Err(format!(
                "Skill \"{name}\" not found. No skills are currently available."
            ));
        }
        Err(format!(
            "Skill \"{name}\" not found. Available skills: {}",
            names.join(", ")
        ))
    }
}

#[cfg(test)]
#[path = "../../tests/tools/skill.rs"]
mod tests;
