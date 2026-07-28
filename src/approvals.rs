//! Approvals - the pure fold over the Session's Approval state (CONTEXT.md:
//! Approval, Standing Approval; ADR-0005).
//!
//! Owns the one pending Approval (the open modal) and the Session's Standing
//! Approvals. Returns verdicts; the Agent hosts this struct and translates the
//! verdicts into the actual `send` to the Run task and the broadcast to
//! subscribers - this module knows nothing about task handles or events.
//!
//! Standing Approval matching is string equality only - no prefix, glob, or
//! whitespace normalization (`mix  test` ≠ `mix test`). Every widening rule is
//! a place where the model could compose an unapproved command out of an
//! approved stem (ADR-0005).

use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// The Approval policy: the one place that declares which Tools gate and, for
/// each, the field whose value the user reads in the modal (and a Standing
/// Approval matches by exact string equality - ADR-0005). run_command shows
/// the command (arbitrary code); web_fetch shows the URL (the one Tool that
/// reaches outside the Project Root - ADR-0024).
///
/// One row per gated Tool ties "this gates" to "this is the field" so the two
/// facts can never disagree. A gated Tool with the field missing or non-string
/// still gates, reading the empty string - the gate is about the Tool, not the
/// input's shape.
const GATED: &[(&str, &str)] = &[("run_command", "command"), ("web_fetch", "url")];

/// The single Approval-gate query over a Tool Call (name + input): `Some(text)`
/// means the Call gates and `text` is exactly what the user reads (and what a
/// Standing Approval matches); `None` means no gate. Because the same lookup
/// answers both facts, "does this gate" and "what text" can never disagree.
///
/// The text is read from the extension-adjusted input the caller hands over.
pub fn gate_text(name: &str, input: &Value) -> Option<String> {
    let (_, field) = GATED.iter().find(|(tool, _)| *tool == name)?;
    Some(
        input
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

/// A unique identifier for an Approval request, standing in for baud's
/// `make_ref()`. In the wired Agent the id is minted by the Run Loop and
/// carried as the opaque string the `request_approval` Dep hands over (baud's
/// `make_ref()` reference); the Agent's Approvals fold keys on that same string
/// so a decision matches the pending modal. [`ApprovalId::new`] mints a fresh,
/// never-repeating id for standalone use (tests).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalId(String);

impl Default for ApprovalId {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ApprovalId(format!(
            "approval-{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Wraps the opaque reference the Run Loop minted (`request_approval`'s
    /// `id` argument) so the Agent's fold keys on the same string the Loop and
    /// the UI both hold.
    pub fn from_ref(id: impl Into<String>) -> Self {
        ApprovalId(id.into())
    }

    /// The opaque reference string (for the `approval_request` /
    /// `approval_resolved` events and the reply-channel key).
    pub fn as_ref_str(&self) -> &str {
        &self.0
    }
}

/// The user's decision on the pending Approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    ApproveAlways,
}

/// The one pending Approval: the open modal's id and its exact command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: ApprovalId,
    pub command: String,
}

/// The verdict of folding in an Approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A Standing Approval covers the exact command string: answer the Run
    /// approved immediately, no modal (the caller emits `approval_auto`).
    Auto(Approvals),
    /// No cover: the request becomes the pending Approval (the caller
    /// broadcasts `approval_request` so the UI opens the modal).
    Pending(Approvals),
}

/// The verdict of folding in the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decide {
    /// The id matches the pending Approval: relay `approved` to the waiting
    /// Run. `ApproveAlways` records the pending command as Standing first.
    Forward(bool, Approvals),
    /// Stale or duplicate id, or nothing pending: drop it, so late decisions
    /// never pile up in the Run task's mailbox.
    Ignore(Approvals),
}

/// The Approval state: the one pending Approval (or none) and the Session's
/// Standing Approvals (string-equality set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    pub pending: Option<Pending>,
    pub standing: HashSet<String>,
}

