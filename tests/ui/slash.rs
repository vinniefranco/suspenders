use super::*;

// --- is_slash ----------------------------------------------------------

#[test]
fn is_slash_true_only_when_the_trimmed_draft_starts_with_a_slash() {
    assert!(is_slash("/"));
    assert!(is_slash("/model"));
    assert!(is_slash("   /model"), "leading whitespace is ignored");
    assert!(!is_slash(""));
    assert!(!is_slash("model"));
    assert!(
        !is_slash("fix the /bug"),
        "a slash mid-text is not a command"
    );
}

// --- parse -------------------------------------------------------------

#[test]
fn parse_lone_slash_is_an_empty_name_no_rest() {
    assert_eq!(
        parse("/"),
        SlashDraft {
            name: "".into(),
            rest: None,
        }
    );
}

#[test]
fn parse_a_bare_command_has_no_rest() {
    assert_eq!(
        parse("/model"),
        SlashDraft {
            name: "model".into(),
            rest: None,
        }
    );
    // A partial token still parses (it filters the menu).
    assert_eq!(
        parse("/mod"),
        SlashDraft {
            name: "mod".into(),
            rest: None,
        }
    );
}

#[test]
fn parse_a_trailing_space_yields_an_empty_rest() {
    assert_eq!(
        parse("/model "),
        SlashDraft {
            name: "model".into(),
            rest: Some("".into()),
        }
    );
}

#[test]
fn parse_splits_the_remainder_at_the_first_space() {
    assert_eq!(
        parse("/model qw"),
        SlashDraft {
            name: "model".into(),
            rest: Some("qw".into()),
        }
    );
    // Only the first space is the boundary; later spaces live in rest.
    assert_eq!(
        parse("/model qwen 2"),
        SlashDraft {
            name: "model".into(),
            rest: Some("qwen 2".into()),
        }
    );
}

#[test]
fn parse_ignores_whitespace_before_the_slash() {
    assert_eq!(
        parse("  /model qw"),
        SlashDraft {
            name: "model".into(),
            rest: Some("qw".into()),
        }
    );
}

// --- lookup ------------------------------------------------------------

#[test]
fn lookup_matches_a_registered_name_exactly() {
    assert_eq!(lookup("model"), Some(&COMMANDS[0]));
    assert_eq!(lookup("theme"), Some(&COMMANDS[1]));
    assert_eq!(lookup("mod"), None, "partial names never resolve");
    assert_eq!(lookup("compact"), None);
    assert_eq!(lookup(""), None);
}

#[test]
fn every_selector_command_states_its_list_title() {
    // The popup titles itself from the descriptor, so a selector-opening
    // command without a plural noun would paint an empty title.
    for c in COMMANDS.iter().filter(|c| c.opens_selector) {
        assert!(!c.list_title.is_empty(), "/{} has no list_title", c.name);
    }
    assert_eq!(lookup("model").unwrap().list_title, "models");
    assert_eq!(lookup("theme").unwrap().list_title, "themes");
}

#[test]
fn model_and_theme_open_a_selector_sub_state() {
    assert!(
        lookup("model").expect("model is registered").opens_selector,
        "committing /model switches the popup to its own value list"
    );
    assert!(
        lookup("theme").expect("theme is registered").opens_selector,
        "committing /theme switches the popup to the theme list (ADR-0038)"
    );
}

// --- alt_names ---------------------------------------------------------

#[test]
fn every_command_declares_its_alt_names_slice() {
    // The palette ranking (ADR-0051) reads name + alt_names; a missing
    // slice would be a compile error, so this just pins the current empty
    // aliases so a future alias addition is a deliberate change.
    for c in COMMANDS {
        assert!(c.alt_names.is_empty(), "/{} has no aliases yet", c.name);
    }
}

// --- the two-layer registry (ADR-0032/0058) ----------------------------

fn skill_cmd(name: &str, help: &str, hint: Option<&str>) -> SkillCommand {
    SkillCommand {
        name: name.to_string(),
        help: help.to_string(),
        argument_hint: hint.map(str::to_string),
    }
}

#[test]
fn commands_ref_unions_the_built_ins_then_the_skill_layer() {
    // The union is every built-in first (in registry order), then each runtime
    // skill command - the stable `original_index` basis the ranking leans on.
    let skills = [skill_cmd("commit", "write a commit", None)];
    let refs = commands_ref(&skills);
    assert_eq!(refs.len(), COMMANDS.len() + 1);
    // Built-ins lead.
    assert_eq!(refs[0].name, COMMANDS[0].name);
    // The skill trails, projected fire-and-run with its hint slot.
    let last = refs.last().unwrap();
    assert_eq!(last.name, "commit");
    assert!(!last.opens_selector, "a skill command never opens a selector");
    assert_eq!(last.argument_hint, None);
}

#[test]
fn a_skill_ref_carries_its_argument_hint() {
    let skills = [skill_cmd("commit", "write a commit", Some("<message>"))];
    let refs = commands_ref(&skills);
    let commit = refs.iter().find(|c| c.name == "commit").unwrap();
    assert_eq!(commit.argument_hint, Some("<message>"));
    assert_eq!(commit.help, "write a commit");
}

#[test]
fn a_skill_is_never_a_built_in_lookup() {
    // The built-in-only lookup does not resolve a skill name (a skill is never
    // selector-opening), so the Composer's "opens a selector?" test correctly
    // says no for a `/<skill>` token.
    assert!(lookup("commit").is_none());
    assert!(lookup("model").is_some());
}

#[test]
fn a_built_in_wins_a_union_name_collision() {
    // A skill named after a built-in cannot shadow it: the same-named skill is
    // dropped from the union, so only the built-in `/model` remains.
    let skills = [skill_cmd("model", "a shadowing skill", None)];
    let refs = commands_ref(&skills);
    let models: Vec<_> = refs.iter().filter(|c| c.name == "model").collect();
    assert_eq!(models.len(), 1, "the skill collision is dropped");
    assert!(models[0].opens_selector, "the built-in /model wins");
}
