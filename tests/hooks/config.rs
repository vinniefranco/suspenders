//! Unit tests for the hook config parser (ADR-0066): valid parse across the
//! three types, each malformed shape failing open, and the SKILL.md YAML -> Value
//! conversion feeding the SAME parser as config.json.

use super::*;
use serde_json::json;

/// A valid config.json `hooks` block with all three types parses into the
/// grouped-by-event shape, preserving matcher, the type-specific field, and the
/// timeout.
#[test]
fn parses_all_three_types() {
    let hooks = json!({
        "PreToolUse": [
            {
                "matcher": "run_command",
                "hooks": [
                    { "type": "command", "command": "guard.sh", "timeout": 5 },
                    { "type": "http", "url": "https://audit.example/hook" }
                ]
            }
        ],
        "Stop": [
            { "hooks": [ { "type": "prompt", "prompt": "vet: $ARGUMENTS" } ] }
        ]
    });

    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(failures.is_empty(), "clean block has no failures: {failures:?}");

    let pre = cfg.definitions(HookEvent::PreToolUse);
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].matcher.as_deref(), Some("run_command"));
    assert_eq!(pre[0].hooks.len(), 2);
    assert_eq!(
        pre[0].hooks[0],
        Hook {
            kind: HookKind::Command {
                command: "guard.sh".to_string()
            },
            timeout_secs: Some(5),
        }
    );
    assert_eq!(
        pre[0].hooks[1],
        Hook {
            kind: HookKind::Http {
                url: "https://audit.example/hook".to_string()
            },
            timeout_secs: None,
        }
    );

    let stop = cfg.definitions(HookEvent::Stop);
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].matcher, None);
    assert_eq!(
        stop[0].hooks[0].kind,
        HookKind::Prompt {
            prompt: "vet: $ARGUMENTS".to_string()
        }
    );
}

/// An event with no hooks is simply absent from the parsed map (not an empty
/// entry).
#[test]
fn absent_event_has_no_definitions() {
    let hooks = json!({ "Stop": [ { "hooks": [ { "type": "command", "command": "x" } ] } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.definitions(HookEvent::PreToolUse).is_empty());
    assert_eq!(cfg.definitions(HookEvent::Stop).len(), 1);
}

/// A non-object `hooks` value fails open: the whole block is skipped with one
/// failure.
#[test]
fn non_object_hooks_fails_open() {
    let hooks = json!([1, 2, 3]);
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("not an object"));
}

/// An unknown event name is skipped with a failure; the good events still parse.
#[test]
fn unknown_event_name_fails_open() {
    let hooks = json!({
        "NotAnEvent": [ { "hooks": [ { "type": "command", "command": "x" } ] } ],
        "Stop": [ { "hooks": [ { "type": "command", "command": "y" } ] } ]
    });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert_eq!(cfg.definitions(HookEvent::Stop).len(), 1);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("unknown hook event name"));
    assert!(failures[0].1.contains("NotAnEvent"));
}

/// qwen's non-event `hooks` fields (enabled/disabled/notifications) are skipped
/// SILENTLY - no failure recorded.
#[test]
fn non_event_fields_skipped_silently() {
    let hooks = json!({
        "enabled": true,
        "disabled": [],
        "notifications": { "x": 1 },
        "Stop": [ { "hooks": [ { "type": "command", "command": "x" } ] } ]
    });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(failures.is_empty(), "config fields are not failures: {failures:?}");
    assert_eq!(cfg.definitions(HookEvent::Stop).len(), 1);
}

/// A non-array definition list for an event is skipped with a failure.
#[test]
fn non_array_definitions_fails_open() {
    let hooks = json!({ "Stop": { "not": "an array" } });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("not an array"));
}

