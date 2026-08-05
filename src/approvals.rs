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
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

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

/// The kind of effect a Tool has (qwen `Kind`, tools.ts): a per-Tool
/// self-description the plan-mode fold (ADR-0067) reads to decide whether a Call
/// mutates. The [`Tool`](crate::tool::Tool) trait exposes it via `kind()`
/// (default [`Kind::Other`]), and `Approvals::classify` folds it against the
/// mode. It lives here in the Approval leaf (not `tool`) because the fold that
/// consumes it is the sole reader, and both `tool` and each tool depend one-way
/// on this leaf - no `tool <-> approvals` cycle.
///
/// The mutating set is `Edit`/`Execute`; the read-only set that Plan mode
/// allows is `Read`/`Search`/`Fetch`/`Think` (see [`Kind::is_plan_allowed`]).
/// `Other` and `Agent` are neither - they are BLOCKED in plan mode (fail-safe:
/// an undeclared tool defaults to `Other`, so a mis-declared tool is blocked
/// rather than slipping a mutation through).
///
/// qwen's `Kind` additionally carries `Delete` and `Move` in its
/// `MUTATOR_KINDS`; Suspenders has no tool that deletes or moves files (every
/// filesystem mutation is an `Edit`-kind rewrite or an `Execute`-kind shell
/// command), so the two variants are not carried - a future deleting/moving
/// tool re-adds its variant with itself, and until then the `Other` default
/// keeps any such tool blocked in plan mode anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Edit,
    Search,
    Execute,
    Think,
    Fetch,
    Agent,
    Other,
}

impl Kind {
    /// Whether Plan mode allows a Call of this Kind (ADR-0067). The read-only
    /// Kinds - `Read`, `Search`, `Fetch`, `Think` - are safe to run while
    /// planning (they gather information or write only to the model's own record
    /// / trusted subtrees, as qwen's `todo_write`/`save_memory` do). Every other
    /// Kind (the mutators `Edit`/`Execute`, plus `Agent` and the fail-safe
    /// `Other`) is blocked. This mirrors qwen's read-only carve-out:
    /// its scheduler blocks any call in `PLAN` whose confirmation is not `info`,
    /// and the tools whose confirmation is `info` / who need no confirmation at
    /// all are exactly the read-only Kinds here.
    fn is_plan_allowed(self) -> bool {
        matches!(self, Kind::Read | Kind::Search | Kind::Fetch | Kind::Think)
    }
}

/// The verdict of folding a Tool Call through the mode-aware classifier
/// (ADR-0067): the one place the Approval mode decides a Call's fate. It absorbs
/// what were the separate `gate_text` + `request` facts plus plan-mode
/// enforcement, so every Call folds through one method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the Call with no modal: `Yolo` on a gated Call, a covering Standing
    /// Approval, or any non-gated Call outside plan mode.
    Allow,
    /// Open the Approval modal over `text` (the exact string the user reads and
    /// a Standing Approval matches - ADR-0005): a gated Call with no cover,
    /// outside plan mode, or a gated read-only Call (`web_fetch`) IN plan mode
    /// (qwen keeps its confirmation).
    Ask(String),
    /// Block the Call with no modal (ADR-0067): a mutating / non-read-only Kind
    /// in plan mode. `reason` is qwen's verbatim "not a read-only tool" message,
    /// fed to the model as an error result.
    Block(String),
}

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
///
/// Kept as a helper `classify` calls (rather than a separate gate seam): it
/// still owns the web_fetch domain-scoping logic, but the mode-aware decision
/// now lives in one place ([`Approvals::classify`]).
pub fn gate_text(name: &str, input: &Value) -> Option<String> {
    let (_, field) = GATED.iter().find(|(tool, _)| *tool == name)?;
    let raw = input.get(field).and_then(Value::as_str).unwrap_or("");
    if name == "web_fetch" {
        Some(web_fetch_domain(raw))
    } else {
        Some(raw.to_string())
    }
}

/// The VERBATIM qwen plan-mode block message (coreToolScheduler.ts, the
/// `isPlanRequiredTeammate` branch, which is the ordinary interactive posture:
/// not SDK mode, not a subagent). Fed to the model as the error result when a
/// mutating / non-read-only Kind is called in Plan mode. Only the tool name is
/// interpolated; the rest is qwen verbatim so a small model reads the same
/// guidance qwen gives it.
fn plan_mode_block_message(name: &str) -> String {
    format!(
        "Tool blocked by plan mode: \"{name}\" is not a read-only tool. \
Only read-only tools (read_file, grep_search, glob, list_directory, \
web_fetch, etc.) are allowed in plan mode. Do NOT retry this tool. \
Pivot to read-only alternatives to gather the information you need, then \
call exit_plan_mode with a plan that covers this tool's purpose."
    )
}

