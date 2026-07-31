//! Subagent definitions and the child tool subset (P4/F4, ADR-0061).
//!
//! A LEAF: this module names only `tool`/`content`/`llm`/`std` (plus the
//! built-in tool set through [`crate::tools::tools`]). It carries no Agent/Run
//! import, so the subagent vocabulary sits below the Run in the module graph -
//! the `agent` tool holds a [`SubagentRegistry`] the same way the `skill` tool
//! holds a `SkillManager`, and the child-Run factory reads a def's fields to
//! assemble a child Run.
//!
//! ## Built-in agents (qwen parity)
//!
//! [`builtins`] ports qwen v0.16.0's `BuiltinAgentRegistry` (from
//! `subagents/builtin-agents.ts`). Suspenders ships exactly TWO of qwen's
//! three: `general-purpose` (the default) and the read-only `Explore` agent.
//! qwen's third built-in (`statusline-setup`) is Qwen-Code-specific (it edits
//! `~/.qwen/settings.json`) and has no Suspenders analog, so it is not ported.
//! Both system prompts and descriptions are copied VERBATIM from qwen; the tool
//! names qwen interpolates into the prompts (`${ToolNames.READ_FILE}` etc.)
//! resolve to Suspenders' own tool names at the interpolation points (qwen does
//! the same substitution at build time), and qwen's em-dashes are rendered as
//! hyphens per the house style.
//!
//! ## The child tool subset
//!
//! [`subagent_tools`] builds a child Run's tool set from the built-ins,
//! narrowed by the def's [`ToolSelector`], then dropped of every tool in
//! [`EXCLUDED_TOOLS_FOR_SUBAGENTS`] (qwen's exclusion list, intersected with the
//! tools that exist here). MCP tools for subagents are DEFERRED (ADR-0061):
//! built-ins only, so the child set is sourced from [`crate::tools::tools`], not
//! the Session-stable set.

use crate::tool::Tool;

/// Which Model a subagent runs on (qwen's `SubagentConfig.model`, narrowed).
/// `Inherit` runs the subagent on the parent Run's captured Model; `Scoped`
/// pins a specific `provider/model-id` the spawner resolves against the
/// Session's Provider set. qwen's `'fast'` alias (a provider-level fast-model
/// hint) is DEFERRED (ADR-0061) - the `Explore` agent, which qwen marks `'fast'`,
/// ships as `Inherit` for now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentModel {
    /// Run on the parent Run's captured Model.
    Inherit,
    /// Run on this scoped `provider/model-id`, resolved against the Session's
    /// Provider set at spawn.
    Scoped(String),
}

/// Which tools a subagent may call (qwen's `SubagentConfig.tools`). `All`
/// exposes every built-in (still minus [`EXCLUDED_TOOLS_FOR_SUBAGENTS`]);
/// `Allow` narrows to a named allowlist (again minus the exclusions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSelector {
    /// Every built-in tool (minus the exclusions).
    All,
    /// Only the named tools (minus the exclusions).
    Allow(Vec<String>),
}

/// One subagent definition (qwen's `SubagentConfig`, narrowed to what the
/// foreground child-Run factory reads): the `name` the `agent` tool routes by,
/// the `description` it embeds in its own tool description, the verbatim
/// `system_prompt` the child Run opens on, the `model` choice, and the `tools`
/// the child may call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentDef {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub model: SubagentModel,
    pub tools: ToolSelector,
}

/// The set of subagent definitions available to the `agent` tool (qwen's
/// `SubagentManager`, narrowed to the built-in set - user/project subagent
/// files are DEFERRED, ADR-0061). Held by the `agent` tool as an `Arc`, the way
/// the `skill` tool holds a `SkillManager`.
#[derive(Debug, Clone, Default)]
pub struct SubagentRegistry {
    defs: Vec<SubagentDef>,
}

impl SubagentRegistry {
    /// Builds a registry over the given definitions.
    pub fn new(defs: Vec<SubagentDef>) -> Self {
        SubagentRegistry { defs }
    }

    /// The def with the given name (case-insensitive, matching qwen's
    /// `getBuiltinAgent`), or `None`.
    pub fn get(&self, name: &str) -> Option<&SubagentDef> {
        let lower = name.to_lowercase();
        self.defs.iter().find(|d| d.name.to_lowercase() == lower)
    }

