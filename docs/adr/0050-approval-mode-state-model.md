# The approval-mode state model: pure on Approvals, mirrored to the Screen

qwen-code lets the operator cycle an APPROVAL MODE with Shift+Tab (win32: Tab),
rotating `plan → default → auto-edit → auto → yolo → (wrap)`, and shows the
current non-default mode above the composer as the `AutoAcceptIndicator`.
suspenders carries the same cycle and footer indicator.

suspenders' Approval state lives as a pure fold (`approvals::Approvals`,
ADR-0005) hosted by the Agent actor. The questions: where does the mode live, how
does the UI see it, and which modes actually change behavior.

## Decision

**The mode lives on `Approvals` (pure), authoritative on the Agent.** `ApprovalMode
{ Plan, Default(default), AutoEdit, Auto, Yolo }` - the variant ORDER is qwen's
`APPROVAL_MODES` array order, so `ApprovalMode::cycle` is the same `(i + 1) % len`
fold the CLI uses, with a hard wrap. `Approvals` gains a `mode` field and a
`cycle_mode(self) -> (Self, ApprovalMode)`. The gate consults it:
`Approvals::request` answers `Auto` (no modal) when the mode auto-approves all, or
when a Standing Approval covers the command; otherwise it becomes pending.

**The Agent owns the transition + broadcast.** A new `Command::CycleApprovalMode`
folds `cycle_mode` and broadcasts `Event::ApprovalModeChanged { mode }`. The Screen
holds a DISPLAY-ONLY mirror (`Screen::approval_mode`) - it never decides the mode,
only reflects it. The mirror has ONE writer, `Screen::mirror_approval_mode`, fed
from two moments: the `ApprovalModeChanged` event fold (the only path a
model/Agent-driven change reaches the Screen), and the adapter's set from an
awaited authoritative fold result (the cycle and `/plan` enter/exit) - required
because the broadcast channel is lossy (a `Lagged` could leave the footer
indicator permanently stale). Both write the Agent's fold result, never a
Screen-side decision. Shift+Tab flows ui.rs `BackTab`/`Tab+SHIFT` →
`Key::CycleApprovalMode` → `AgentCommand::CycleApprovalMode` → the Agent. The
cycle is Session-scoped: it works whether or not a Run is in flight, and while an
Approval is open the key is swallowed by the block (the pending Approval is left
untouched - no double-meaning).

**Default, Yolo, and Plan change BEHAVIOR; AutoEdit/Auto are DISPLAY-COMPLETE
but behavior-STUBBED.** Every Tool Call passes through the single mode-aware
`Approvals::classify(name, kind, input) -> Verdict` fold (ADR-0067), so the mode
is consulted for every call, not just the gated ones.
- `Default` gates run_shell_command and web_fetch as usual.
- `Yolo` auto-approves EVERY gated Call - `request` returns `Auto` with no modal
  and no Standing entry (dropping out of Yolo re-gates the command).
- `Plan` is read-only (ADR-0067): `enter_plan_mode` / `exit_plan_mode` tools,
  a per-Tool-Call read-only verdict on the Tool's `Kind`, the plan-mode shell
  classifier, and a per-Pass reminder.
- `AutoEdit`, `Auto` gate EXACTLY like `Default`. suspenders does not gate edits
  (so `AutoEdit` is vacuous) and has no Auto classifier. The footer still NAMES
  them so the cycle is whole and forward-compatible, but they carry no behavior.
  This is documented, not hidden.

**A PermissionRequest hook decides ahead of the mode.** A Hook (ADR-0066) firing
at the PermissionRequest event may return a permission decision, and it is
consulted before the gate consults the mode. An `allow` auto-approves the Call:
`Approvals::request` short-circuits to `Auto` with no modal, scoped to this Call
rather than a Standing entry. A `deny` rejects the Call outright, returning the
hook's stated reason to the model in place of a Tool Result, and the Approval
gate never opens. An `ask` (and any hook that returns no decision) falls through
to the normal gate, so the mode and any Standing Approval decide as described
above. Because the hook is consulted first, a `deny` overrides even Yolo, which
is deliberate: a PermissionRequest hook is an operator-installed guard.

**The `AutoAcceptIndicator` is a footer status segment.** `StatusSegment::ApprovalMode(mode)`
(the mode label, coloured per mode) + `StatusSegment::ApprovalModeHint` (the
secondary ` (shift + tab to cycle)` phrase - a separate segment because it is a
distinct colour). Both are assembled only when the mode is not `Default` (qwen
renders nothing for Default), placed right after the mode block in the left group,
and NOT subject to the fit/drop policy (an unusual approval mode is a safety
signal the operator must keep seeing). Labels are qwen-verbatim: `plan mode`(green)
/ `auto-accept edits`(yellow) / `auto mode (classifier-evaluated)`(yellow) /
`YOLO mode`(red).

## Consequences

- The two always-variants of qwen's exec/mcp confirmations collapse onto
  suspenders' single session-scoped `ApproveAlways` (ADR-0005): no cross-session
  persistence, no per-user scope. Deliberate.
- **Risk:** the stubbed AutoEdit/Auto are display-only - the footer names a mode
  that changes nothing. Accepted and documented until their own behavior lands;
  Plan and Yolo carry real behavior, so the safety-relevant labels are honest.
- The `expire(now)` timeout hook on `SelectionList` (ADR-0049) is host-driven and
  never fires for the 3-row approval.

## Revision (P5, ADR-0062): memory-dir writes/edits auto-approved

Managed auto-memory (ADR-0062) writes and edits into the trusted memory subtree must not prompt (qwen flips its default permission to 'allow' for a memory write). In Suspenders this is INHERENT, not a new branch: `write_file`/`edit_file` are already ungated here (the gate policy covers only code-execution `run_shell_command` and outbound `web_fetch`), so a write into the memory dir - like any write - carries no gate at all, while `run_shell_command` still gates. The auto-approval of memory writes is therefore a property of the ungated write path; a test in `approvals.rs` pins that a memory-dir write does not gate while code-execution still does.
