# The Recovery Turn: the eighth Intervention, with Handoff as its default shape

For hard implementation tasks the Turn Limit, not ability, was the
binding constraint: 12 of 15 f5 runs died at the 32-pass cap
(docs/tuning/LOG.md 005–006), including near-misses one honest
debugging turn from green and mid-refactor compile errors a single
"make it compile" turn would repair — while the same model solved the
same fixture in 13 passes when its first design was sound. Variance,
not capability, and a capped Turn had no recovery opportunity.

## Decision

The closed Intervention set (ADR-0026) grows its eighth kind — **close
the Turn and open a Recovery Turn** — issued by the Endgame Governor
when a Turn closes at its Turn Limit with the Ledger showing unfinished
work (`unverified_writes || command_failing`), bounded per user request
by the `recovery_limit` Setpoint (default 1; 0 disables). The Agent
executes the opening. A capped Turn that settled green closes plain.

The Recovery Turn is the first Turn whose prompt belongs to the Voice
rather than the user; it still serves the original user request.
Rollover outranks it (rolled-over Steering is the user's own
continuation of the same request), and Cancellation suppresses it.

Two shapes, chosen by the `recovery_shape` Setpoint:

- **Continuation** keeps the Conversation and appends the recovery
  prompt.
- **Handoff** (default) retires the Conversation and seeds a fresh one
  through the compaction machinery: the model narrative, the mechanical
  facts appended outside the LLM (original task verbatim, files
  touched), the final `run_command` result verbatim — the single
  highest-value artifact for a debugging continuation — and the
  recovery prompt. The Plan survives verbatim because the harness owns
  it. A failed summarize degrades to the mechanical skeleton; recovery
  never fails on its own machinery.

## Considered options

- **Pick one shape by measurement first**: the c007 two-turn protocol
  was designed for exactly this comparison but was invalidated by the
  model server going down and the fixtures are gone (/tmp reboot). Both
  arms are cheap once the trigger and vet plumbing exist, so both were
  built and the choice left measurable. Handoff is default on the prior
  that a fresh context with a structured handoff beats continuing a
  degraded one, and the gap widens as models shrink.
- **An Agent-level mechanic like Rollover**: rejected — the trigger is
  a judgment over Ledger facts and carries Setpoints, which is
  Governor-shaped through and through; Rollover has no trigger logic,
  it mechanically delivers the user's own words.

## Consequences

- Per ADR-0026's own consequence, a new Intervention kind is a visible
  design decision touching the loop's firing sites; this ADR is that
  record.
- Both arms round-trip the Session Log (`recovery` and `handoff`
  entries); Resume restores the spent recovery count, so a resumed
  session cannot re-trigger unboundedly.
- Validation pending fixture rebuild: re-run the c007 protocol
  (LOG.md 007) as Continuation-vs-Handoff-vs-off on the f5 scorecard.

## Addendum (2026-07-13): two implementation holes, found live, closed

The stated trigger — a Turn closing at its Turn Limit with unfinished
work — was implemented only for the tool-answering cap and the
tool-insistent reply. But ADR-0015 withdraws every tool on the final
Pass, so a capped Turn nearly always ends as a plain text settle with
`end_turn`, and that path never consulted the recovery judgment: in a
5-run batch with every run capped and red, recovery fired once (the
one tool-insistent reply). The final-Pass text settle now consults the
same judgment, restoring the intent above. One difference in the
close: the reply is the model's genuine wrap-up, not insistent markup,
so it enters the Conversation before the close — Handoff's
compaction-seeding and Continuation both read it. A green settle at
the cap, or a spent budget, still concludes on the reply as before.

The judgment's failing arm was `command_failing` — last-command-only —
and models were observed laundering it: a red full-suite run followed
by a green filtered rerun (`cargo test one_test_name`, exit 0) read as
green at the cap. The failing arm is now the Dangling Failure: the
Ledger records the most recent outcome per distinct command string
this Turn, and the judgment (and the `verification_failing` fact the
recovery prompt is parameterized with) fires when any command string's
most recent run failed — a passing run clears only its own string.
`command_failing` and its other consumers (the Verify-failed Nudge)
are untouched.

## Addendum (2026-07-14): a false recovery on read-only work, found live, closed

A read-only "evaluate this repo" task settled green at the cap and yet
opened a Handoff Recovery Turn, which handed the model a fresh
Conversation and made it restart the whole evaluation — the `restart`
CONTEXT.md's Handoff entry explicitly forbids
(session 20260714-174034). Three faults chained:

1. **A spurious failure the exit code could not disown.** The model ran
   `cargo test --lib 2>&1 | head -200`; under `run_command`'s `bash -o
   pipefail` (a deliberate choice — a pipe must report the producer's
   failure), `head` closed the pipe early, cargo died writing to it, and
   the pipeline reported exit `101` — cargo's own code, indistinguishable
   from a real test failure. The model re-ran with `| tail -50` (exit 0,
   1022 passed), but that is a *different command string*, so the
   Dangling Failure from the `head` run never cleared. This is the
   anti-laundering rule of the 2026-07-13 addendum firing as a false
   positive.

2. **The trigger fired on exploration.** With zero writes, the
   dangling-failure arm fired alone. But this ADR's whole evidence base
   is unverified writes and mid-fix near-misses; a failing command during
   pure exploration is not unfinished implementation. The dangling-failure
   arm now additionally requires that a write landed this Turn — a new
   monotonic Ledger fact, distinct from `unverified_writes` (which clears
   on the next `run_command`). Recovery fires on `unverified_writes ||
   (dangling_failure && wrote_this_turn)`. Per-Turn scope; `recovery_limit`
   already bounds re-firing. Accepted trade-off: a capped attempt that
   never managed a single write no longer recovers — but a Turn with no
   writes across the whole cap has shown no progress a Handoff restart
   would not simply repeat. The laundering protection is untouched: that
   case always writes.

3. **The Handoff seed contradicted its own prompt.** The seed carried the
   *last* `run_command` result (the green `tail` rerun) while the
   `verification_failing` prompt said "fix the failure" — a contradiction
   the model could only resolve by starting over. The seed now carries the
   Dangling Failure's *own* result verbatim (the failing command string's
   last result, the command the prompt names), threaded from the Ledger
   through the `Recovery` payload. This also fixes the canonical laundering
   case (red suite → green filtered rerun → recovery), where the seed would
   otherwise carry the green rerun.

The generative cause — the model piping test output through `head` to
manage size — is met with a Voice steer (system prompt): run commands
bare, because the Result Cap already truncates output and keeps the exit
code, and piping through `head`/`tail`/`wc` under `pipefail` can make a
passing command report failure. Unlike an exit-code heuristic, a steer
generalizes: a pipe-induced `101` cannot be told from a real one.