    /// Every def, in registration order.
    pub fn list(&self) -> &[SubagentDef] {
        &self.defs
    }

    /// The names of every def, in registration order (for the `agent` tool's
    /// `subagent_type` enum and its not-found message).
    pub fn names(&self) -> Vec<String> {
        self.defs.iter().map(|d| d.name.clone()).collect()
    }
}

/// Tools no subagent may ever call (qwen's `EXCLUDED_TOOLS_FOR_SUBAGENTS` from
/// `agents/runtime/agent-core.ts`), intersected with the tools that exist in
/// Suspenders. qwen's full list is `agent`, the cron tools, `task_stop`,
/// `send_message`, and the worktree-enter/exit tools; of those, only `agent`,
/// `task_stop`, and `ask_user_question` are relevant here (`ask_user_question`
/// is dropped because a foreground subagent has no modal channel to answer a
/// question through - its `Questioner` is a `DecliningQuestioner` and the tool
/// would only ever return the decline string). `agent` is the recursion guard's
/// belt-and-braces companion (the child's `subagents` capability is already
/// degraded); `task_stop` is a deferred tool that does not exist here yet but is
/// listed so a later phase that adds it inherits the exclusion.
pub const EXCLUDED_TOOLS_FOR_SUBAGENTS: &[&str] = &["agent", "task_stop", "ask_user_question"];

/// The built-in subagent definitions (qwen's `BuiltinAgentRegistry`, ported
/// verbatim - see the module docs). Exactly two: `general-purpose` and the
/// read-only `Explore`.
pub fn builtins() -> Vec<SubagentDef> {
    vec![general_purpose(), explore()]
}

/// The canonical name of the default built-in subagent (qwen's
/// `DEFAULT_BUILTIN_SUBAGENT_TYPE`). The `agent` tool defaults `subagent_type`
/// to this when the model omits it.
pub const DEFAULT_BUILTIN_SUBAGENT_TYPE: &str = "general-purpose";

// The `general-purpose` def (qwen `builtin-agents.ts`): default agent, Inherit
// model, All tools. Description + system prompt VERBATIM from qwen; the one
// interpolation (`${ToolNames.READ_FILE}`) resolves to `read_file`, Suspenders'
// own name.
fn general_purpose() -> SubagentDef {
    SubagentDef {
        name: DEFAULT_BUILTIN_SUBAGENT_TYPE.to_string(),
        description: "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. When you are searching for a keyword or file and are not confident that you will find the right match in the first few tries use this agent to perform the search for you.".to_string(),
        system_prompt: "You are a general-purpose agent. Given the user's message, you should use the tools available to complete the task. Do what has been asked; nothing more, nothing less. When you complete the task, respond with a concise report covering what was done and any key findings - the caller will relay this to the user, so it only needs the essentials.

Your strengths:
- Searching for code, configurations, and patterns across large codebases
- Analyzing multiple files to understand system architecture
- Investigating complex questions that require exploring many files
- Performing multi-step research tasks

Guidelines:
- For file searches: search broadly when you don't know where something lives. Use read_file when you know the specific file path.
- For analysis: Start broad and narrow down. Use multiple search strategies if the first doesn't yield results.
- Be thorough: Check multiple locations, consider different naming conventions, look for related files.
- NEVER create files unless they're absolutely necessary for achieving your goal. ALWAYS prefer editing an existing file to creating a new one.
- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested.
- In your final response, share file paths (always absolute, never relative) that are relevant to the task. Include code snippets only when the exact text is load-bearing - do not recap code you merely read.
- For clear communication, avoid using emojis.

Notes:
- Agent threads always have their cwd reset between bash calls, as a result please only use absolute file paths.
- In your final response, share file paths (always absolute, never relative) that are relevant to the task. Include code snippets only when the exact text is load-bearing (e.g., a bug you found, a function signature the caller asked for) - do not recap code you merely read.
- For clear communication with the user the assistant MUST avoid using emojis.".to_string(),
        model: SubagentModel::Inherit,
        tools: ToolSelector::All,
    }
}

