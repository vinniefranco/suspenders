//! Unit tests for the decision-protocol parser (ADR-0066): parsing across ALL
//! qwen output shapes (block/deny/allow/ask, continue:false+stopReason,
//! additionalContext escaping, permissionDecision with base-decision fallback,
//! suppressOutput, empty/absent) and the ported helper methods.

use super::*;
use serde_json::json;

/// An empty `{}` output parses to the all-None default: it steers nothing.
#[test]
fn empty_object_steers_nothing() {
    let out = HookOutcome::parse("{}").unwrap();
    assert_eq!(out, HookOutcome::default());
    assert!(!out.is_blocking());
    assert!(!out.should_stop());
    assert!(out.additional_context().is_none());
    assert!(out.permission_decision().is_none());
    assert_eq!(out.blocking_error(), (false, String::new()));
}

/// A `decision: block` with a reason is a blocking decision; the blocking error
/// carries the reason.
#[test]
fn block_decision_is_blocking() {
    let out = HookOutcome::parse(r#"{"decision":"block","reason":"nope"}"#).unwrap();
    assert_eq!(out.decision, Some(Decision::Block));
    assert!(out.is_blocking());
    assert_eq!(out.effective_reason(), "nope");
    assert_eq!(out.blocking_error(), (true, "nope".to_string()));
}

/// A `decision: deny` is equally blocking (qwen: block || deny).
#[test]
fn deny_decision_is_blocking() {
    let out = HookOutcome::parse(r#"{"decision":"deny","reason":"forbidden"}"#).unwrap();
    assert!(out.is_blocking());
    assert_eq!(out.blocking_error(), (true, "forbidden".to_string()));
}

/// `allow` and `approve` are NOT blocking.
#[test]
fn allow_and_approve_not_blocking() {
    let allow = HookOutcome::parse(r#"{"decision":"allow"}"#).unwrap();
    let approve = HookOutcome::parse(r#"{"decision":"approve"}"#).unwrap();
    assert!(!allow.is_blocking());
    assert!(!approve.is_blocking());
    assert_eq!(allow.decision, Some(Decision::Allow));
    assert_eq!(approve.decision, Some(Decision::Approve));
}

/// `ask` is a distinct decision, not blocking.
#[test]
fn ask_decision_parses() {
    let out = HookOutcome::parse(r#"{"decision":"ask","reason":"confirm?"}"#).unwrap();
    assert_eq!(out.decision, Some(Decision::Ask));
    assert!(!out.is_blocking());
}

/// `continue: false` requests a stop; effective reason prefers stopReason.
#[test]
fn continue_false_with_stop_reason() {
    let out = HookOutcome::parse(r#"{"continue":false,"stopReason":"halt now"}"#).unwrap();
    assert!(out.should_stop());
    assert_eq!(out.effective_reason(), "halt now");
    assert_eq!(out.stop_hook_feedback().as_deref(), Some("Stop hook feedback:\nhalt now"));
}

/// effectiveReason falls back stopReason -> reason -> the sentinel.
#[test]
fn effective_reason_fallback_chain() {
    let only_reason = HookOutcome::parse(r#"{"reason":"because"}"#).unwrap();
    assert_eq!(only_reason.effective_reason(), "because");
    let neither = HookOutcome::parse("{}").unwrap();
    assert_eq!(neither.effective_reason(), "No reason provided");
}

/// stopHookFeedback is None when no stopReason is set.
#[test]
fn stop_hook_feedback_absent() {
    let out = HookOutcome::parse(r#"{"reason":"x"}"#).unwrap();
    assert!(out.stop_hook_feedback().is_none());
}

/// additionalContext is read from hookSpecificOutput and its `<`/`>` escaped to
/// block tag injection (qwen sanitize).
#[test]
fn additional_context_escaped() {
    let out = HookOutcome::parse(
        r#"{"hookSpecificOutput":{"additionalContext":"<policy>note</policy>"}}"#,
    )
    .unwrap();
    assert_eq!(
        out.additional_context().as_deref(),
        Some("&lt;policy&gt;note&lt;/policy&gt;")
    );
}

/// A non-string additionalContext yields None (qwen typeof guard).
#[test]
fn additional_context_non_string_is_none() {
    let out = HookOutcome::parse(r#"{"hookSpecificOutput":{"additionalContext":42}}"#).unwrap();
    assert!(out.additional_context().is_none());
}

/// permissionDecision in hookSpecificOutput wins directly (allow/deny/ask).
#[test]
fn permission_decision_from_hook_specific_output() {
    let allow = HookOutcome::parse(
        r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
    )
    .unwrap();
    assert_eq!(allow.permission_decision(), Some(PermissionDecision::Allow));

    let deny = HookOutcome::parse(
        r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#,
    )
    .unwrap();
    assert_eq!(deny.permission_decision(), Some(PermissionDecision::Deny));

    let ask = HookOutcome::parse(
        r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
    )
    .unwrap();
    assert_eq!(ask.permission_decision(), Some(PermissionDecision::Ask));
}

/// With no hookSpecificOutput, the base `decision` maps: approve/allow -> allow,
/// deny/block -> deny, ask -> ask (qwen's fallback).
#[test]
fn permission_decision_falls_back_to_base_decision() {
    let cases = [
        (r#"{"decision":"approve"}"#, PermissionDecision::Allow),
        (r#"{"decision":"allow"}"#, PermissionDecision::Allow),
        (r#"{"decision":"deny"}"#, PermissionDecision::Deny),
        (r#"{"decision":"block"}"#, PermissionDecision::Deny),
        (r#"{"decision":"ask"}"#, PermissionDecision::Ask),
    ];
    for (json_text, expected) in cases {
        let out = HookOutcome::parse(json_text).unwrap();
        assert_eq!(
            out.permission_decision(),
            Some(expected),
            "case {json_text}"
        );
    }
}

/// An INVALID permissionDecision string falls through to the base decision
/// mapping rather than short-circuiting to None (qwen ignores the bad string).
#[test]
fn invalid_permission_decision_falls_through_to_base() {
    let out = HookOutcome::parse(
        r#"{"decision":"block","hookSpecificOutput":{"permissionDecision":"maybe"}}"#,
    )
    .unwrap();
    assert_eq!(out.permission_decision(), Some(PermissionDecision::Deny));
}

/// No decision and no permissionDecision yields None.
#[test]
fn permission_decision_absent_is_none() {
    let out = HookOutcome::parse(r#"{"reason":"x"}"#).unwrap();
    assert!(out.permission_decision().is_none());
}

/// permissionDecisionReason is read from hookSpecificOutput, else the base reason.
#[test]
fn permission_decision_reason_prefers_hook_specific() {
    let hso = HookOutcome::parse(
        r#"{"reason":"base","hookSpecificOutput":{"permissionDecisionReason":"specific"}}"#,
    )
    .unwrap();
    assert_eq!(hso.permission_decision_reason().as_deref(), Some("specific"));

    let base = HookOutcome::parse(r#"{"reason":"base"}"#).unwrap();
    assert_eq!(base.permission_decision_reason().as_deref(), Some("base"));
}

/// suppressOutput parses as a bool.
#[test]
fn suppress_output_parses() {
    let out = HookOutcome::parse(r#"{"suppressOutput":true}"#).unwrap();
    assert_eq!(out.suppress_output, Some(true));
}

/// PostToolUse allow-by-default: no explicit decision defaults to allow with the
/// "No reason provided" reason (qwen's PostToolUseHookOutput).
#[test]
fn post_tool_use_defaults_to_allow() {
    let out = HookOutcome::parse("{}").unwrap();
    let (decision, reason) = out.post_tool_use_decision();
    assert_eq!(decision, Decision::Allow);
    assert_eq!(reason, "No reason provided");
}

/// PostToolUse honors an explicit decision + reason when present.
#[test]
fn post_tool_use_honors_explicit() {
    let out = HookOutcome::parse(r#"{"decision":"deny","reason":"lint failed"}"#).unwrap();
    let (decision, reason) = out.post_tool_use_decision();
    assert_eq!(decision, Decision::Deny);
    assert_eq!(reason, "lint failed");
}

/// A double-encoded JSON string is unwrapped once (qwen's JSON.parse(JSON.parse)).
#[test]
fn double_encoded_json_string_unwrapped() {
    let inner = json!({"decision":"block","reason":"x"});
    let double = serde_json::Value::String(inner.to_string());
    let text = double.to_string();
    let out = HookOutcome::parse(&text).unwrap();
    assert_eq!(out.decision, Some(Decision::Block));
}

/// A non-object JSON value (bare number/array) is an Err.
#[test]
fn non_object_json_is_err() {
    assert!(HookOutcome::parse("42").is_err());
    assert!(HookOutcome::parse("[1,2]").is_err());
}

/// Unparseable text is an Err (the runner turns this into a fail-open outcome).
#[test]
fn unparseable_text_is_err() {
    assert!(HookOutcome::parse("not json at all").is_err());
    assert!(HookOutcome::parse("").is_err());
}