/// The Plan-mode verdict for a mutating / non-read-only Call (the
/// `mode == Plan && !is_plan_allowed` arm of [`Approvals::classify`], ADR-0067).
/// `run_shell_command` runs the plan-mode shell classifier; every other tool is
/// blocked wholesale with qwen's verbatim [`plan_mode_block_message`].
fn plan_verdict(name: &str, input: &Value) -> Verdict {
    if name == crate::tools::run_command::SHELL {
        plan_shell_verdict(input)
    } else {
        Verdict::Block(plan_mode_block_message(name))
    }
}

/// The plan-mode shell-classifier verdict (ADR-0067, Phase 4b): parse the
/// `run_shell_command` command with the tree-sitter safety classifier
/// ([`crate::shell_safety`], a faithful port of qwen's `shellAstParser.ts`) and
/// map the three-valued result to a [`Verdict`], mirroring qwen's decision in
/// `coreToolScheduler.ts` + `plan-mode-shell-policy.ts`:
///
/// - [`ReadOnly`](crate::shell_safety::ShellCommandSafety::ReadOnly) ->
///   [`Verdict::Allow`]. qwen auto-approves a read-only shell command in plan
///   mode: `planShellRequiresConfirmation` is false and the shell tool's
///   read-only permission is `allow`, so the scheduler takes the auto-approve
///   branch (`ProceedAlways`, coreToolScheduler.ts:2610-2637). This is the
///   read-only investigation ADR-0067 keeps available during planning.
/// - [`Write`](crate::shell_safety::ShellCommandSafety::Write) ->
///   [`Verdict::Block`] with the verbatim [`WRITE_BLOCK_MESSAGE`]. qwen:
///   `rejectPlanShell(writeBlockMessage)` (coreToolScheduler.ts:2606-2608).
/// - [`Unknown`](crate::shell_safety::ShellCommandSafety::Unknown) ->
///   [`Verdict::Ask`] over the raw command. qwen sets
///   `planShellRequiresConfirmation = true` and SURFACES a confirmation decorated
///   with `hideAlwaysAllow` + the UNKNOWN_WARNING
///   (`decoratePlanModeShellConfirmation`, plan-mode-shell-policy.ts:247-257);
///   only when NO surface is available does it `rejectPlanShell(noApprovalMessage)`
///   (coreToolScheduler.ts:2810). suspenders' [`Verdict::Ask`] IS that surface (the
///   approval modal), so `unknown` asks. The [`Verdict::Ask`] text is the raw
///   command, matching the gate text a normal `run_shell_command` shows.
fn plan_shell_verdict(input: &Value) -> Verdict {
    use crate::shell_safety::ShellCommandSafety;
    match crate::shell_safety::classify_shell_input(input) {
        ShellCommandSafety::ReadOnly => Verdict::Allow,
        ShellCommandSafety::Write => Verdict::Block(WRITE_BLOCK_MESSAGE.to_string()),
        // gate_text always yields Some for run_shell_command (a GATED tool), so the
        // unwrap_or is defensive only.
        ShellCommandSafety::Unknown => Verdict::Ask(
            input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    }
}

/// The VERBATIM qwen `WRITE_BLOCK_MESSAGE` (plan-mode-shell-policy.ts:25): the
/// error result when a `run_shell_command` classifies as state-modifying in Plan
/// mode. qwen feeds it to the model via `rejectPlanShell(writeBlockMessage)`
/// (coreToolScheduler.ts:2607). Byte-verbatim so a small model reads qwen's exact
/// guidance ("continue read-only investigation and include the action in the
/// plan").
const WRITE_BLOCK_MESSAGE: &str = "Plan mode blocked this shell command because it was classified as state-modifying. Do not retry it through wrappers or obfuscation; continue read-only investigation and include the action in the plan.";

// qwen's `NO_APPROVAL_MESSAGE` (plan-mode-shell-policy.ts:27) - "Plan mode could
// not determine whether this shell command is read-only, and no approval surface
// is available. The command was not run; Plan mode remains active." - has NO
// consumer in suspenders: it is qwen's `rejectPlanShell(noApprovalMessage)` for
// the case where an `unknown`-classified shell Call cannot open a confirmation
// (an SDK/headless surface, coreToolScheduler.ts:2810). suspenders' loop ALWAYS
// has an approval surface (the modal), so an `unknown` shell Call maps to
// [`Verdict::Ask`], never to this no-surface reject. It is recorded here (not as a
// dead const) so the port's coverage of qwen's messages is explicit.
//
// qwen's `UNKNOWN_WARNING` (plan-mode-shell-policy.ts:23) is likewise a modal
// DECORATION (an extra warning line + `hideAlwaysAllow`), not a distinct verdict;
// suspenders' [`Verdict::Ask`] carries no per-verdict warning slot, so the warning
// text is not surfaced separately. The approve-once semantics it guards (no
// "always allow") are inherent to a one-shot `Ask` here.

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

/// The user's decision on a pending plan-exit dialog (ADR-0067, qwen's
/// `ToolConfirmationOutcome` mapped to a target mode in exitPlanMode.ts). A plan
/// exit is a THREE-outcome radio picking a TARGET mode, not the two-way
/// approve/deny of an [`Approval`](Decision): the four variants map to qwen's
/// four `onConfirm` cases, and [`PlanDecision::target`] resolves each to the
/// mode plan flips to (or `None` to stay in plan). This is the deliberate
/// divergence from the Approval radio (ADR-0067).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDecision {
    /// "Yes, restore previous mode ({mode})" -> the saved prePlanMode
    /// (exitPlanMode.ts `RestorePrevious` -> `snapshot.prePlanMode`).
    RestorePrevious,
    /// "Yes, and manually approve edits" -> `Default` (exitPlanMode.ts
    /// `ProceedOnce` -> `ApprovalMode.DEFAULT`).
    ProceedOnce,
    /// "Yes, and auto-accept edits" -> `AutoEdit` (exitPlanMode.ts
    /// `ProceedAlways` -> `ApprovalMode.AUTO_EDIT`).
    ProceedAlways,
    /// "No, keep planning (esc)" -> stay in `Plan` (exitPlanMode.ts `Cancel` ->
    /// no approval). Escape declines, as an Approval Escape denies.
    Cancel,
}

