# Loop heuristics are Governors: a closed Intervention set behind one arbiter

The drive-vet-tune loop keeps producing behavioral heuristics - the Nudge
family, the Endgame schedule (ADR-0015, ADR-0016), Anchor cadence - and
each one landed as wiring across five places: state in the `Nudges`
struct, a predicate, a Voice string, a firing site in `loop_`/`batch`/
`finish`, and re-arm logic. The split-loop refactor (75c1a9b) gave each
concern a file but left the wiring shape intact: the next tuning learning
still costs a five-file diff, and the setpoints had two homes with no rule
(`turn_limit`/`anchor_interval` on the Session, `FAILURE_NUDGE_FROM`/
`EXPLORE_NUDGE_EVERY`/`STUCK_RECENCY` as constants).

## Decision

Formalize the heuristics as **Governors** (CONTEXT.md): tunable rules that
watch the Pass cycle and act only through a closed **Intervention** set -
replace a Tool Result, annotate one, stand alone as a user message, ride
the results tail, narrow the offered Tools, silence Thinking, close on a
marker. Every Intervention belongs to exactly one of three moments of a
Pass - request shaping, Tool Call answering, finish settlement - and one
arbiter owns an explicit precedence order per moment. Facts live in a
**Run Ledger** written once by the loop; each Governor keeps only its
private trigger state and its **Setpoints** (declared with defaults,
resolved by the Session at launch, exposed to user config only when a
real model has demanded a different value). Compaction and Eviction are
budget arithmetic, not Governors - they are correct or incorrect, never
tuned.

## Considered options

- **Open registry (Extension-shaped)**: a trait plus a list, new heuristic =
  new impl, zero loop changes. Rejected because the interactions BETWEEN
  heuristics are the domain, not incidental wiring: Verify-failed >
  Verify > Empty is strict precedence, the Verification Pass prompt
  subsumes the wrap-up warning, duplicate memory clears on a successful
  write, gates re-arm only on progress. A registry makes all of that
  emergent from registration order - invisible, and silently reordered by
  the next tune. The extensibility actually needed is "add heuristic #9
  without touching five files," not third-party registration; the closed
  set delivers that (one module, one Voice string, one precedence line)
  while keeping precedence a single readable function - the artifact a
  tuning session diffs.
- **Heuristics as Middleware**: rejected on a boundary now in CONTEXT.md - a
  Middleware acts on one Tool Call in isolation, fail-open, no Run history;
  a Governor judges the Run's trajectory and needs the Ledger. The
  litmus test is Run history: the duplicate check cannot be a Middleware
  ("still-fresh from the previous Pass" is a fact about the Run), the
  Diff Presenter cannot be a Governor. Approval is neither - the user's
  judgment, not a tuned learning.
- **Status quo**: well-named files, but the five-file wiring cost per
  learning is the shotgun surgery this exists to remove.

## Consequences

- A new KIND of Intervention (an eighth variant) deliberately touches the
  loop's firing sites. That is the trade: a new way to steer the
  Conversation is a visible design decision, per the same
  mechanics-over-prose stance as ADR-0015/0016 - do not "fix" this by
  generalizing the enum away.
- Governors are first-party only. Open extension stays with Extensions
  (ADR-0007), which keep their fail-open contract; Governors have no
  failure mode - they are pure judgment over Ledger facts, and are part
  of the Run's correctness.
- At the Tool Call moment the ordering is fixed: Governors judge what the
  model sent and what the model will read; Middleware shapes what actually
  runs in between (consistent with the existing rule that duplicates key
  on what the model sent while Approval shows the Middleware-adjusted
  command).
- Cross-cutting reads become principled: Endgame reading verification
  state and Settlement reading stuckness are Ledger reads; no Governor
  reads a sibling's trigger state. `stuck()` stays one exported pure
  predicate (failure Governor's setpoints, two readers).
- Migration is a four-step strangler, each step behavior-neutral and
  gated by tests and clippy: arbiter entry points as thin wrappers,
  Ledger carved out of `Nudges`, Governors migrated one at a time,
  `Nudges` deleted. Per ADR-0021, the behavior tests in `loop_` survive
  every step unchanged - a step that forces a test rewrite changed
  behavior and is rejected. `endgame.rs` is already pure: it becomes the
  Endgame Governor by rewiring, not rewriting.
