//! The explore Governor: consecutive Passes spent hand-exploring inline -
//! reading files or shelling out to search - instead of dispatching a Scout
//! draw the Explore Nudge (CONTEXT.md: Governor, Nudge, Scout; ADR-0026).
//!
//! * **Trigger**: `streak`, the private count of consecutive Passes whose
//!   Tool Calls were ALL exploration - read-only Tools or a search-shaped
//!   run_command ([`search_command`]). A Pass extends the streak only when it
//!   made at least one Tool Call and every one was exploration; any other
//!   Tool Call, or a Pass with no Tool Calls, resets it. The Pass's carried
//!   calls are a Ledger fact; the streak over them is this Governor's own.
//! * **Interventions**: rides the results tail at the answering moment
//!   ([`Explore::note_pass_calls`]) - the Explore Nudge merges into the
//!   trailing tool-results user message, where a small model actually
//!   attends.
//! * **Setpoints**: [`Setpoints`] - the firing cadence (every Nth consecutive
//!   exploration Pass; firing resets the streak so each fire starts a fresh
//!   window). Not user-exposed: resolution at launch is the [`Default`],
//!   because a Setpoint becomes user-configurable only when a real model has
//!   demanded a different value (CONTEXT.md: Setpoint).
//!
//! The wording lives in [`crate::voice`]; this module never authors nudge
//! strings.

pub mod search_command;

use serde_json::Value;

const EXPLORE_TOOLS: &[&str] = &["read_file", "list_files", "grep"];

/// The explore Governor's Setpoints (CONTEXT.md: Setpoint).
#[derive(Debug, Clone)]
pub struct Setpoints {
    /// The Explore Nudge fires on every Nth consecutive exploration Pass
    /// (Nth, 2Nth, 3Nth, ...).
    pub nudge_every: u64,
}

impl Default for Setpoints {
    fn default() -> Self {
        Setpoints { nudge_every: 3 }
    }
}

/// The explore Governor's private trigger state and resolved Setpoints.
#[derive(Debug, Clone, Default)]
pub struct Explore {
    setpoints: Setpoints,
    // Consecutive Passes whose Tool Calls were ALL exploration.
    streak: u64,
}

// qual:allow(coupling, oi) reason: "Explore also names the explore Tool (tools::explore); this impl sits with its own Governor type - OI is a name-collision false positive resolved by file-discovery order, which differs between local and CI"
impl Explore {
    pub fn new() -> Self {
        Explore::default()
    }

    /// Folds one Pass's Tool Calls into the exploration streak and returns
    /// whether the Explore Nudge fires on this Pass. Fires every
    /// `nudge_every`th consecutive exploration Pass, and firing resets the
    /// streak.
    pub fn note_pass_calls(&mut self, calls: &[(String, Value)]) -> bool {
        if exploration_only(calls) {
            let streak = self.streak + 1;
            if streak.is_multiple_of(self.setpoints.nudge_every) {
                self.streak = 0;
                true
            } else {
                self.streak = streak;
                false
            }
        } else {
            self.streak = 0;
            false
        }
    }
}

// Exploration: at least one Tool Call, all of them exploration. An empty batch
// is not exploration (it resets).
fn exploration_only(calls: &[(String, Value)]) -> bool {
    !calls.is_empty() && calls.iter().all(exploration_call)
}

// A read-only Tool, or a run_command whose command string is search-shaped.
fn exploration_call(call: &(String, Value)) -> bool {
    let (name, input) = call;
    if EXPLORE_TOOLS.contains(&name.as_str()) {
        true
    } else if name == "run_command" {
        search_command::search_shaped(command_string(input))
    } else {
        false
    }
}