impl PlanDecision {
    /// The mode a proceed outcome flips to, given the mode Plan was entered from
    /// (`pre_plan_mode`). `None` is the [`PlanDecision::Cancel`] "keep planning"
    /// outcome that stays in Plan. Mirrors exitPlanMode.ts `onConfirm`'s
    /// `targetMode` mapping (RestorePrevious -> prePlanMode, ProceedOnce ->
    /// DEFAULT, ProceedAlways -> AUTO_EDIT).
    pub fn target(self, pre_plan_mode: ApprovalMode) -> Option<ApprovalMode> {
        match self {
            PlanDecision::RestorePrevious => Some(pre_plan_mode),
            PlanDecision::ProceedOnce => Some(ApprovalMode::Default),
            PlanDecision::ProceedAlways => Some(ApprovalMode::AutoEdit),
            PlanDecision::Cancel => None,
        }
    }
}

/// The Approval mode - the Session-scoped policy the Shift+Tab cycle rotates
/// through (ADR-0050, qwen `ApprovalMode`/`APPROVAL_MODES`). The order of the
/// variants IS qwen's `APPROVAL_MODES` array order, so [`ApprovalMode::cycle`]
/// is the same `(i + 1) % len` fold the CLI uses
/// (`AgentComposer.tsx:113-116`): plan → default → auto-edit → auto → yolo →
/// (wrap to plan).
///
/// Three modes change BEHAVIOR (ADR-0050, ADR-0067): `Default` gates exactly as
/// before, `Yolo` auto-approves every gated Call, and `Plan` is now REAL - a
/// read-only planning mode whose `classify` fold `Block`s any mutating /
/// non-read-only Kind (ADR-0067). `AutoEdit`/`Auto` remain DISPLAY-COMPLETE but
/// behavior-STUBBED (they gate exactly like `Default`) until their own behavior
/// lands; the footer still names them so the cycle is whole. The stub is
/// documented, not hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    Plan,
    #[default]
    Default,
    AutoEdit,
    Auto,
    Yolo,
}

