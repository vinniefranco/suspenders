# The inline approval attaches to the newest pending Tool Call

qwen-code confirms a gated Tool Call INLINE: `ToolConfirmationMessage` renders the
question + a radio list INSIDE the confirming tool's group box, and the box turns
`status.warning` while pending. suspenders matches: the Approval is an inline
block in the confirming tool's group box, not a centered modal.

The wire event carries only what the Run Loop mints: `ApprovalRequest {
approval_id, command }` - no `tool_use` id. So the render has to answer "which
tool box does this Approval belong to?" without a plumbed correlation id.

## Decision

**The inline approval attaches to the NEWEST pending Tool Call by POSITION.**
suspenders executes a Pass's Tool Call batch SEQUENTIALLY (`run::batch::execute_tools`
loops one call at a time), so at the moment the gate fires there is exactly ONE
live Tool Call - a `TranscriptItem::ToolCall` still awaiting its result (a
`ToolResult` supersedes the call in the store, so any surviving `ToolCall` item is
unresolved). The render finds it with `items.rposition(ToolCall)` - the same
newest-match identity the transcript already uses to pair results. No `tool_use`
id is added to the event, no new Dep is plumbed.

**The `ConfirmKind` derives from that call's name.** `run_shell_command` → `Exec` (the
command is arbitrary code, question `Allow execution of: '{command}'?`);
everything else, incl. `web_fetch`, → `Info` (the generic `Do you want to
proceed?`). Only those two tools gate today (`approvals::GATED`); a future gated
tool falls back to `Info` so a block is never empty. Edit/plan/mcp confirmation
shapes are future exhaustive arms.

**The three options are fixed and verbatim** (qwen exec/info set, the no-`{{action}}`
fallback): `Yes, allow once`(Approve) / `Always allow in this project`(ApproveAlways)
/ `No, suggest changes (esc)`(Deny). suspenders' single session-scoped
`ApproveAlways` (ADR-0005) collapses BOTH qwen always-variants (project/user) onto
one option - there is no cross-session or per-user standing approval.

**The block renders in the PENDING path only, never through the cache.** The
approval rows are built at render time from `Screen::pending_approval` (a pure
`PendingApproval { approval_id, command, kind, selection }`), NOT from the item's
cached lines. The confirming group is re-rendered specially: `?` (warning) marker
on the confirming call in place of `⊷`, a `warning` box border, and the approval
block (gap row + question + numbered radio) appended after the call's header. The
`RenderCache` never sees the approval, so a settled group's cached lines carry no
approval trace.

**Border precedence** (qwen `ToolGroupMessage.tsx:325`):
shell → `ui.symbol` (grey) > confirming → `status.warning` > `border.default`. A
`run_shell_command` group keeps its grey shell border even mid-approval (shell wins);
the confirming marker still flips to `?`.

**Keys are a SUPERSET of the old modal.** The arrow keys + Enter drive a shared
pure `SelectionList` (see below); the numbered digits `1`/`2`/`3` quick-select;
the legacy `y`/`n`/`a` quick-keys stay (approve-once / deny / approve-always) so
every existing screen test stays green. Escape DENIES THIS TOOL and the Run
CONTINUES - qwen-faithful (`ToolConfirmationMessage.tsx:106-114`: Escape →
`ToolConfirmationOutcome.Cancel` = deny the call), matching the
`No, suggest changes (esc)` option label. It routes to `Decision::Deny`, not a
Run cancel. Escape only cancels the whole Run when NO approval is open and a Run
is streaming (qwen's `esc to cancel` spinner + suspenders' global cancel).

**The radio mechanic is a shared pure `SelectionList`** (`src/ui/selection.rs`, no
ratatui), reused unchanged by the model/theme dialogs (ADR-0051; qwen shares
`BaseSelectionList` across approval + dialogs). Up/Down wrap; Enter selects; digit
quick-select is 1-indexed with qwen's `NUMBER_INPUT_TIMEOUT_MS=1000` immediate-vs-
buffered rule; a host-driven `expire(now)` stands in for qwen's `setTimeout` (no
background timer crosses the pure seam). With the 3-row approval every digit
resolves immediately, so the buffered/expire path is never exercised here; it is
tested in `selection.rs`.

## Consequences

- The approval rows are render-time only (ADR-0046): the confirming Tool Call
  has no result, so it is still in the Pending tail; on resolve the call
  supersedes to a `ToolResult` with NO approval trace in the history. A screen
  seam-identity test and a components render test both prove it.
- Rigidity (ADR-0029) holds: the question + radio rows go through `box_row`, so
  every row is exactly the box inner width. A render test asserts the borders align.
- **Risk (recorded):** if the batch ever ran Tool Calls CONCURRENTLY, the
  newest-position attach could bind the block to the wrong call. `run::batch` is
  sequential by construction; the risk is HIGH-impact / LOW-likelihood and gated
  by that structural fact.
- `render_approval_modal`, the modal overlay call, and the modal sizing constants
  are deleted. `MODAL_MIN_WIDTH` survives (the Session Picker still uses it).