fn command_string(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // note_pass folds one Pass's Tool Calls into the streak and returns whether
    // the Explore Nudge fires. Accepts bare names (input defaults to {}).
    fn note_pass(explore: &mut Explore, calls: &[&str]) -> bool {
        let calls: Vec<(String, Value)> =
            calls.iter().map(|n| (n.to_string(), json!({}))).collect();
        explore.note_pass_calls(&calls)
    }

    fn note_pass_pairs(explore: &mut Explore, calls: &[(&str, Value)]) -> bool {
        let calls: Vec<(String, Value)> = calls
            .iter()
            .map(|(n, i)| (n.to_string(), i.clone()))
            .collect();
        explore.note_pass_calls(&calls)
    }

    const READONLY: &[&str] = &["read_file", "list_files", "grep"];

    #[test]
    fn fires_on_the_3rd_consecutive_exploration_pass() {
        let mut explore = Explore::new();
        let f1 = note_pass(&mut explore, &["read_file"]);
        let f2 = note_pass(&mut explore, &["list_files"]);
        let f3 = note_pass(&mut explore, &["grep"]);

        assert!(!f1);
        assert!(!f2);
        assert!(f3);
    }

    #[test]
    fn fires_again_on_the_6th_9th() {
        let mut explore = Explore::new();
        note_pass(&mut explore, &["read_file"]);
        note_pass(&mut explore, &["read_file"]);
        assert!(note_pass(&mut explore, &["read_file"]));

        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(note_pass(&mut explore, &["read_file"]));

        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(note_pass(&mut explore, &["read_file"]));
    }

    #[test]
    fn a_mix_of_all_three_readonly_tools_in_one_pass_counts() {
        let mut explore = Explore::new();
        note_pass(&mut explore, READONLY);
        note_pass(&mut explore, READONLY);
        assert!(note_pass(&mut explore, READONLY));
    }

    #[test]
    fn a_pass_with_no_tool_calls_resets_the_streak() {
        let mut explore = Explore::new();
        note_pass(&mut explore, &["read_file"]);
        note_pass(&mut explore, &["read_file"]);
        note_pass(&mut explore, &[]);

        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(note_pass(&mut explore, &["read_file"]));
    }

    #[test]
    fn a_pass_containing_a_non_readonly_tool_resets_the_streak() {
        for non_readonly in ["explore", "plan", "edit_file", "write_file", "run_command"] {
            let mut explore = Explore::new();
            note_pass(&mut explore, &["read_file"]);
            note_pass(&mut explore, &["grep"]);
            let fired = note_pass(&mut explore, &["read_file", non_readonly]);
            assert!(!fired, "expected reset with {non_readonly}");
        }
    }

    #[test]
    fn a_web_fetch_pass_resets_the_streak_like_any_non_exploration_tool() {
        // Doc-reading over the network is not local exploration: it must not
        // count toward the Explore Nudge, and it resets the streak (ADR-0024).
        let mut explore = Explore::new();
        note_pass(&mut explore, &["read_file"]);
        note_pass(&mut explore, &["grep"]);
        let fired = note_pass_pairs(
            &mut explore,
            &[("web_fetch", json!({"url": "https://docs.rs/tokio"}))],
        );
        assert!(!fired);

        // The streak restarted: three fresh exploration Passes fire again.
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(note_pass(&mut explore, &["read_file"]));
    }

    #[test]
    fn a_search_shaped_run_command_counts_as_exploration() {
        let grep = ("run_command", json!({"command": "grep -rn foo lib"}));
        let find = (
            "run_command",
            json!({"command": "find . -name '*.ex' | xargs grep foo"}),
        );

        let mut explore = Explore::new();
        let f1 = note_pass_pairs(&mut explore, &[grep]);
        let f2 = note_pass_pairs(&mut explore, &[find]);
        let f3 = note_pass_pairs(&mut explore, &[("read_file", json!({"path": "a.ex"}))]);
        assert!(!f1);
        assert!(!f2);
        assert!(f3);
    }

    #[test]
    fn a_mix_test_run_command_resets_the_streak() {
        let mut explore = Explore::new();
        note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "grep foo lib"}))],
        );
        note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "grep bar lib"}))],
        );
        let fired = note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "mix test"}))],
        );
        assert!(!fired);

        assert!(!note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "grep a lib"}))]
        ));
        assert!(!note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "grep b lib"}))]
        ));
        assert!(note_pass_pairs(
            &mut explore,
            &[("run_command", json!({"command": "grep c lib"}))]
        ));
    }

    #[test]
    fn a_search_run_command_mixed_with_a_readonly_tool_counts() {
        let call: &[(&str, Value)] = &[
            ("read_file", json!({"path": "a.ex"})),
            ("run_command", json!({"command": "grep x lib"})),
        ];
        let mut explore = Explore::new();
        note_pass_pairs(&mut explore, call);
        note_pass_pairs(&mut explore, call);
        assert!(note_pass_pairs(&mut explore, call));
    }

    #[test]
    fn a_non_search_run_command_resets_even_alongside_readonly_tools() {
        let mut explore = Explore::new();
        note_pass(&mut explore, &["read_file"]);
        note_pass(&mut explore, &["grep"]);

        let reset: &[(&str, Value)] = &[
            ("read_file", json!({"path": "a.ex"})),
            ("run_command", json!({"command": "git status"})),
        ];
        assert!(!note_pass_pairs(&mut explore, reset));
    }

    #[test]
    fn the_streak_resets_after_firing() {
        let mut explore = Explore::new();
        note_pass(&mut explore, &["grep"]);
        note_pass(&mut explore, &["grep"]);
        note_pass(&mut explore, &["grep"]);

        // A non-exploration Pass right after a fire resets cleanly.
        note_pass(&mut explore, &["edit_file"]);
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(!note_pass(&mut explore, &["read_file"]));
        assert!(note_pass(&mut explore, &["read_file"]));
    }
}