/// The Shift+Tab cycle order, qwen's `APPROVAL_MODES` array verbatim (ADR-0050).
/// [`ApprovalMode::cycle`] steps `(i + 1) % len` over it, so the wrap and the
/// order live in one place - adding a mode is one edit here, not a rewired match.
const CYCLE: [ApprovalMode; 5] = [
    ApprovalMode::Plan,
    ApprovalMode::Default,
    ApprovalMode::AutoEdit,
    ApprovalMode::Auto,
    ApprovalMode::Yolo,
];

impl ApprovalMode {
    /// The next mode in the Shift+Tab cycle (qwen `(i + 1) % len` over
    /// [`CYCLE`]): plan → default → auto-edit → auto → yolo → plan. A total
    /// function with a hard wrap, so the cycle can never leave the set.
    pub fn cycle(self) -> ApprovalMode {
        let i = CYCLE.iter().position(|&m| m == self).unwrap_or(0);
        CYCLE[(i + 1) % CYCLE.len()]
    }

    /// Whether this mode auto-approves EVERY gated Call without ever showing an
    /// Approval (ADR-0050). Only `Yolo` does; every other mode - including the
    /// display-stubbed `AutoEdit`/`Auto` - defers to Standing Approvals and the
    /// pending gate, exactly like `Default`. (`Plan` no longer reaches here for a
    /// mutating Kind - `classify` blocks it first - but a read-only gated Call in
    /// Plan still falls through to this, and Plan does not auto-approve.)
    fn auto_approves_all(self) -> bool {
        matches!(self, ApprovalMode::Yolo)
    }

    /// The mode's qwen wire string (approval-mode.ts `ApprovalMode` enum values:
    /// `plan`/`default`/`auto-edit`/`auto`/`yolo`). The plan-mode tools interpolate
    /// it VERBATIM into qwen's guidance strings (the outside-plan guidance names the
    /// `current mode: {mode}`, the plan-exit "restore previous mode ({mode})" row,
    /// the manual-exit reminder's `current approval mode is: {mode}`), so it must
    /// match qwen exactly.
    pub fn wire_str(self) -> &'static str {
        match self {
            ApprovalMode::Plan => "plan",
            ApprovalMode::Default => "default",
            ApprovalMode::AutoEdit => "auto-edit",
            ApprovalMode::Auto => "auto",
            ApprovalMode::Yolo => "yolo",
        }
    }

    /// The stable `u8` image of this mode for the [`AtomicApprovalMode`] mirror
    /// (ADR-0067): the mode's index in the [`CYCLE`] order, so the atomic stores
    /// one byte and [`ApprovalMode::from_u8`] round-trips it. Tied to `CYCLE` so
    /// adding a mode is still one edit there.
    fn as_u8(self) -> u8 {
        CYCLE.iter().position(|&m| m == self).unwrap_or(0) as u8
    }

    /// Recovers a mode from its [`as_u8`](ApprovalMode::as_u8) image, defaulting
    /// to `Default` for an out-of-range byte (the atomic is only ever written by
    /// `as_u8`, so this is defensive - a torn/foreign value fails safe to the
    /// gating `Default`, never a permissive mode).
    fn from_u8(v: u8) -> ApprovalMode {
        CYCLE
            .get(v as usize)
            .copied()
            .unwrap_or(ApprovalMode::Default)
    }
}

/// The shared live-mode mirror (ADR-0067): an [`AtomicU8`] over [`ApprovalMode`]
/// the Agent WRITES on every mode change (the `Shift+Tab` cycle, and in later
/// phases `enter_plan_mode`/`exit_plan_mode`) and the Run loop READS in
/// `classify` / `shape_request`. It is a strict mirror - write-only from the
/// Agent (the single authority: the Agent's `Approvals` fold owns the mode and
/// the mutation), read-only from the loop - not a second source of truth. The
/// Run executes in a separate task that cannot see the Agent's `Approvals`, so
/// this atomic gives it a genuinely live view without a per-call round-trip.
///
/// A child Run (subagent) gets a fresh `Default` mirror: subagents do not cycle
/// the mode, and plan-mode entry inside a subagent is a no-op (ADR-0067), so the
/// child never needs the parent's live mode.
#[derive(Debug)]
pub struct AtomicApprovalMode(AtomicU8);

impl Default for AtomicApprovalMode {
    fn default() -> Self {
        AtomicApprovalMode::new(ApprovalMode::default())
    }
}

impl AtomicApprovalMode {
    /// A mirror seeded to `mode`.
    pub fn new(mode: ApprovalMode) -> Self {
        AtomicApprovalMode(AtomicU8::new(mode.as_u8()))
    }

