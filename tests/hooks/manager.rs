//! Unit tests for the hook manager (ADR-0066): resolution from config + skills,
//! grouping by event, and matcher filtering (regex, exact fallback, match-all,
//! inert-on-non-tool-events).

use super::*;
use crate::hooks::config::HookKind;
use serde_json::json;

/// A manager built from no config holds no hooks and no failures.
#[test]
fn empty_config_has_nothing() {
    let mgr = HookManager::from_config(None);
    assert!(mgr.failures().is_empty());
    assert!(mgr.hooks_for(HookEvent::Stop, None).is_empty());
}

/// hooks_for returns a non-tool event's hooks regardless of matcher (matcher
/// inert on non-tool events).
#[test]
fn non_tool_event_ignores_matcher() {
    let hooks = json!({
        "Stop": [ { "matcher": "irrelevant", "hooks": [ { "type": "command", "command": "x" } ] } ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    let selected = mgr.hooks_for(HookEvent::Stop, None);
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].hook.kind,
        HookKind::Command { command: "x".to_string() }
    );
    assert!(selected[0].skill_root.is_none(), "config hook has no skill root");
}

/// A tool event with an exact-name matcher fires only for the matching tool.
#[test]
fn tool_event_exact_matcher() {
    let hooks = json!({
        "PreToolUse": [
            { "matcher": "run_command", "hooks": [ { "type": "command", "command": "guard" } ] }
        ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    assert_eq!(mgr.hooks_for(HookEvent::PreToolUse, Some("run_command")).len(), 1);
    assert!(mgr.hooks_for(HookEvent::PreToolUse, Some("edit_file")).is_empty());
}

/// A regex matcher matches the tool name (qwen regex.test semantics).
#[test]
fn tool_event_regex_matcher() {
    let hooks = json!({
        "PreToolUse": [
            { "matcher": "^edit_.*", "hooks": [ { "type": "command", "command": "fmt" } ] }
        ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    assert_eq!(mgr.hooks_for(HookEvent::PreToolUse, Some("edit_file")).len(), 1);
    assert_eq!(mgr.hooks_for(HookEvent::PreToolUse, Some("edit_notebook")).len(), 1);
    assert!(mgr.hooks_for(HookEvent::PreToolUse, Some("run_command")).is_empty());
}

/// An invalid regex matcher falls back to an exact string compare (qwen).
#[test]
fn invalid_regex_matcher_falls_back_to_exact() {
    // "[" is not a valid regex; qwen treats it as a literal exact match.
    let hooks = json!({
        "PreToolUse": [
            { "matcher": "[", "hooks": [ { "type": "command", "command": "x" } ] }
        ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    assert_eq!(mgr.hooks_for(HookEvent::PreToolUse, Some("[")).len(), 1);
    assert!(mgr.hooks_for(HookEvent::PreToolUse, Some("run_command")).is_empty());
}

/// An absent or `*` matcher matches all tools.
#[test]
fn absent_and_wildcard_matcher_match_all() {
    let hooks = json!({
        "PostToolUse": [
            { "hooks": [ { "type": "command", "command": "a" } ] },
            { "matcher": "*", "hooks": [ { "type": "command", "command": "b" } ] }
        ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    let selected = mgr.hooks_for(HookEvent::PostToolUse, Some("anything"));
    assert_eq!(selected.len(), 2);
}

/// A tool event with a real matcher but no tool name matches nothing (defensive).
#[test]
fn tool_event_real_matcher_without_tool_name_matches_none() {
    let hooks = json!({
        "PreToolUse": [
            { "matcher": "run_command", "hooks": [ { "type": "command", "command": "x" } ] }
        ]
    });
    let mgr = HookManager::from_config(Some(&hooks));
    assert!(mgr.hooks_for(HookEvent::PreToolUse, None).is_empty());
}

/// A registered skill's hooks are resolved with the skill root attached (so the
/// runner sets SUSPENDERS_SKILL_ROOT), and standing hooks fire first.
#[test]
fn skill_hooks_carry_skill_root_after_config() {
    let config = json!({
        "PostToolUse": [ { "hooks": [ { "type": "command", "command": "standing" } ] } ]
    });
    let mgr = HookManager::from_config(Some(&config));

    let skill_hooks = json!({
        "PostToolUse": [ { "hooks": [ { "type": "command", "command": "skill-fmt" } ] } ]
    });
    mgr.register_skill("formatter", "/skills/formatter", &skill_hooks);

    let selected = mgr.hooks_for(HookEvent::PostToolUse, Some("edit_file"));
    assert_eq!(selected.len(), 2);
    // Standing source first.
    assert_eq!(
        selected[0].hook.kind,
        HookKind::Command { command: "standing".to_string() }
    );
    assert!(selected[0].skill_root.is_none());
    // Skill source second, carrying its root.
    assert_eq!(
        selected[1].hook.kind,
        HookKind::Command { command: "skill-fmt".to_string() }
    );
    assert_eq!(selected[1].skill_root.as_deref(), Some("/skills/formatter"));
}

/// A malformed skill hooks block is recorded fail-open, labeled by skill name.
#[test]
fn malformed_skill_hooks_recorded_fail_open() {
    let mgr = HookManager::from_config(None);
    let bad = json!({ "Stop": [ { "hooks": [ { "type": "function" } ] } ] });
    mgr.register_skill("broken", "/skills/broken", &bad);
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.failures()[0].0, "skill broken");
    assert!(mgr.hooks_for(HookEvent::Stop, None).is_empty());
}

/// Registering the SAME skill twice is idempotent: its hooks are added once, not
/// doubled (ADR-0066).
#[test]
fn register_skill_is_idempotent() {
    let mgr = HookManager::from_config(None);
    let skill_hooks = json!({
        "Stop": [ { "hooks": [ { "type": "command", "command": "once" } ] } ]
    });
    mgr.register_skill("dup", "/skills/dup", &skill_hooks);
    mgr.register_skill("dup", "/skills/dup", &skill_hooks);
    assert_eq!(
        mgr.hooks_for(HookEvent::Stop, None).len(),
        1,
        "a skill invoked twice registers its hooks once"
    );
}

/// A registered skill with NO hooks block (an empty object) contributes nothing.
#[test]
fn register_skill_with_no_hooks_adds_nothing() {
    let mgr = HookManager::from_config(None);
    mgr.register_skill("empty", "/skills/empty", &json!({}));
    assert!(mgr.hooks_for(HookEvent::Stop, None).is_empty());
    assert!(mgr.failures().is_empty(), "an empty hooks object is not a failure");
}

/// A malformed config.json hooks block records a failure but the manager still
/// builds (fail-open), keeping the good hooks.
#[test]
fn malformed_config_is_fail_open() {
    let config = json!({
        "Nope": [],
        "Stop": [ { "hooks": [ { "type": "command", "command": "ok" } ] } ]
    });
    let mgr = HookManager::from_config(Some(&config));
    assert_eq!(mgr.failures().len(), 1);
    assert_eq!(mgr.hooks_for(HookEvent::Stop, None).len(), 1);
}