impl Approvals {
    pub fn new() -> Self {
        Approvals::default()
    }

    /// Folds in an Approval request from the Run.
    pub fn request(mut self, id: ApprovalId, command: impl Into<String>) -> Request {
        let command = command.into();
        if self.standing.contains(&command) {
            Request::Auto(self)
        } else {
            self.pending = Some(Pending { id, command });
            Request::Pending(self)
        }
    }

    /// Folds in the user's decision.
    pub fn decide(mut self, id: ApprovalId, decision: Decision) -> Decide {
        match &self.pending {
            Some(pending) if pending.id == id => {
                if decision == Decision::ApproveAlways {
                    let command = pending.command.clone();
                    self.standing.insert(command);
                }
                self.pending = None;
                Decide::Forward(decision != Decision::Deny, self)
            }
            _ => Decide::Ignore(self),
        }
    }

    /// Clears the pending Approval (the Run it belonged to is gone: settled or
    /// freshly started). Standing Approvals survive - they are Session-scoped.
    pub fn reset(mut self) -> Self {
        self.pending = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> ApprovalId {
        ApprovalId::new()
    }

    // ---- gate_text: the one Approval-gate query ----

    #[test]
    fn gate_text_gates_run_command_and_web_fetch_and_nothing_else() {
        assert!(gate_text("run_command", &json!({"command": "mix test"})).is_some());
        assert!(gate_text("web_fetch", &json!({"url": "https://docs.rs/tokio"})).is_some());

        for name in [
            "plan",
            "read_file",
            "list_files",
            "grep",
            "explore",
            "edit_file",
            "write_file",
        ] {
            assert_eq!(gate_text(name, &json!({})), None, "{name} must not gate");
        }
        assert_eq!(gate_text("no_such_tool", &json!({})), None);
    }

    #[test]
    fn gate_text_is_the_command_for_run_command() {
        assert_eq!(
            gate_text("run_command", &json!({"command": "mix test"})),
            Some("mix test".to_string())
        );
    }

    #[test]
    fn gate_text_is_the_url_for_web_fetch() {
        assert_eq!(
            gate_text("web_fetch", &json!({"url": "https://docs.rs/tokio"})),
            Some("https://docs.rs/tokio".to_string())
        );
    }

    #[test]
    fn gate_text_is_none_for_tools_that_need_no_approval() {
        assert_eq!(gate_text("read_file", &json!({"path": "a.ex"})), None);
        assert_eq!(gate_text("no_such_tool", &json!({})), None);
    }

    #[test]
    fn gate_text_falls_back_to_empty_when_the_field_is_missing_or_non_string() {
        assert_eq!(gate_text("run_command", &json!({})), Some(String::new()));
        assert_eq!(
            gate_text("web_fetch", &json!({"url": 42})),
            Some(String::new())
        );
    }

    // An Approvals with a Standing Approval for `command`, nothing pending.
    fn granted(command: &str) -> Approvals {
        let Request::Pending(approvals) = Approvals::new().request(id(), command) else {
            panic!("expected pending");
        };
        // Reuse the pending id to record it as standing.
        let pending_id = approvals.pending.as_ref().unwrap().id.clone();
        let Decide::Forward(true, approvals) =
            approvals.decide(pending_id, Decision::ApproveAlways)
        else {
            panic!("expected forward true");
        };
        approvals
    }

    // ---- request/3 ----

    #[test]
    fn with_no_standing_approval_the_request_becomes_pending() {
        let approvals = Approvals::new();
        let r = id();

        let Request::Pending(approvals) = approvals.request(r.clone(), "mix test") else {
            panic!("expected pending");
        };
        assert_eq!(
            approvals.pending,
            Some(Pending {
                id: r,
                command: "mix test".into()
            })
        );
    }

    #[test]
    fn a_standing_approval_covering_the_exact_command_answers_auto_with_nothing_pending() {
        let Request::Auto(approvals) = granted("mix test").request(id(), "mix test") else {
            panic!("expected auto");
        };
        assert_eq!(approvals.pending, None);
    }

    #[test]
    fn matching_is_string_equality_only_whitespace_variants_are_different_commands() {
        let approvals = granted("mix test");

        assert!(matches!(
            approvals.clone().request(id(), "mix test"),
            Request::Auto(_)
        ));
        assert!(matches!(
            approvals.clone().request(id(), "mix  test"),
            Request::Pending(_)
        ));
        assert!(matches!(
            approvals.clone().request(id(), "mix test "),
            Request::Pending(_)
        ));
        assert!(matches!(
            approvals.clone().request(id(), "MIX TEST"),
            Request::Pending(_)
        ));
    }

    #[test]
    fn no_prefix_widening_an_approved_stem_does_not_cover_a_longer_command() {
        let approvals = granted("mix test");

        assert!(matches!(
            approvals.request(id(), "mix test --seed 0 && rm -rf /"),
            Request::Pending(_)
        ));
    }

    // ---- decide/3 ----

    #[test]
    fn approve_forwards_approved_without_recording_a_standing_approval() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };

        let Decide::Forward(true, approvals) = approvals.decide(r, Decision::Approve) else {
            panic!("expected forward true");
        };
        assert_eq!(approvals.pending, None);
        assert!(matches!(
            approvals.request(id(), "mix test"),
            Request::Pending(_)
        ));
    }

    #[test]
    fn deny_forwards_not_approved_and_records_nothing() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };

        let Decide::Forward(false, approvals) = approvals.decide(r, Decision::Deny) else {
            panic!("expected forward false");
        };
        assert_eq!(approvals.pending, None);
        assert!(matches!(
            approvals.request(id(), "mix test"),
            Request::Pending(_)
        ));
    }

    #[test]
    fn approve_always_forwards_approved_and_records_the_exact_pending_command() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };

        let Decide::Forward(true, approvals) = approvals.decide(r, Decision::ApproveAlways) else {
            panic!("expected forward true");
        };
        assert!(matches!(
            approvals.request(id(), "mix test"),
            Request::Auto(_)
        ));
    }

    #[test]
    fn a_stale_or_unknown_id_is_ignored_leaving_the_pending_approval_open() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };

        let Decide::Ignore(approvals) = approvals.decide(id(), Decision::Approve) else {
            panic!("expected ignore");
        };
        assert_eq!(
            approvals.pending,
            Some(Pending {
                id: r,
                command: "mix test".into()
            })
        );
    }

    #[test]
    fn a_duplicate_decision_after_the_forward_is_ignored_not_forwarded_twice() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };
        let Decide::Forward(true, approvals) = approvals.decide(r.clone(), Decision::Approve)
        else {
            panic!();
        };

        assert!(matches!(
            approvals.decide(r, Decision::Approve),
            Decide::Ignore(_)
        ));
    }

    #[test]
    fn with_nothing_pending_every_decision_is_ignored() {
        assert!(matches!(
            Approvals::new().decide(id(), Decision::ApproveAlways),
            Decide::Ignore(_)
        ));
    }

    // ---- reset/1 ----

    #[test]
    fn clears_the_pending_approval() {
        let r = id();
        let Request::Pending(approvals) = Approvals::new().request(r.clone(), "mix test") else {
            panic!();
        };
        let approvals = approvals.reset();

        assert_eq!(approvals.pending, None);
        assert!(matches!(
            approvals.decide(r, Decision::Approve),
            Decide::Ignore(_)
        ));
    }

    #[test]
    fn standing_approvals_survive_they_are_session_scoped_not_run_scoped() {
        let approvals = granted("mix test").reset();

        assert!(matches!(
            approvals.request(id(), "mix test"),
            Request::Auto(_)
        ));
    }
}
