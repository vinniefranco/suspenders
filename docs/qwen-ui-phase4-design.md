# Phase 4 design — inline approval flow + approval-mode indicator

Convert the approval MODAL to qwen's inline block (inside the confirming tool's box,
pending region) + add the Shift+Tab approval-mode cycle + the footer AutoAcceptIndicator
(Phase-3 seam). All ratatui in ui.rs/components.rs (ADR-0019); approval/mode/selection
state pure. Ground truth: `docs/qwen-ui-reference.md` §4 + qwen source at
`/home/vinnie/Sandbox/qwen-code-v0.16.0/packages/cli/src/ui/`. Preserve Phase-1
committed==pending identity (approval is pending-only; the tool commits after resolve
with no approval rows).

## Decisions

1. **Inline approval attaches to the NEWEST pending ToolCall by position** (ADR). The
   ApprovalRequest event carries only `approval_id` + `command`, not a tool_use id;
   suspenders executes batch tools sequentially (`batch.rs:43`), so the gated call is
   the newest pending `ToolCall` with no result — the same newest-match identity the
   transcript already uses. No event/Dep plumbing. Render: delete `render_approval_modal`
   + the overlay; `tool_inner_lines` for the confirming call appends, after its header:
   a gap row, the question line (`primary_style`), then the radio rows — all via `box_row`
   (rigidity holds). Confirming marker flips `⊷`→`?` (`ToolMarker::Confirming`, warning);
   `group_border_style` gains a warning branch (shell→symbol > confirming→warning >
   border.default, per ToolGroupMessage.tsx:325).

2. **Shared pure `SelectionList` widget** (new `src/ui/selection.rs`, no ratatui) —
   reused by Phase 5 dialogs (qwen shares BaseSelectionList across approval + model/theme).
   `SelectionList { active, len, number_buffer }`, `SelectionKey {Up,Down,Enter,Digit,Escape,Other}`,
   `SelectionOutcome {Moved,Selected(i),Cancelled,Ignored}`, `handle(key, now)` + host-driven
   `expire(now)` for the `NUMBER_INPUT_TIMEOUT_MS=1000` digit quick-select (no bg timer —
   ui.rs tick drives expire). Wrap up/down, Enter→Selected, Digit quick-select, Esc→Cancelled.
   Render `selection_rows(items, active, show_numbers)`: `›` gutter (2-wide, success when
   active), right-aligned `N.`, label success-when-active else primary, truncate. Do NOT
   fold picker.rs/selector.rs in now — converge in Phase 5.

3. **Only exec + info gated today** (`approvals.rs:28`: run_command, web_fetch). ConfirmKind
   {Exec, Info}, derived from the attached ToolCall.name (no event change). Questions/options
   verbatim: Exec `Allow execution of: '{command}'?`; Info `Do you want to proceed?`; both →
   `Yes, allow once`(Approve) / `Always allow in this project`(ApproveAlways, the no-{{action}}
   fallback) / `No, suggest changes (esc)`(Deny). Edit/plan/mcp = future exhaustive arms
   (stubbed generic). Standing approval (ADR-0005 session-scoped) collapses BOTH qwen
   always-variants (project/user) onto suspenders' single `ApproveAlways` — no cross-session
   persistence, no per-user scope (deliberate, ADR-0005-grounded). proceed_always_user/
   modify_with_editor/restore_previous NOT implemented.
   Key routing: map key→SelectionKey→drive the widget; Selected(i)→that option's Decision→
   `AgentCommand::Approve(id, decision)`. PRESERVE `y`/`n`/`a` quick-keys + digits (superset,
   keeps existing tests green). Escape→deny THIS tool (`Decision::Deny`), Run continues -
   qwen-faithful (ToolConfirmationMessage.tsx:106-114), matching the `(esc)` option label.
   Escape only cancels the Run when NO approval is open and the Run is streaming.

