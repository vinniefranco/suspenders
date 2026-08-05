# Plan Mode: one mode-aware verdict fold over every Tool Call

ADR-0050 gave suspenders the Shift+Tab approval-mode cycle and a
display-complete-but-behavior-stubbed `ApprovalMode::Plan`: the footer says
`plan mode`, but the mode gates exactly like `Default` - no read-only
enforcement, no plan loop, no enter/exit. That ADR recorded wiring the real
behavior as future work "it needs a classifier + a plan-loop suspenders does not
have." This ADR is that work: a faithful port of qwen v0.21.4's plan-mode
tooling (`enter_plan_mode`, `exit_plan_mode`, the read-only enforcement, the
plan-mode shell classifier, the two system reminders), and the refactor that
makes it clean rather than bolted-on.

Source of truth: qwen-code v0.21.4 (`/home/vinnie/Sandbox/qwen-code-v0.21.4`).
Where qwen's plan machinery is team- or ACP-specific (team leader plan approval,
headless/ACP entry paths), suspenders omits it - that architecture is absent
here, the same scope call ADR-0061 made for subagents. The subagent block IS
ported: suspenders has subagents.

## The mismatch that forced a refactor

qwen keys read-only enforcement off "would this tool require a confirmation?" -
in `PLAN` the scheduler blocks any call that is not read-only (its
`getDefaultPermission` is not a bare `allow`), routing exit/enter/ask_user_question
and `info`-typed confirmations around the block. That works because in qwen the
approval mode and the permission evaluation live in the same place (the
scheduler over `config.getApprovalMode()`), and EVERY tool call is evaluated
against the mode.

suspenders' gate was narrower. `approvals::gate_text` gates exactly two tools
(`run_shell_command`, `web_fetch`); every other tool - including `edit_file` and
`write_file` - short-circuits in `gated_execute` straight to execution and never
consults the mode at all. That was fine while only `Yolo`/`Default` changed
behavior. Plan mode breaks it: plan mode must block edits, and edits do not
gate. Bolting a second, plan-only enforcement path beside the approval gate
would be the parallel mechanic this project avoids.

## Decision

### One pure verdict fold over every Tool Call

Replace the gated-tools-only `gate_text` + `Approvals::request` pair with a
single pure classifier on `Approvals`:

```
Approvals::classify(name, kind, input) -> Verdict { Allow, Ask(text), Block(reason) }
```

Every tool call folds through it - not just the two that gate today. The fold is
the one place the mode decides a call's fate, absorbing what were separate facts:

- `Yolo` -> `Allow` for gated calls (as today).
- a covering Standing Approval -> `Allow` (as today).
- a gated call with no cover -> `Ask(text)` (as today, the modal).
- **`Plan` + a mutating `Kind` -> `Block(reason)`** with qwen's verbatim
  "not a read-only tool" message and no modal.
- **`Plan` + `run_shell_command` -> the plan-mode shell classifier** (below),
  which may `Allow` a read-only command or `Block` a mutating one.
- read-only Kinds (`Read`/`Search`/`Fetch`/`Think`) -> `Allow` in `Plan`, even
  `web_fetch` (qwen lists it among the plan-allowed tools).

