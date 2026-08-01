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
/// each, the field whose value seeds what the user reads in the modal (and a
/// Standing Approval matches by exact string equality - ADR-0005). run_command
/// shows the command (arbitrary code); web_fetch reads the URL but shows (and
/// matches on) its DOMAIN (the one Tool that reaches outside the Project Root -
/// ADR-0024, revised: domain-scoped, faithful to qwen's `WebFetch(<hostname>)`).
///
/// One row per gated Tool ties "this gates" to "this is the field" so the two
/// facts can never disagree. A gated Tool with the field missing or non-string
/// still gates, reading the empty string - the gate is about the Tool, not the
/// input's shape.
const GATED: &[(&str, &str)] = &[("run_shell_command", "command"), ("web_fetch", "url")];

/// The single Approval-gate query over a Tool Call (name + input): `Some(text)`
/// means the Call gates and `text` is exactly what the user reads (and what a
/// Standing Approval matches); `None` means no gate. Because the same lookup
/// answers both facts, "does this gate" and "what text" can never disagree.
///
/// The text is read from the extension-adjusted input the caller hands over.
/// For most gated Tools the field value IS the gate text; web_fetch is the one
/// exception - its gate text is the URL's DOMAIN (ADR-0024, revised), so a
/// Standing Approval covers the whole host and a second fetch to the same host
/// auto-approves.
pub fn gate_text(name: &str, input: &Value) -> Option<String> {
    let (_, field) = GATED.iter().find(|(tool, _)| *tool == name)?;
    let raw = input.get(field).and_then(Value::as_str).unwrap_or("");
    if name == "web_fetch" {
        Some(web_fetch_domain(raw))
    } else {
        Some(raw.to_string())
    }
}

/// The DOMAIN a web_fetch Approval scopes to (qwen web-fetch.ts
/// `getConfirmationDetails`): the URL's hostname, or - if the URL will not parse
/// or carries no host - the raw string itself (qwen's `catch { domain = url }`).
/// Domain-scoped is the deliberate widening ADR-0024 (revised) chose for
/// qwen-fidelity: a Standing Approval on `docs.rs` covers every path under it,
/// so repeated doc lookups do not re-prompt per URL.
fn web_fetch_domain(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
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
}

/// The user's decision on the pending Approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    ApproveAlways,
}

/// The Approval mode - the Session-scoped policy the Shift+Tab cycle rotates
/// through (ADR-0050, qwen `ApprovalMode`/`APPROVAL_MODES`). The order of the
/// variants IS qwen's `APPROVAL_MODES` array order, so [`ApprovalMode::cycle`]
/// is the same `(i + 1) % len` fold the CLI uses
/// (`AgentComposer.tsx:113-116`): plan → default → auto-edit → auto → yolo →
/// (wrap to plan).
///
/// Only two modes change BEHAVIOR in suspenders today (ADR-0050): `Default`
/// gates exactly as before, and `Yolo` auto-approves every gated Call.
/// `Plan`/`AutoEdit`/`Auto` are DISPLAY-COMPLETE but behavior-STUBBED - they
/// gate exactly like `Default` - because suspenders has no plan-loop, no
/// classifier, and does not gate edits (so `AutoEdit` is vacuous). The footer
/// still names them so the cycle is whole; the stub is documented, not hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    Plan,
    #[default]
    Default,
    AutoEdit,
    Auto,
    Yolo,
}

impl ApprovalMode {
    /// The next mode in the Shift+Tab cycle (qwen `(i + 1) % len` over
    /// `APPROVAL_MODES`): plan → default → auto-edit → auto → yolo → plan. A
    /// total function with a hard wrap, so the cycle can never leave the set.
    pub fn cycle(self) -> ApprovalMode {
        match self {
            ApprovalMode::Plan => ApprovalMode::Default,
            ApprovalMode::Default => ApprovalMode::AutoEdit,
            ApprovalMode::AutoEdit => ApprovalMode::Auto,
            ApprovalMode::Auto => ApprovalMode::Yolo,
            ApprovalMode::Yolo => ApprovalMode::Plan,
        }
    }

    /// Whether this mode auto-approves EVERY gated Call without ever showing an
    /// Approval (ADR-0050). Only `Yolo` does; every other mode - including the
    /// display-stubbed `Plan`/`AutoEdit`/`Auto` - defers to Standing Approvals
    /// and the pending gate, exactly like `Default`.
    fn auto_approves_all(self) -> bool {
        matches!(self, ApprovalMode::Yolo)
    }
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

/// The Approval state: the one pending Approval (or none), the Session's
/// Standing Approvals (string-equality set), and the current Approval mode
/// (ADR-0050) the Shift+Tab cycle rotates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    pub pending: Option<Pending>,
    pub standing: HashSet<String>,
    pub mode: ApprovalMode,
}

impl Approvals {
    pub fn new() -> Self {
        Approvals::default()
    }

    /// Folds in an Approval request from the Run. `Yolo` mode (ADR-0050)
    /// auto-approves EVERY gated Call, so the request answers `Auto` without a
    /// modal; otherwise a covering Standing Approval answers `Auto`, and a bare
    /// request becomes the pending Approval. `Plan`/`AutoEdit`/`Auto` gate
    /// exactly like `Default` this phase (display-complete, behavior-stubbed).
    pub fn request(mut self, id: ApprovalId, command: impl Into<String>) -> Request {
        let command = command.into();
        if self.mode.auto_approves_all() || self.standing.contains(&command) {
            Request::Auto(self)
        } else {
            self.pending = Some(Pending { id, command });
            Request::Pending(self)
        }
    }

    /// Rotates the Approval mode one step in the Shift+Tab cycle (ADR-0050),
    /// returning the new state and the mode it landed on so the host can
    /// broadcast it. Pure: it touches only `mode`, never the pending Approval
    /// or the Standing set, so cycling while an Approval is open leaves that
    /// Approval untouched.
    pub fn cycle_mode(mut self) -> (Self, ApprovalMode) {
        self.mode = self.mode.cycle();
        let mode = self.mode;
        (self, mode)
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
#[path = "../tests/approvals.rs"]
mod tests;
