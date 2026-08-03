use super::*;
use crate::ui::slash;

#[test]
fn model_theme_mcp_and_plan_are_handled_and_an_unknown_name_is_not() {
    assert!(is_handled("model"));
    assert!(is_handled("theme"));
    assert!(is_handled("mcp"));
    assert!(is_handled("plan"));
    assert!(!is_handled("compact"));
    assert!(!is_handled(""));
}

// Adding a COMMANDS entry without a `handled` mapping would otherwise fail
// silently (ADR-0032's extension seam): assert every registered command is
// handled. This drives the real classifier - the same one `run`/`choose`
// match exhaustively - so a registry entry cannot outrun its adapter arm.
#[test]
fn every_registry_command_is_handled() {
    for c in slash::COMMANDS {
        assert!(is_handled(c.name), "unhandled command: {}", c.name);
    }
}
