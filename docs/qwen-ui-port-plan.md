# qwen-code UI/UX port — working plan

Port qwen-code's TUI wholesale into Suspenders (ADR-0046). This is a living plan,
worked phase by phase across sessions. It is NOT a spec: exact strings, glyphs,
colours, and layout are extracted from ground truth per phase, never inferred.

## Ground-truth rule

Primary source is the **readable qwen-code source pinned to the exact `v0.16.0`
tag** at `/home/vinnie/Sandbox/qwen-code-v0.16.0/packages/cli/src/ui/` (git tag
`v0.16.0`, commit `1b1f4867`). This is the same version the reference screenshots
came from — verified byte-identical to the shipped bundle on drift-sensitive
values (e.g. `TOOL_STATUS` glyphs). Read it directly; do NOT read GitHub `main`,
which has drifted (main uses `✗` where 0.16.0 uses `x`/`-`/`?`).

Tiebreaker oracle: the shipped bundle
`/nix/store/z0vqhnlw648d8736c17bnxw6gbljr1x8-qwen-code-0.16.0/share/qwen-code/cli.js`
— consult only to resolve ambiguity or confirm the tag matches the build (byte
window: `grep -abo SYMBOL cli.js` → `dd ... skip=OFF count=N`).

No mechanic is written until its source is read and cited (file:line).
Screenshot reads are evidence of behaviour, never of mechanism.

## Per-phase protocol (every phase, no exceptions)

1. **Extract** — pull the phase's components from the bundle, cite offsets, record
   the exact spec (strings/glyphs/colours/layout/logic) in this doc.
2. **Implement** — dispatch a high-level implementation agent. Agents run
   **sequentially** when they touch overlapping files (`components.rs`,
   `screen.rs`, `transcript.rs` overlap heavily), parallel only when disjoint.
3. **Adversarial review** — dispatch elite reviewer agent(s) (persona below) the
   moment the implementer is done. Reviewers hunt for wrongness, ugliness, and
   over-complexity; where complexity appears they must name the design pattern
   that dissolves it.
4. **Remediate** — fix every accepted finding.
5. **Re-review** — a fresh reviewer confirms the remediation and finds nothing new.
6. **Gate** — `cargo test`, `cargo clippy` (deny warnings), `rustqual` (no
   regression vs baseline). Phase is not done until all green.

## Reviewer persona (the adversary)

A distinguished staff+ engineer with decades in systems and terminal UIs. Ruthless
on: separation of concerns (the pure Screen core must never touch crossterm/ratatui
— ADR-0019; only `ui.rs` + `components.rs` may), clean-code naming and cohesion,
design-pattern fit (state/strategy/newtype-typestate over sprawling match/flags),
Rust idiom, and dead-code. Stance is adversarial: assume the diff is wrong or ugly
until proven otherwise; for every "this is complex" produce the simpler pattern; for
every "this is odd" produce the beautiful version. Also checks fidelity: does the
output match the cited 0.16.0 ground truth exactly?

## Phases

### Phase 0 — Ground-truth extraction (read-only)
Extract and document every UI component from the bundle into a spec appendix here.
No code changes. Deliverable: the appendix below, filled and cited.

### Phase 1 — Rendering seam (inline + Commit) [FOUNDATION]
Swap `ratatui::init()` alternate-screen for an inline viewport. Implement the
Commit path: pure Screen core marks items committable (final, strict in-order,
stop at first pending) and emits a `Commit` effect; the adapter drains it via
`insert_before` into native scrollback. Retire `viewport.rs`, the scrollbar,
mouse-scroll capture, and scroll-key effects. Everything else renders into this.
Blocks all later phases.

### Phase 2 — Committed history rendering
`HistoryItemDisplay` equivalents: user prompt, assistant text, info, tool-call box
(✓/? marker, rounded border, name + dim summary), tool result, diff (keep syntect
+ tint), the committed `TodoWrite` call list. One item, printed once, frozen.

### Phase 3 — Pending region layout
`MainContent` + `pendingHistoryItems`: in-flight tool calls, streaming assistant
message, the sticky "Current tasks" box (verified spec in appendix), the
`LoadingIndicator` spinner line (phrase / thought-subject + elapsed · ↑ tokens ·
esc to cancel), the composer, and the `Footer` (approval-mode indicator + `N%
used`). Depends on Phase 1.

### Phase 4 — Approval flow (inline)
`Apply this change?` numbered radio as an inline block in the pending region (not a
modal), key capture, the mode options. Depends on Phase 3.

### Phase 5 — Menus revamp
Slash-command, model, and theme selectors adopt qwen's interaction: arrow-nav +
type-to-search + `Press Enter`. Depends on Phase 3.

### Phase 6 — Thinking mechanic
Rolling thought-subject on the spinner line; resolve whether completed thoughts
persist as `✦` history bullets (extract before building). Depends on Phase 2/3.

### Phase 7 — Themes reconcile + polish
Map Suspenders' theme slots onto qwen's colour roles; verify diff/syntect intact;
final fidelity pass against all five reference screenshots.

## Extraction appendix (filled during Phase 0)

### Sticky "Current tasks" box — VERIFIED (cli.js 0.16.0, `StickyTodoListComponent`)
- Glyphs `STATUS_ICONS3`: pending `○` U+25CB, in_progress `◐` U+25D0, completed `●` U+25CF.
- Order `getOrderedStickyTodos`: stable sort by `STICKY_TODO_STATUS_PRIORITY`
  {in_progress:0, pending:1, completed:2}, tie-break original index.
- Number label = original position `index+1` (unsorted), so a floated in-progress
  item keeps its original number (Image #4: `7.` above `1.`).
- Cap `STICKY_TODO_MAX_VISIBLE_ITEMS = 5`; overflow row `"... and {{count}} more"`.
- Colours: in_progress = AccentGreen, else Foreground; completed = strikethrough.
- Box: `borderStyle: "round"`, marginX 2, paddingX 1, header bold `"Current tasks"`.

### Footer context usage — VERIFIED (cli.js 0.16.0, `ContextUsageDisplay`)
- `percentage = promptTokenCount / contextWindowSize`; hidden when count is 0.
- Label `"% used"` when `terminalWidth < 100`, else `"% context used"`; over-limit
  branch when `percentage > 1`. Formatting via `formatPercentageUsed`.

_(remaining sections added as Phase 0 extracts them)_
