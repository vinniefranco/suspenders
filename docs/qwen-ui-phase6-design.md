# Phase 6 design — thinking subject + compact mode (Ctrl+O)

Fill the Phase-3 spinner `subject` seam with a rolling thought-subject, and add
qwen's compact mode (Ctrl+O) that hides thinking + tool output. All ratatui in
ui.rs/components.rs (ADR-0019); thinking/compact state pure. Ground truth:
`docs/qwen-ui-reference.md` §6 + qwen source. Two features, one hard problem
(compact over frozen scrollback).

## CRITICAL PRE-FINDING
Ctrl-T (`Key::ToggleThinking` + `thinking_expanded`) is STILL LIVE in the code
(screen.rs ~164/974/308, GREETING ~48). ADR-0046 decided to retire it but left it
inert. Phase 6 completes the retirement.

## Part A — thought subject (low-risk, ship independently)
- qwen `parseThought` (core thoughtUtils.ts): subject = trimmed text between the FIRST
  `**...**`; description = the rest; no bold → subject empty. Spinner: `thought?.subject
  || currentLoadingPhrase` (LoadingIndicator.tsx:72). Cleared on new-prompt/cancel/error;
  vanishes at Idle.
- Suspenders reasoning streams (Delta::Thinking → streaming_thinking(): String) do NOT
  reliably emit `**bold**` subjects. DECISION (divergence, ADR): pure
  `thought_subject(thinking) -> Option<String>` in transcript.rs = (1) parseThought bold
  subject if present, else (2) last non-empty line of streaming_thinking (the live
  reasoning head), else (3) None → spinner falls back to lull phrase. Feed it into the
  Phase-3 `SpinnerState.subject` seam in render_pending_body_at.
- Clear-timing is FREE: streaming_thinking empties between messages (subject→None
  automatically); spinner only renders while Running (vanishes at Idle). No manual reset.
- History: keep committing raw Thinking text (bold subject renders as bold-grey markdown
  inside the grey thought); thought_subject is spinner-only.

## Part B — compact mode (Ctrl+O), the hard part
- qwen compactMode (default false, Ctrl+O TOGGLE_COMPACT_MODE) hides: Thinking items
  (HistoryItemDisplay !compactMode), tool RESULT bodies (ToolMessage `!compactMode ||
  forceShowResult`), keeping tool headers. (Tool-group merge/summary absorption OUT of
  scope — suspenders has no tool_use_summary.)
- DECISION: collapse suspenders' TWO existing expand toggles (thinking_expanded=Ctrl-T,
  tools_expanded=Ctrl-O) into ONE `Screen.compact_mode: bool` (default false = show all).
  Retire Ctrl-T entirely (delete Key::ToggleThinking + handler + thinking_expanded; update
  GREETING + its test). Remap Ctrl-O → `Key::ToggleCompact` flipping compact_mode. NO
  Ctrl-T remains (confirmed qwen Ctrl+T = TOGGLE_TOOL_DESCRIPTIONS, unrelated). `Toggles`
  struct collapses to one `compact: bool` threaded through ~10 sites (compiler enumerates).
  Inversion: old renders full when expanded==true; new renders full when compact==false.
- What compact hides in suspenders: Thinking item → ZERO lines (delete the non-faithful
  collapsed one-liner; qwen only show/hide); tool result body → header row only; live
  thinking tail (live_thinking_lines) suppressed. Spinner SUBJECT stays (qwen doesn't gate
  LoadingIndicator on compactMode).
