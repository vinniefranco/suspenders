# The Open Plan: a third evidence of unfinished for the Recovery Run

A start-to-finish greenfield build (Conway's Game of Life, `Qwen3.6-27B`,
session 20260728-025325) completed well but made the user babysit it: the
Run fragmented across four Runs, hit the Run Limit three times, and twice
the user had to type "continue" to keep it going. The two Runs that
auto-recovered did so on the broken-state arms (ADR-0028); the two that
handed back had settled **green** with the model's Plan still showing
unchecked steps. The Recovery Run's trigger had no evidence for "correct,
but not done," so a green-but-incomplete Run returned to the user - the
babysitting.

## Decision

The Recovery Run's trigger (ADR-0028: `unverified_writes || (dangling_failure
&& wrote_this_run)`) grows a **third evidence of unfinished - the Open
Plan** - rather than a new Run concept. Green-but-incomplete is not a
sibling of Recovery; it is the same "the work is demonstrably unfinished,
issue one more bounded attempt" umbrella, widened from "broken" to
"broken *or* not-yet-complete." No new Intervention kind enters the closed
set (ADR-0026): `close-and-open-a-Recovery-Run` is unchanged. The
`Recovery` payload's `verification_failing: bool` generalises to a
`reason` (unverified writes, Dangling Failure, or Open Plan), the fact the
Voice's recovery prompt is already parameterized with.

An **Open Plan** is a syntactic fact: the Plan's content contains at least
one unchecked-box line (`[ ]`). The scan lives in the Plan's own module,
not the Ledger (the Ledger stores the computed fact, it does not parse
markdown - its facts-not-opinions invariant). It is **fail-safe toward
stopping**: fenced code blocks are excluded (a Plan documenting `- [ ]`
syntax must not read as forever-open), and any ambiguity resolves to
not-open. A Plan with no checkboxes can never be Open - completeness is
uninferrable from prose - so the mechanic is a deliberate no-op for
prose/emoji plans.

Three guards keep a small model from spinning on its own checkboxes:

1. **Green is an inviolable precondition.** The Open Plan arm fires only
   when no broken-state evidence holds. A Plan claiming "done" while writes
   are unverified still routes to repair; a Plan claiming "open" never
   suppresses a Dangling-Failure repair. Precedence is resolved inside the
   one judgment (broken arms first), so the arbiter gains no new
   cross-Governor ordering.
2. **Made-progress guard.** The Run must have checked off a Plan step this
   Run (checked-box count strictly increased over a Run-start baseline).
   This mirrors ADR-0028's 2026-07-14 `wrote_this_run` addendum exactly: a
   Run that shows no progress is one a continuation would simply repeat.
   Without it, a model that keeps editing but checks no new box would burn
   the whole budget on no-progress churn - strictly worse than the one
   "continue" this ADR removes.
3. **A separate, larger Setpoint.** `advance_limit` (default 3) bounds
   Open-Plan continuations per user request, distinct from `recovery_limit`
   (default 1) for broken states: self-continuing a green build is a
   different risk profile than one-shot break-fixing, and carries its own
   counter, reset per user request and restored on Resume.

**Shape diverges by reason: an Open Plan reopens as a Continuation, not a
Handoff.** ADR-0028 defaults broken-state recovery to Handoff on the prior
that "a fresh context beats continuing a degraded one." A green,
step-advancing Run is not degraded - it is productive - so that prior
inverts: retiring the Conversation throws away the working context (file
contents, what is built, test state) the next steps need. The same session
proved the cost live: both auto-recoveries were Handoffs, and each was
immediately followed by the model re-reading the files it had just built
(the Explore Nudge fired). Continuation keeps that context while budget
lasts; Compaction absorbs it once when the Context Budget is finally
pressed, instead of Handoff discarding it unconditionally at every
boundary.

## Considered options

- **A separate "Progress Run" concept** mirroring the Recovery Run's spine
  (parallel Setpoint, payload, `FinishIntervention::CloseProgress`, and a
  `progress_used` counter plumbed through the Agent): rejected. It
  duplicates an entire mechanism to model a third trigger the existing one
  already means, and adds an Intervention kind - which ADR-0026 marks a
  visible design decision not to be taken to avoid a `||`. ADR-0028's own
  history is the precedent: its trigger grew from one arm to two by
  addendum, never by spawning a sibling Run.
- **Handoff shape for the Open Plan arm** (the uniform choice): rejected on
  the live re-read evidence above. Shape is now a function of reason.
- **Raise the Run Limit only**: rejected as the primary fix. It reduces
  boundaries but a green-but-incomplete Run still hands back to the user;
  it only defers the babysitting. (Sizing the limit remains an orthogonal
  knob.)
- **Trust a richer completeness signal than checkboxes**: rejected. The
  Plan is the model's voice, and this codebase has documented the model
  gaming the green signal (ADR-0028 addenda). The checkbox is therefore a
  *bounded, revocable, non-authoritative* signal - never allowed to
  override a mechanical one - which the green precondition and the two
  budgets enforce.

## Consequences

- The recovery-run Session Log entry and Transcript event carry the
  `reason`, so an Open-Plan continuation logs and greps distinctly from a
  broken-state recovery while remaining one mechanism; Resume restores the
  spent `advance_limit` count alongside the recovery count.
- The mechanic silently no-ops for any model that writes prose or
  emoji-styled plans (the source session itself used `## Step 6: DONE ✅`,
  zero `[ ]`). Validation must measure the target model's checkbox-emission
  rate, not just the happy path - a low rate means the headline benefit
  does not land and the steer belongs in the Voice (nudge the model to keep
  the Plan's boxes current), not here.
- Handoff-summary enrichment (carrying open-step state into the seed) was
  considered and dropped: it would reverse the "the Plan never enters the
  seed" invariant and compete with the Anchor, which already re-injects the
  Plan verbatim. With Continuation as the Open Plan shape, the Conversation
  is kept anyway, so the seed is not on this path.
- Validation pending: re-run a checkbox-planned greenfield build on a small
  local model with `advance_limit=3` - success is multiple steps driven
  across Continuation boundaries with no user "continue", a clean plain
  close once every box is checked, and at most one further reopen when the
  model stops checking boxes (the made-progress guard holding).