/// A definition that is not an object is skipped with a failure.
#[test]
fn non_object_definition_fails_open() {
    let hooks = json!({ "Stop": [ 42 ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("not an object"));
}

/// A definition with no `hooks` array is skipped with a failure.
#[test]
fn definition_without_hooks_array_fails_open() {
    let hooks = json!({ "Stop": [ { "matcher": "x" } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("no `hooks` array"));
}

/// A non-string matcher is dropped (ignored) with a failure, but the definition's
/// hooks still load - the matcher is a soft field.
#[test]
fn non_string_matcher_dropped_but_hooks_survive() {
    let hooks = json!({
        "PreToolUse": [ { "matcher": 123, "hooks": [ { "type": "command", "command": "x" } ] } ]
    });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    let defs = cfg.definitions(HookEvent::PreToolUse);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].matcher, None, "bad matcher dropped to None");
    assert_eq!(defs[0].hooks.len(), 1, "hooks still load");
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("matcher"));
}

/// A hook with a missing `type` is skipped with a failure; a sibling good hook in
/// the same definition survives.
#[test]
fn hook_missing_type_fails_open_but_siblings_survive() {
    let hooks = json!({
        "Stop": [ { "hooks": [
            { "command": "no-type-here" },
            { "type": "command", "command": "good" }
        ] } ]
    });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    let defs = cfg.definitions(HookEvent::Stop);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].hooks.len(), 1, "only the good hook survives");
    assert_eq!(
        defs[0].hooks[0].kind,
        HookKind::Command {
            command: "good".to_string()
        }
    );
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("missing or non-string `type`"));
}

/// A command hook missing its `command` field is skipped with a failure.
#[test]
fn command_hook_missing_command_fails_open() {
    let hooks = json!({ "Stop": [ { "hooks": [ { "type": "command" } ] } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty(), "definition with only bad hooks drops");
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("missing or empty `command`"));
}

/// An empty required field (url: "") is treated as missing - a hook that cannot
/// run.
#[test]
fn empty_required_field_treated_as_missing() {
    let hooks = json!({ "Stop": [ { "hooks": [ { "type": "http", "url": "" } ] } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("missing or empty `url`"));
}

/// The rejected `function` hook type (ADR-0066) is skipped with a failure naming
/// it unsupported.
#[test]
fn function_hook_type_rejected() {
    let hooks = json!({ "Stop": [ { "hooks": [ { "type": "function", "id": "cb" } ] } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("`function` hook type is not supported"));
}

/// An unknown hook type is skipped with a failure.
#[test]
fn unknown_hook_type_fails_open() {
    let hooks = json!({ "Stop": [ { "hooks": [ { "type": "carrier-pigeon", "note": "fly" } ] } ] });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    assert!(cfg.by_event.is_empty());
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("unknown hook type"));
}

/// A non-numeric timeout is a failure (the hook is skipped); the rest survives.
#[test]
fn non_numeric_timeout_fails_open() {
    let hooks = json!({
        "Stop": [ { "hooks": [
            { "type": "command", "command": "x", "timeout": "soon" },
            { "type": "command", "command": "y", "timeout": 3 }
        ] } ]
    });
    let mut failures = Vec::new();
    let cfg = parse_hooks(&hooks, "config.json", &mut failures);
    let defs = cfg.definitions(HookEvent::Stop);
    assert_eq!(defs[0].hooks.len(), 1);
    assert_eq!(defs[0].hooks[0].timeout_secs, Some(3));
    assert_eq!(failures.len(), 1);
    assert!(failures[0].1.contains("timeout"));
}

/// The failure context labels the source (a skill), matching the SkillManager
/// convention.
#[test]
fn failure_context_labels_the_source() {
    let hooks = json!({ "Nope": [] });
    let mut failures = Vec::new();
    let _ = parse_hooks(&hooks, "skill formatter", &mut failures);
    assert_eq!(failures[0].0, "skill formatter");
}

/// A SKILL.md `hooks:` YAML block converts to the SAME Value shape and parses
/// identically to config.json - one parser, both sources (ADR-0066).
#[test]
fn skill_yaml_block_parses_via_same_parser() {
    let yaml = "\
PostToolUse:
  - matcher: edit_file
    hooks:
      - type: command
        command: format.sh
        timeout: 10
";
    let value = hooks_value_from_yaml(yaml).expect("valid YAML converts to Value");
    let mut failures = Vec::new();
    let cfg = parse_hooks(&value, "skill formatter", &mut failures);
    assert!(failures.is_empty(), "clean YAML has no failures: {failures:?}");
    let defs = cfg.definitions(HookEvent::PostToolUse);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].matcher.as_deref(), Some("edit_file"));
    assert_eq!(
        defs[0].hooks[0],
        Hook {
            kind: HookKind::Command {
                command: "format.sh".to_string()
            },
            timeout_secs: Some(10),
        }
    );
}

/// A malformed YAML `hooks:` block is a fail-open Err (the caller drops the
/// skill's hooks), never a crash.
#[test]
fn malformed_skill_yaml_is_err() {
    let yaml = "PostToolUse:\n  - matcher: [unterminated\n";
    let result = hooks_value_from_yaml(yaml);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid YAML"));
}