    /// Writes the new mode (Agent-side, on a mode change). `Relaxed` is enough:
    /// there is no other state ordered against this write - the loop reads the
    /// latest value, and a one-call staleness window (a cycle landing exactly as
    /// a call classifies) is benign (the very next call sees it).
    pub fn store(&self, mode: ApprovalMode) {
        self.0.store(mode.as_u8(), Ordering::Relaxed);
    }

    /// Reads the current mode (loop-side, in `classify` / `shape_request`).
    pub fn load(&self) -> ApprovalMode {
        ApprovalMode::from_u8(self.0.load(Ordering::Relaxed))
    }
}

/// The `AtomicU8` sentinel for "no pending manual-exit notice"
/// ([`PendingManualPlanExit`]): out of range of any [`ApprovalMode::as_u8`]
/// (0..=4), so it can never be a real mode. Distinct from `from_u8`'s
/// out-of-range Default fallback because the carrier tests for exactly this
/// value to mean "empty".
const NO_PENDING_NOTICE: u8 = u8::MAX;

/// The one-shot manual-plan-exit notice carrier (ADR-0067, qwen's
/// `pendingManualPlanExitNotice` + `takePendingManualPlanExitNotice`): the shared
/// cell the Agent WRITES with the new mode when the user leaves Plan mode OUTSIDE
/// the approved `exit_plan_mode` flow (a `Shift+Tab` cycle), and the Run loop
/// TAKES-and-clears in `shape_request` to inject
/// [`crate::voice::manual_plan_exit_reminder`] into that one request's system
/// text exactly once.
///
/// Mirrors [`AtomicApprovalMode`] as a strict Agent-writes / loop-reads shared
/// atomic - not a second source of truth, just the carrier the notice rides from
/// the Agent (which owns the mode change) to the Run task (which shapes the wire
/// request). One `AtomicU8`: an [`ApprovalMode::as_u8`] image (0..=4) when a
/// notice is pending, or [`NO_PENDING_NOTICE`] when none is. [`Self::take`] swaps
/// the sentinel back in, so the notice is delivered to exactly one request (the
/// take-and-clear is the one-shot semantics - qwen's `cursor.seenVersion`
/// advance, without the version machinery its resend-based retry needs).
///
/// suspenders' single-owner loop reaches `shape_request` exactly once per Pass
/// (a compaction retry recurses through `build_request` but only the terminal
/// fitting call reaches `shape_request`), so a plain take-and-clear there injects
/// the notice into exactly one request without qwen's per-send `version` de-dup.
///
/// A child Run (subagent) gets a fresh empty carrier: a subagent does not cycle
/// the mode, so it never has a manual exit to notice.
#[derive(Debug)]
pub struct PendingManualPlanExit(AtomicU8);

impl Default for PendingManualPlanExit {
    fn default() -> Self {
        PendingManualPlanExit::empty()
    }
}

impl PendingManualPlanExit {
    /// An empty carrier (no notice pending).
    pub fn empty() -> Self {
        PendingManualPlanExit(AtomicU8::new(NO_PENDING_NOTICE))
    }

    /// Queues the one-shot notice carrying the mode Plan was left FOR (Agent-side,
    /// on a manual exit). `Relaxed` for the same reason as the mode mirror: no
    /// other state is ordered against this write, and the loop reads the latest
    /// value at request-shaping. A second manual exit before the first was taken
    /// simply overwrites the pending mode with the newer one - the reminder always
    /// carries the current mode, so the most recent exit is the correct one.
    pub fn set(&self, mode: ApprovalMode) {
        self.0.store(mode.as_u8(), Ordering::Relaxed);
    }