The Run loop calls `classify` for every call in `run_block`: `Allow` executes,
`Block` returns an error Answer with no modal (qwen's behavior), `Ask`
round-trips `request_approval` to open the modal. The `gated_execute`
short-circuit is deleted; the mode is now the single axis every call is
evaluated against, matching qwen's single permission evaluation.

### `Kind` on the Tool trait

The fold needs to know whether a tool mutates. Port qwen's `Kind` as a method
on the `Tool` trait's spec (`Read`, `Edit`, `Search`, `Execute`, `Think`,
`Fetch`, `Agent`, `Other` - qwen's `Delete`/`Move` are not carried: no
Suspenders tool deletes or moves files, and an undeclared tool's `Other`
default already blocks in plan mode). The mutating set (`Edit`, `Execute` -
qwen's `MUTATOR_KINDS` minus the uncarried pair) is what `Plan` blocks. This is
a per-tool self-description, so the enforcement fact lives with the tool, not in
a hardcoded set the fold reaches around - the choice ADR-0050's revision hinted
at and this ADR makes real. `run_shell_command` is `Execute` but is special-cased
to the shell classifier rather than a blanket block; `todo_write` and
`save_memory` are `Think` (they write, but to the model's own record / the
trusted memory subtree) and stay allowed in plan mode, as in qwen.

### The live mode view: a shared atomic mirror

The mode is Agent-owned state on the `Approvals` fold, and it changes mid-Run
(a `Shift+Tab` cycle, an `enter_plan_mode` call, an `exit_plan_mode` approval),
while the Run executes in a separate task that could not see it. Rather than a
per-call round-trip to the Agent for a pure verdict, the Agent writes every mode
change into a shared `Arc<AtomicApprovalMode>` mirror, and the loop reads it in
`classify` and `shape_request`. Genuinely live - the three change paths update
one atomic uniformly, no refresh logic, no staleness within a batch. The Agent
stays the single authority (it owns the `Approvals` fold and the mutation); the
atomic is a read-only mirror for the loop, not a second source of truth.

### The two tools reach Agent state through a new capability

`enter_plan_mode {userRequested?}` and `exit_plan_mode {plan, originalRequest?,
researchSummary?}` are faithful ports. They mutate Agent-owned state (the mode)
and raise a user modal, so they reach the host through the Capability Context
(ADR-0055), like the Approver and the Questioner - a new tx-backed `PlanMode`
capability whose real impl relays over the Agent mpsc:

- `enter_plan_mode`: `allow` permission (a privilege reduction needs no
  confirmation), a YOLO guard (a model-initiated entry - `userRequested` falsey -
  in Yolo is a no-op returning guidance, only an explicit user request enters
  plan from Yolo), idempotent (only flips when not already `Plan`, so `prePlanMode`
  is never overwritten), and it reveals the deferred `exit_plan_mode` tool through
  the concrete registry on the caps (the same reveal `tool_search` does), so the
  model can call it directly. It returns the plan-mode reminder as its result.
- `exit_plan_mode`: always-visible (deferred + `alwaysLoad` so its schema is
  always declared once plan mode tells the model to call it). Empty `plan`
  rejected. Outside plan mode it returns a guidance error, not a deny. In plan
  mode it raises the plan-confirmation dialog and, on approval, the Agent flips
  the mode to the outcome's target atomically.

The subagent block is ported: a child Run's `PlanMode` capability is degraded
(like the recursion guard for subagents, ADR-0061), so `enter_plan_mode` /
`exit_plan_mode` inside a subagent return qwen's block result rather than
mutating the parent's mode.

### The plan-confirmation dialog is a `PendingPlan` modal

qwen's plan confirmation is a three-outcome radio over the plan text. It maps
onto the `SelectionList` standalone-box pattern ADR-0057 established for
`ask_user_question` - a `PendingPlan` modal parallel to `PendingQuestion`, drawn
bottom-most in the pending body, with qwen's verbatim rows:

- **"Yes, restore previous mode ({mode})"** -> `prePlanMode`
- **"Yes, and auto-accept edits"** -> `AutoEdit`
- **"Yes, and manually approve edits"** -> `Default`
- **"No, keep planning (esc)"** -> stay in `Plan` (Escape declines, as a Question
  Escape declines)

The three-outcome shape is the deliberate divergence from the two-way Approval
radio (approve / deny / approve-always): a plan exit picks a TARGET mode, not a
yes/no. On a proceed outcome the Agent saves the plan to disk (`session_dir`),
flips the mode atomically, and the tool returns qwen's verbatim "User approved.
You can now start coding." A stale-revision guard (qwen's `approvalModeRevision`)
covers the mode changing between raising the dialog and resolving it; in
suspenders' single-owner Agent this is one revision counter, far simpler than
qwen's concurrent-exit machinery.

### The per-Pass reminder is ephemeral request-shaping Voice

While in `Plan`, qwen re-injects `getPlanModeSystemReminder` into every request
(`client.ts`), not just once on entry - a small model drifts without the
standing read-only reminder. suspenders does the same: a Voice string appended to
`req.system` in `shape_request` when the live mode is `Plan`, ephemeral (it rides
each request but never enters the Conversation or the Session Log). When the user
leaves plan mode outside the approved exit flow (a `Shift+Tab` cycle), the
`getManualPlanExitSystemReminder` one-shot is injected on the next request,
superseding the standing reminder.

This is a real amendment to ADR-0045's "the loop injects nothing to steer" wager,
and the honest statement is: that wager holds for normal operation, and Plan mode
is an explicit, user-chosen read-only mode that carries a standing per-Pass
reminder. It is not the retired reactive nudge apparatus (Governor / Anchor /
Endgame) - those injected corrective text INTO the Conversation in response to
model drift; the plan reminder is a mode invariant injected at request-shaping,
ephemeral, and gated on a mode the user deliberately entered. qwen does it, and
it is correct; ADR-0045 and CONTEXT.md are revised to state this rather than
treated as a constraint against it.

### The plan-mode shell classifier

Ported faithfully from qwen's `plan-mode-shell-policy.ts`: in plan mode a
`run_shell_command` call is parsed and classified as read-only (`ls`, `cat`,
`git status`, ...) - which is `Allow`ed - or mutating - which is `Block`ed. This
keeps the read-only investigation the model needs during planning (inspecting the
tree with shell) available while still blocking state changes.

## Consequences

- The `gate_text` + `request` pair and the `gated_execute` ungated short-circuit
  are gone, replaced by one `classify` fold every call passes through. The
  narrower "two tools gate" model is retired; the mode is now consulted for every
  tool call, which is both more faithful to qwen and the only shape in which plan
  mode is not a bolt-on.
- `ApprovalMode::Plan` is no longer a display stub. ADR-0050's stubbed-modes risk
  note is discharged for Plan (AutoEdit/Auto remain as ADR-0050 describes until
  their own behavior lands).
- `Kind` is a new fact every tool must state. The initial assignment matches
  qwen's; a new tool declares its Kind or defaults to `Other` (never a mutator by
  default, so a mis-declared tool fails safe - blocked in plan mode rather than
  slipping an edit through).
- The shared `Arc<AtomicApprovalMode>` is a second reader of the mode beside the
  `Approvals` fold. It is a strict mirror (write-only from the Agent, read-only
  from the loop); the fold stays authoritative. A test pins that every mutation
  path (cycle / enter / exit) updates the mirror.
- Omitted by scope (architecturally absent, recorded not hidden): team leader
  plan approval (`team-plan-approval.ts`), and the ACP/headless plan entry paths.
  If suspenders grows a team or ACP surface, they attach here.
