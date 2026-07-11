//! The empty Governor: a reply that arrives empty — no content once tool_use
//! blocks are dropped, or a parroted empty-response marker — draws the
//! Empty-response Nudge, and repeated emptiness escalates to the break-glass
//! no-think rescue (CONTEXT.md: Governor, Nudge, Thinking; ADR-0026).
//!
//! * **Trigger**: [`is_empty_reply`], a pure predicate over the finishing
//!   response's content blocks, gated by the private once-per-Turn cap
//!   (`nudged`, re-armed by progress — [`Empty::note_progress`]) and the
//!   rescue's private arming state (`rescue_next`, `rescue_sticky`,
//!   `empty_count`). Progress never disarms the rescue.
//! * **Interventions**: stands alone as a user message at the finish
//!   settlement (the Empty-response Nudge — the model gets one more Pass),
//!   and silences Thinking for a Pass at the request-shaping moment (the
//!   rescue: after the Nudge fires, the very next model call carries
//!   no_think, then reverts — unless the SECOND empty of the Turn has made
//!   the rescue sticky). Consulting the shaping moment consumes the one-Pass
//!   arm ([`Empty::consume_rescue`]).
//! * **Setpoints**: [`Setpoints`] — whether the rescue may arm at all. This
//!   one IS user-exposed: the Session's `no_think_rescue` knob feeds it,
//!   resolved at Session construction; the default matches the Session's.
//!   With the knob off the Nudge still fires and the empty count still
//!   advances, but nothing arms.
//!
//! The wording lives in [`crate::voice`]; this module never authors nudge
//! strings.

use serde_json::Value;

use crate::content::ContentBlock;
use crate::voice;

/// The empty Governor's Setpoints (CONTEXT.md: Setpoint). Fed by the
/// Session's resolved `no_think_rescue` knob at Turn start.
#[derive(Debug, Clone)]
pub struct Setpoints {
    /// May the break-glass no-think rescue arm after an Empty-response Nudge?
    pub no_think_rescue: bool,
}

impl Default for Setpoints {
    fn default() -> Self {
        Setpoints {
            no_think_rescue: true,
        }
    }
}

/// The empty Governor's private trigger state and resolved Setpoints.
#[derive(Debug, Clone, Default)]
pub struct Empty {
    setpoints: Setpoints,
    nudged: bool,
    rescue_next: bool,
    rescue_sticky: bool,
    empty_count: u64,
}

impl Empty {
    pub fn new(setpoints: Setpoints) -> Self {
        Empty {
            setpoints,
            nudged: false,
            rescue_next: false,
            rescue_sticky: false,
            empty_count: 0,
        }
    }

    /// Does end-of-turn owe the Empty-response Nudge?
    pub fn due(&self) -> bool {
        !self.nudged
    }

    /// The Empty-response Nudge fired: the once-per-Turn cap sets (UNTIL
    /// progress re-arms it), the empty count advances, and — when the
    /// setpoint allows — the no-think rescue arms for the very next model
    /// call, going STICKY on the second empty of the Turn.
    pub fn note_fired(&mut self) {
        self.empty_count += 1;
        self.rescue_next = self.setpoints.no_think_rescue;
        self.rescue_sticky =
            self.rescue_sticky || (self.setpoints.no_think_rescue && self.empty_count >= 2);
        self.nudged = true;
    }

    /// Records a Pass's Tool Calls for the re-arming rule. A Pass that made at
    /// least one Tool Call is progress: it re-arms the Nudge's cap. An idle
    /// Pass (no Tool Calls) never re-arms, and progress never disarms the
    /// rescue.
    pub fn note_progress(&mut self, calls: &[(String, Value)]) {
        if calls.is_empty() {
            return;
        }
        self.nudged = false;
    }

    /// Should the next model call carry no_think? (armed or sticky)
    pub fn rescue_armed(&self) -> bool {
        self.rescue_next || self.rescue_sticky
    }

    /// Consumes the one-Pass rescue arm: the rescue rides exactly the Pass
    /// right after the Empty-response Nudge fired — unless it has gone sticky.
    pub fn consume_rescue(&mut self) {
        self.rescue_next = false;
    }
}

/// An empty reply: zero content blocks once tool_use blocks are dropped, OR a
/// parroted empty-response marker. Pure over what the model sent.
pub fn is_empty_reply(blocks: &[ContentBlock]) -> bool {
    let kept: Vec<&ContentBlock> = blocks.iter().filter(|b| !b.is_tool_use()).collect();
    if kept.is_empty() {
        true
    } else {
        marker_parrot(&kept)
    }
}

fn marker_parrot(blocks: &[&ContentBlock]) -> bool {
    let all_text = blocks
        .iter()
        .all(|b| matches!(b, ContentBlock::Text { .. }));
    if !all_text {
        return false;
    }
    let joined = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim() == voice::empty_response_marker()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn on() -> Empty {
        Empty::new(Setpoints {
            no_think_rescue: true,
        })
    }

    // ----- empty-response nudge re-arm -----

    #[test]
    fn empty_a_pass_with_a_tool_call_re_arms_it() {
        let mut empty = on();
        empty.note_fired();
        empty.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(empty.due());
    }

    #[test]
    fn empty_a_pass_with_no_tool_calls_does_not_re_arm_it() {
        let mut empty = on();
        empty.note_fired();
        empty.note_progress(&[]);

        assert!(!empty.due());
    }

    // ----- no-think rescue -----

    #[test]
    fn the_first_empty_arms_one_pass_consuming_reverts() {
        let mut empty = on();
        empty.note_fired();
        assert!(empty.rescue_armed());

        empty.consume_rescue();
        assert!(!empty.rescue_armed());
    }

    #[test]
    fn the_second_empty_makes_the_rescue_sticky() {
        let mut empty = on();
        empty.note_fired();
        empty.consume_rescue();
        empty.note_fired();
        empty.consume_rescue();

        assert!(empty.rescue_armed());
    }

    #[test]
    fn an_off_knob_never_arms_but_the_count_advances() {
        let mut empty = Empty::new(Setpoints {
            no_think_rescue: false,
        });
        empty.note_fired();
        empty.note_fired();

        assert!(!empty.rescue_armed());
        assert_eq!(empty.empty_count, 2);
    }

    #[test]
    fn progress_never_disarms_the_sticky_rescue() {
        let mut empty = on();
        empty.note_fired();
        empty.note_fired();
        empty.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(empty.rescue_armed());
    }
}
