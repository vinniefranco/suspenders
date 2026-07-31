use super::*;
use crate::ui::slash;

#[test]
fn model_and_theme_are_handled_and_an_unknown_name_is_not() {
    assert!(handled("model").is_some());
    assert!(handled("theme").is_some());
    assert!(handled("compact").is_none());
    assert!(handled("").is_none());
}

// Adding a COMMANDS entry without a `handled` mapping would otherwise fail
// silently (ADR-0032's extension seam): assert every registered command is
// handled. This drives the real classifier - the same one `run`/`choose`
// match exhaustively - so a registry entry cannot outrun its adapter arm.
#[test]
fn every_registry_command_is_handled() {
    for c in slash::COMMANDS {
        assert!(handled(c.name).is_some(), "unhandled command: {}", c.name);
    }
}