4. **Approval-mode state on `Approvals` (pure), mirrored to Screen** (ADR).
   `ApprovalMode {Plan, Default, AutoEdit, Auto, Yolo}` (default Default); `cycle_mode()`
   fold = plan→default→auto-edit→auto→yolo→wrap (qwen APPROVAL_MODES order). Gate consults
   it: Yolo→Request::Auto always; Default→today; Plan/AutoEdit/Auto→behave as Default this
   phase (display-complete, behavior-stubbed — suspenders has no classifier/plan-loop;
   AutoEdit is vacuous since edits aren't gated). Shift+Tab: ui.rs map `KeyCode::BackTab`
   (and `Tab`+SHIFT) → `Key::CycleApprovalMode` → `AgentCommand::CycleApprovalMode` → Agent
   `cycle_mode` + broadcast `Event::ApprovalModeChanged` → Screen mirror.

5. **AutoAcceptIndicator = a footer status segment** (fills the Phase-3 left_bottom seam).
   `StatusSegment::ApprovalMode(mode)` pushed only when mode≠Default (qwen default→nothing);
   labels `plan mode`(green)/`auto-accept edits`(yellow)/`auto mode (classifier-evaluated)`(yellow)/
   `YOLO mode`(red) + a secondary ` (shift + tab to cycle)` segment (two-color, so split into
   two segments).

## Invariants
- ADR-0019: ApprovalMode/ConfirmKind/SelectionList pure (approvals.rs/screen.rs/selection.rs);
  all render in components.rs; keys in ui.rs/screen.rs.
- committed==pending: approval lines gated on pending_approval (never set for committed items);
  the confirming call can't commit (no result); on resolve it supersedes→ToolResult and commits
  with no approval rows. Extend the seam-identity test.
- Rigidity (ADR-0029): approval rows through box_row → ≤ inner width.

## ADRs to write
- ADR-0049: inline approval attaches to newest pending ToolCall (position, sequential batch);
  modal→inline; `?`/warning confirming marker + warning group border.
- ADR-0050: approval-mode state model (pure on Approvals, mirrored to Screen; 5-mode Shift+Tab
  cycle; Default/Yolo live, Plan/AutoEdit/Auto display-only→Default; both always-variants →
  session-scoped ApproveAlways per ADR-0005).

## Tests
approvals.rs: cycle order, Yolo→Auto, Default unchanged, ApproveAlways standing. selection.rs:
wrap, Enter, digit immediate/timeout(expire), Esc, out-of-range. screen.rs: existing y/n/a/Esc
green; arrow+Enter→right Decision; digit 2→ApproveAlways; CycleApprovalMode emits cmd;
ApprovalModeChanged mirror; cycle-while-open doesn't disturb pending_approval. agent.rs:
CycleApprovalMode folds+broadcasts; Yolo auto-runs a gated call. components.rs: selection_rows
styling; inline approval appends question+radio (row width==inner); confirming ?/warning marker
+ warning border; footer segment per mode; committed==pending (resolved approval commits with NO
approval rows). Delete render_approval_modal tests.

## Risks
1. Position-attach desync if tools ever run concurrently (HIGH-impact/LOW-likelihood, batch.rs:43
   sequential) — assert sequential in a test; ADR records it.
2. Escape=deny-tool (Run continues), qwen-faithful (ToolConfirmationMessage.tsx:106-114),
   matches the `(esc)` label; the whole-Run cancel stays on Escape when no approval is open.
3. Number-timeout timer crossing pure seam — host-driven expire(now); ≤4 options so never fires
   in Phase 4.
4. Mode stubs (Plan/AutoEdit/Auto) misleading (footer says plan, files still modified) — ADR
   documents; only Yolo/Default change behavior; consider a startup notice (defer).
5. Border warning precedence — mirror exact (shell>confirming>default); unit-test all three.

## Checklist (green between steps)
1. approvals.rs: ApprovalMode + mode + cycle_mode + mode-aware request (Yolo→Auto).
2. agent.rs: handle CycleApprovalMode → cycle+broadcast.
3. event.rs+screen.rs: Event::ApprovalModeChanged, AgentCommand+Key::CycleApprovalMode, mirror+arms.
4. ui.rs: BackTab/Shift+Tab → CycleApprovalMode.
5. Footer: StatusSegment::ApprovalMode + cycle-hint + push rule + paint/style + view wiring.
6. selection.rs: pure SelectionList + tests.
7. screen.rs PendingApproval: + SelectionList + ConfirmKind; rewrite gate leg driving the widget,
   preserve y/n/a/Esc.
8. components.rs: ToolMarker::Confirming ?, warning border branch, selection_rows, inline approval
   in tool_inner_lines, wire render_pending.
9. Delete render_approval_modal + overlay + modal tests; extend seam-identity test.
10. Full test/clippy/rustqual + live smoke (exec approval + Shift+Tab). Write ADR-0049/0050.
