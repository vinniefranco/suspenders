
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
