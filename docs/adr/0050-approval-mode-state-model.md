# The approval-mode state model: pure on Approvals, mirrored to the Screen

qwen-code lets the operator cycle an APPROVAL MODE with Shift+Tab (win32: Tab),
rotating `plan → default → auto-edit → auto → yolo → (wrap)`, and shows the
current non-default mode above the composer as the `AutoAcceptIndicator`. Phase 4
of the qwen UI port adds this cycle + the footer indicator to suspenders.

suspenders' Approval state already lives as a pure fold (`approvals::Approvals`,
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
holds a DISPLAY-ONLY mirror (`Screen::approval_mode`) fed by that event - it never
decides the mode, only reflects it. Shift+Tab flows ui.rs `BackTab`/`Tab+SHIFT` →
`Key::CycleApprovalMode` → `AgentCommand::CycleApprovalMode` → the Agent. The
cycle is Session-scoped: it works whether or not a Run is in flight, and while an
Approval is open the key is swallowed by the block (the pending Approval is left
untouched - no double-meaning).

**Only Default and Yolo change BEHAVIOR; Plan/AutoEdit/Auto are DISPLAY-COMPLETE
but behavior-STUBBED.**
- `Default` gates exactly as before.
- `Yolo` auto-approves EVERY gated Call - `request` returns `Auto` with no modal
  and no Standing entry (dropping out of Yolo re-gates the command).
- `Plan`, `AutoEdit`, `Auto` gate EXACTLY like `Default` this phase. suspenders has
  no plan-loop and no classifier, and does not gate edits (so `AutoEdit` is
  vacuous). The footer still NAMES them so the cycle is whole and forward-
  compatible, but they carry no behavior yet. This is documented, not hidden.

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
- **Risk (recorded):** the stubbed modes are MISLEADING - the footer can say `plan
  mode` while files are still modified, because only Yolo/Default alter behavior. A
  startup notice is deferred; this ADR is the record. Wiring real Plan/AutoEdit/Auto
  behavior is future work (it needs a classifier + a plan-loop suspenders does not
  have).
- The `expire(now)` timeout hook on `SelectionList` (ADR-0049) is host-driven and
  never fires for the 3-row approval; the tick wiring lands with Phase 5's longer
  dialogs.