- THE HARD PART (frozen scrollback): committed Thinking/tool items are frozen in native
  scrollback (insert_before); a compact toggle can't un-draw them → split-brain (old
  thoughts stay, new vanish). qwen solves via refreshStatic = clearTerminal + Static
  remount + replay all committed at the new compactMode, gated by
  compactToggleHasVisualEffect. PORT IT:
  - pure `compact_toggle_has_visual_effect(&Transcript) -> bool` (any committed Thinking or
    tool-group member) in transcript.rs.
  - new `Effect::RedrawScrollback`, minted by the Ctrl+O handler ONLY when the predicate is
    true (else pending-only re-render, free).
  - adapter (ui.rs, sibling to commit_items): `terminal.clear()` + re-blit the WHOLE
    committed slice [0, high_water) via render_committed_slice at the new compact (cache
    rebuilds via needs_rebuild's Toggles key); do NOT reset the high-water mark; then normal
    pending draw. RISK #1 (HIGH): verify terminal.clear() + full re-insert_before doesn't
    double/orphan rows — SPIKE before committing to the API; degraded fallback = viewport-
    only redraw (stale-above-fold) if clear misbehaves.
    - UPDATE (shipped): Risk #1 MATERIALIZED. The faithful re-blit needs to re-anchor the
      inline viewport to y=0 to repaint [0, high_water) from the top, but ratatui 0.29's
      `set_viewport_area` is PRIVATE (the scrollback purge itself is fine — crossterm 0.28
      has `ClearType::Purge`). So the DEGRADED viewport-only fallback SHIPPED: re-sync the
      cache to the new compact + `terminal.clear()` the live region; the frozen prefix keeps
      its old compact (bounded staleness) until it scrolls away. No re-blit. Faithful replay
      is future work gated on an upstream ratatui viewport-anchor/reset API. See ADR-0052.
- committed==pending identity PRESERVED: compact threads through the ONE Toggles→grouped_rows
  path both sides use; after RedrawScrollback the whole committed range is re-blitted at the
  new compact, matching the pending region. Extend the identity test under compact.

## Part C — settled thought persistence (Phase 2 parity)
Phase 2 grey ✦ + grey markdown MATCHES qwen ThinkMessage. Gaps: (1) delete the collapsed
one-liner (qwen has no collapse, only show/hide); (2) continuation gemini_thought_content
split is a qwen streaming-perf concern — suspenders' single Thinking item is semantically
equivalent, NO action. thinking_style plain grey no italic — matches.

## Invariants / ADR-0019
thought_subject + compact_toggle_has_visual_effect + compact_mode PURE (transcript.rs/
screen.rs); Effect::RedrawScrollback carries no ratatui; render + terminal.clear in
ui.rs/components.rs. committed==pending holds (one compact value both sides).

## ADRs
- NEW ADR-0052: compact mode + the RedrawScrollback seam. NOTE (shipped): the faithful
  clear+replay (which WOULD have been a bounded exception to ADR-0046) did not ship; the
  degraded viewport-only fallback did, which touches nothing frozen and stays INSIDE
  ADR-0046. Faithful qwen refreshStatic parity is gated on an upstream ratatui viewport
  anchor/reset API.
- Amend ADR-0046: Ctrl-T retirement COMPLETED in Phase 6; thought-subject fallback divergence.

## Tests
Pure: thought_subject (bold subject / last-line fallback / empty→None / multiline);
compact_toggle_has_visual_effect (empty/user-only false, Thinking/ToolCall/ToolResult/Todo
true); Ctrl+O flips compact_mode + emits RedrawScrollback iff predicate; Key::ToggleThinking
GONE (compile); committable_upto unchanged (compact ≠ commit change). Render: Thinking
compact=false grey ✦ / compact=true empty; tool body hidden under compact, header stays;
spinner subject Some over phrase; committed==pending under compact. Adapter: RedrawScrollback
re-blits committed at new compact, high-water unchanged, cache rebuilt. Manual /verify: stream
reasoning → spinner shows head; settle; Ctrl+O hides ALL thoughts+tool bodies in scrollback
(not just future) + headers stay; Ctrl+O restores; plain-chat toggles no flicker (predicate false).

## Risks
1. terminal.clear()+full re-insert_before correctness (HIGH) — SPIKE first; degraded fallback.
2. Collapsing two toggles → one (MEDIUM, wide) — atomic type change, lean on compiler.
3. Cache invalidation on toggle (MEDIUM) — needs_rebuild must key on compact (already keys Toggles).
4. Subject jitter from last-line fallback (LOW) — one row, spinner cadence; debounce if ugly.
5. GREETING/tests referencing Ctrl-T (LOW) — update in lockstep (tripwire).

## Checklist (green between steps)
1. pure thought_subject + tests. 2. fill spinner subject seam (SHIP Part A — orthogonal, low-risk).
3. pure compact_toggle_has_visual_effect + tests. 4. add Screen.compact_mode (alongside old toggles).
5. collapse Toggles→compact across components.rs; delete collapsed-thought branch; hide Thinking/
   tool-body/live-thinking under compact; update RenderCache key (BIG atomic refactor).
6. retire Ctrl-T (delete Key::ToggleThinking+handler+thinking_expanded; update GREETING+test).
7. remap Ctrl-O→Key::ToggleCompact; handler flips compact_mode + emits RedrawScrollback iff
   predicate (+tests). 8. Effect::RedrawScrollback adapter in ui.rs (SPIKE risk #1 first; clear +
   full committed re-blit, don't reset mark). 9. identity + adapter tests + /verify. 10. ADR-0052 +
   amend ADR-0046.
