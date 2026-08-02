//! Unit tests for the HookEvent enum (ADR-0066): serde maps every variant to its
//! exact qwen wire name, parse is the inverse and rejects non-events, and the
//! tool-event classification matches qwen's matcher-target kinds.

use super::*;

/// All sixteen variants round-trip through their exact qwen wire name.
#[test]
fn wire_names_match_qwen_verbatim() {
    let cases = [
        (HookEvent::PreToolUse, "PreToolUse"),
        (HookEvent::PostToolUse, "PostToolUse"),
        (HookEvent::PostToolUseFailure, "PostToolUseFailure"),
        (HookEvent::UserPromptSubmit, "UserPromptSubmit"),
        (HookEvent::SessionStart, "SessionStart"),
        (HookEvent::SessionEnd, "SessionEnd"),
        (HookEvent::Stop, "Stop"),
        (HookEvent::StopFailure, "StopFailure"),
        (HookEvent::SubagentStart, "SubagentStart"),
        (HookEvent::SubagentStop, "SubagentStop"),
        (HookEvent::Notification, "Notification"),
        (HookEvent::PermissionRequest, "PermissionRequest"),
        (HookEvent::PreCompact, "PreCompact"),
        (HookEvent::PostCompact, "PostCompact"),
        (HookEvent::TodoCreated, "TodoCreated"),
        (HookEvent::TodoCompleted, "TodoCompleted"),
    ];
    // Exactly sixteen events (the full qwen set, no subset).
    assert_eq!(cases.len(), 16);
    for (event, wire) in cases {
        // wire_name and the serde form agree.
        assert_eq!(event.wire_name(), wire);
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json, serde_json::Value::String(wire.to_string()));
        // parse is the inverse.
        assert_eq!(HookEvent::parse(wire), Some(event));
    }
}

/// An unknown key does not parse (config parsing records it as a failure).
#[test]
fn parse_rejects_unknown() {
    assert_eq!(HookEvent::parse("NotAnEvent"), None);
    assert_eq!(HookEvent::parse("enabled"), None);
    assert_eq!(HookEvent::parse(""), None);
}

/// The four tool-dispatch events are tool events; the rest are not (qwen's
/// getHookMatcherTarget tool arm).
#[test]
fn tool_event_classification() {
    for e in [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PermissionRequest,
    ] {
        assert!(e.is_tool_event(), "{e:?} is a tool event");
    }
    for e in [
        HookEvent::Stop,
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::Notification,
        HookEvent::TodoCreated,
    ] {
        assert!(!e.is_tool_event(), "{e:?} is not a tool event");
    }
}