// The `Explore` def (qwen `builtin-agents.ts`): read-only explorer. qwen marks
// it `model: 'fast'` and lists a broad tool set (READ_FILE, GREP, GLOB, SHELL,
// LS, WEB_FETCH, TODO_WRITE, MEMORY, SKILL, LSP, ASK_USER_QUESTION); per ADR-0061
// the `'fast'` alias is deferred (ships Inherit) and the allowlist is qwen's set
// intersected with the tools that exist here, then minus the exclusions - so it
// is read_file/grep_search/glob/run_shell_command/list_directory/web_fetch/todo_write
// (qwen's MEMORY/SKILL/LSP have no Suspenders analog, and ASK_USER_QUESTION is an
// EXCLUDED tool). This grant backs the verbatim prompt, which instructs the
// model to use run_shell_command (read-only git/ls/cat) and to fetch the web.
// Description + system prompt VERBATIM from qwen; the `${ToolDisplayNames.*}`
// interpolations resolve to Suspenders' own tool names (write_file/edit/
// glob/grep_search/read_file/run_shell_command).
fn explore() -> SubagentDef {
    SubagentDef {
        name: "Explore".to_string(),
        description: "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".to_string(),
        system_prompt: "You are a file search specialist agent. You excel at thoroughly navigating and exploring codebases.

=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===
This is a READ-ONLY exploration task. You are STRICTLY PROHIBITED from:
- Creating new files (no write_file, touch, or file creation of any kind)
- Modifying existing files (no edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Your role is EXCLUSIVELY to search and analyze existing code. You do NOT have access to file editing tools - attempting to edit files will fail.

Your strengths:
- Rapidly finding files using glob patterns
- Searching code and text with powerful regex patterns
- Reading and analyzing file contents

Guidelines:
- Use glob for broad file pattern matching
- Use grep_search for searching file contents with regex
- Use read_file when you know the specific file path you need to read
- Use run_shell_command ONLY for read-only operations (ls, git status, git log, git diff, find, cat, head, tail)
- NEVER use run_shell_command for: mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, or any file creation/modification
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- For clear communication, avoid using emojis
- Communicate your final report directly as a regular message - do NOT attempt to create files

NOTE: You are meant to be a fast agent that returns output as quickly as possible. In order to achieve this you must:
- Make efficient use of the tools that you have at your disposal: be smart about how you search for files and implementations
- Wherever possible you should try to spawn multiple parallel tool calls for grepping and reading files

Complete the user's search request efficiently and report your findings clearly.

Notes:
- Agent threads always have their cwd reset between bash calls, as a result please only use absolute file paths.
- In your final response, share file paths (always absolute, never relative) that are relevant to the task. Include code snippets only when the exact text is load-bearing (e.g., a bug you found, a function signature the caller asked for) - do not recap code you merely read.
- For clear communication with the user the assistant MUST avoid using emojis.".to_string(),
        model: SubagentModel::Inherit,
        tools: ToolSelector::Allow(vec![
            "read_file".to_string(),
            "grep_search".to_string(),
            "glob".to_string(),
            "run_shell_command".to_string(),
            "list_directory".to_string(),
            "web_fetch".to_string(),
            "todo_write".to_string(),
        ]),
    }
}

/// Builds a child Run's tool subset from a def's [`ToolSelector`] (P4/F4,
/// ADR-0061). Sourced from the built-in set ([`crate::tools::tools`]) - MCP
/// tools for subagents are DEFERRED. An `Allow` selector keeps only the named
/// tools; then every tool in [`EXCLUDED_TOOLS_FOR_SUBAGENTS`] is dropped
/// regardless of the selector, so a def can never re-admit an excluded tool.
pub fn subagent_tools(selector: &ToolSelector) -> Vec<Box<dyn Tool>> {
    crate::tools::tools()
        .into_iter()
        .filter(|tool| {
            let name = tool.spec().name;
            let allowed = match selector {
                ToolSelector::All => true,
                ToolSelector::Allow(names) => names.iter().any(|n| n == &name),
            };
            allowed && !EXCLUDED_TOOLS_FOR_SUBAGENTS.contains(&name.as_str())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(tools: &[Box<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|t| t.spec().name).collect()
    }

    #[test]
    fn builtins_ship_exactly_general_purpose_and_explore() {
        let defs = builtins();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "general-purpose");
        assert_eq!(defs[1].name, "Explore");
    }

    #[test]
    fn registry_get_is_case_insensitive_and_list_names_agree() {
        let reg = SubagentRegistry::new(builtins());
        assert!(reg.get("general-purpose").is_some());
        assert!(reg.get("GENERAL-PURPOSE").is_some());
        assert!(reg.get("Explore").is_some());
        assert!(reg.get("explore").is_some());
        assert!(reg.get("nope").is_none());
        assert_eq!(reg.list().len(), 2);
        assert_eq!(reg.names(), vec!["general-purpose", "Explore"]);
    }

    #[test]
    fn general_purpose_is_inherit_all_with_the_verbatim_prompt() {
        let def = general_purpose();
        assert_eq!(def.model, SubagentModel::Inherit);
        assert_eq!(def.tools, ToolSelector::All);
        // A load-bearing verbatim fragment of qwen's prompt (em-dash -> hyphen).
        assert!(
            def.system_prompt
                .starts_with("You are a general-purpose agent.")
        );
        assert!(
            def.system_prompt
                .contains("Do what has been asked; nothing more, nothing less.")
        );
    }

    #[test]
    fn explore_is_read_only_and_inherit() {
        let def = explore();
        assert_eq!(def.model, SubagentModel::Inherit);
        // qwen's Explore set (SHELL + WEB_FETCH included), intersected with the
        // tools that exist here - the grant that backs its verbatim prompt.
        assert_eq!(
            def.tools,
            ToolSelector::Allow(vec![
                "read_file".into(),
                "grep_search".into(),
                "glob".into(),
                "run_shell_command".into(),
                "list_directory".into(),
                "web_fetch".into(),
                "todo_write".into(),
            ])
        );
        assert!(
            def.system_prompt
                .starts_with("You are a file search specialist agent.")
        );
        assert!(def.system_prompt.contains("READ-ONLY MODE"));
        // The prompt instructs run_shell_command / web fetches, so the grant must
        // back them.
        assert!(
            def.system_prompt
                .contains("Use run_shell_command ONLY for read-only")
        );
    }

    #[test]
    fn all_selector_drops_the_excluded_tools() {
        let tools = subagent_tools(&ToolSelector::All);
        let names = names_of(&tools);
        // The exclusions are gone.
        assert!(!names.contains(&"agent".to_string()));
        assert!(!names.contains(&"ask_user_question".to_string()));
        assert!(!names.contains(&"task_stop".to_string()));
        // But the ordinary built-ins remain.
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"run_shell_command".to_string()));
        assert!(names.contains(&"tool_search".to_string()));
    }

    #[test]
    fn allow_selector_yields_only_the_allowed_set_minus_exclusions() {
        // Ask for the read-only subset PLUS an excluded tool - the excluded one
        // must still be dropped.
        let selector = ToolSelector::Allow(vec![
            "read_file".into(),
            "grep_search".into(),
            "ask_user_question".into(),
        ]);
        let names = names_of(&subagent_tools(&selector));
        assert_eq!(
            names,
            vec!["read_file".to_string(), "grep_search".to_string()],
            "only the allowed, non-excluded tools survive"
        );
    }

    #[test]
    fn explore_selector_yields_its_read_only_tools_including_shell_and_web_fetch() {
        let def = explore();
        let names = names_of(&subagent_tools(&def.tools));
        // The survivors are Explore's allowlist (minus exclusions), in the
        // registry order of `crate::tools::tools`. run_command + web_fetch ride
        // so the prompt's read-only-shell and web-fetch instructions are backed.
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"grep_search".to_string()));
        assert!(names.contains(&"glob".to_string()));
        assert!(names.contains(&"run_shell_command".to_string()));
        assert!(names.contains(&"list_directory".to_string()));
        assert!(names.contains(&"web_fetch".to_string()));
        assert!(names.contains(&"todo_write".to_string()));
        assert_eq!(names.len(), 7, "exactly the seven allowed tools, no more");
    }
}
