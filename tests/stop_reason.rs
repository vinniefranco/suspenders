use super::*;

// ---- wire names (the Session Log's serialized vocabulary) ----

#[test]
fn every_fixed_reason_round_trips_through_its_wire_name() {
    let fixed = [
        (StopReason::EndTurn, "end_turn"),
        (StopReason::ToolUse, "tool_use"),
        (StopReason::MaxTokens, "max_tokens"),
        (StopReason::StopSequence, "stop_sequence"),
        (StopReason::RunLimit, "turn_limit"),
        (StopReason::RunLimitStuck, "turn_limit_stuck"),
        (StopReason::Error, "error"),
        (StopReason::Unknown, "unknown"),
    ];
    for (reason, name) in fixed {
        assert_eq!(reason.as_str(), name);
        assert_eq!(StopReason::from_str(name), reason);
    }
}

#[test]
fn a_hook_custom_atom_round_trips_verbatim_never_degrading_to_unknown() {
    // The one open-vocabulary reason (ADR-0066): a Hook's `continue:false`
    // atom survives the Session Log byte-identical.
    let custom = StopReason::Custom("budget_hook".to_string());
    assert_eq!(custom.as_str(), "budget_hook");
    assert_eq!(StopReason::from_str("budget_hook"), custom);
}

#[test]
fn an_empty_name_degrades_to_unknown_not_an_empty_atom() {
    assert_eq!(StopReason::from_str(""), StopReason::Unknown);
}

#[test]
fn display_speaks_the_wire_name() {
    assert_eq!(StopReason::RunLimit.to_string(), "turn_limit");
    assert_eq!(StopReason::Custom("policy".into()).to_string(), "policy");
}