    /// Takes-and-clears the pending notice (loop-side, in `shape_request`),
    /// returning the mode it carried, or `None` when none is pending. The swap is
    /// the one-shot: after a take the carrier is empty again, so the notice rides
    /// exactly one request.
    pub fn take(&self) -> Option<ApprovalMode> {
        let prev = self.0.swap(NO_PENDING_NOTICE, Ordering::Relaxed);
        (prev != NO_PENDING_NOTICE).then(|| ApprovalMode::from_u8(prev))
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

    /// A standing-free fold pinned to `mode` (ADR-0067): the LOOP-side view the
    /// batch gate classifies against. The loop reads the live mode from the
    /// shared [`AtomicApprovalMode`] mirror but does NOT hold the Session's
    /// Standing Approvals (those stay Agent-owned), so it classifies with an
    /// empty standing set. The consequence is deliberate and matches today's
    /// routing: a `Yolo`/`Plan` decision is mode-only, so the loop resolves it
    /// directly (`Allow`/`Block`); a Standing cover is unknown to the loop, so a
    /// gated Call still classifies as `Ask` and ROUND-TRIPS through
    /// `request_approval`, where the Agent's authoritative fold applies the real
    /// Standing set (and emits `approval_auto` on a cover) exactly as before.
    pub fn with_mode(mode: ApprovalMode) -> Self {
        Approvals {
            mode,
            ..Approvals::default()
        }
    }

    /// The one mode-aware verdict fold over a Tool Call (ADR-0067): the single
    /// place the Approval mode decides a Call's fate, folding the Call's `name`,
    /// its self-declared [`Kind`], and its `input` into a [`Verdict`]. Every Tool
    /// Call folds through this - not just the two that gate today - so the mode is
    /// the one axis every call is evaluated against (matching qwen's single
    /// permission evaluation over `config.getApprovalMode()`).
    ///
    /// The fold, in qwen's order:
    /// - `Yolo` auto-approves a GATED Call with no modal (`Allow`); a non-gated
    ///   Call in Yolo is moot (there is nothing to gate) and also `Allow`s.
    /// - In `Plan`, a mutating / non-read-only Kind is `Block`ed with qwen's
    ///   verbatim message and no modal. `run_shell_command` (`Kind::Execute`) is
    ///   a mutator, so it is `Block`ed this phase.
    ///   PHASE 4 (ADR-0067) inserts the plan-mode shell classifier HERE: a
    ///   read-only `run_shell_command` (`ls`, `cat`, `git status`, ...) will be
    ///   `Allow`ed and only a mutating one `Block`ed. Until then ALL shell is
    ///   blocked in plan mode. Do not build the classifier now.
    /// - A read-only Kind (`Read`/`Search`/`Fetch`/`Think`) in `Plan` folds
    ///   exactly like the non-plan path: a non-gated read-only Call `Allow`s, and
    ///   a GATED read-only Call (`web_fetch`, `Kind::Fetch`) still `Ask`s. This
    ///   matches qwen: web_fetch's confirmation is `type: 'info'`, and
    ///   `isPlanModeBlocked` returns false for an `info` confirmation, so plan
    ///   mode does NOT block web_fetch - it falls through to the normal
    ///   confirmation, still gated (web-fetch.ts `getConfirmationDetails` +
    ///   permissionFlow.ts `isPlanModeBlocked`).
    /// - Outside plan mode: a gated Call with a covering Standing Approval
    ///   `Allow`s; a gated Call with no cover `Ask`s (the modal); a non-gated
    ///   Call `Allow`s.
    pub fn classify(&self, name: &str, kind: Kind, input: &Value) -> Verdict {
        let gate = gate_text(name, input);

        // Plan mode blocks any mutating / non-read-only Kind (ADR-0067) BEFORE
        // the gate/Yolo path, so a Block cannot be turned into an Allow by Yolo
        // or a Standing Approval - it mirrors how qwen's plan-mode block precedes
        // the ApprovalMode overrides. A read-only Kind falls through to the same
        // classification the non-plan path uses (so `web_fetch` still gates).
        // `run_shell_command` (a `Kind::Execute` mutator) is the one exception:
        // it runs the plan-mode shell classifier (allow read-only, block write,
        // ask unknown) rather than a wholesale block (ADR-0067, qwen
        // plan-mode-shell-policy.ts). Every other non-read-only Kind is blocked.
        if self.mode == ApprovalMode::Plan && !kind.is_plan_allowed() {
            return plan_verdict(name, input);
        }

        // Not a gated Tool: no modal ever (edits/writes/reads/... run straight
        // through here when not blocked by plan mode above).
        let Some(text) = gate else {
            return Verdict::Allow;
        };

        // A gated Tool: Yolo auto-approves every gated Call, and a covering
        // Standing Approval auto-approves; otherwise open the modal over the
        // gate text. This is exactly the old `request` fold, now returning a
        // Verdict instead of mutating the pending slot (the caller drives the
        // modal round-trip).
        if self.mode.auto_approves_all() || self.standing.contains(&text) {
            Verdict::Allow
        } else {
            Verdict::Ask(text)
        }
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
