# Tuning Log

The drive-vet-tune loop (CONTEXT.md: the loop ADR-0026 formalized the *output*
of; this is its *drive* and *vet* halves). One entry per tuning cycle: the
fixture, the observed failure, the tweak, the N=5 confirmation, the verdict.

The loop:
- **Drive** — fixture in `/tmp` as a git repo; run headless.
- **Vet** — stdout + Session Log + `git diff` + frontier-grade judgment;
  "did it finish / burn turns / was the code good."
- **Tune** — whatever surface the vet points at: Setpoints, Voice, Anchor
  structure, Tool shape, Governor behavior.
- **Repeat** — N=5 before a tweak is credited; `git reset --hard && git
  clean -fd` between runs. Tuning is guidance, not muscle (no sampling sweeps).

A tweak is credited only when the same failure mode clears across five runs at
the same config. A single run is a coin flip on a stochastic 9B; the N=5 rule
exists to stop phantom-chasing.

## Surfaces (in priority order, not a menu)

1. **Setpoints** — cadences and thresholds (turn_limit, anchor_interval,
   nudge-from-passes, explore-every, stuck recency, eviction_slack,
   compaction_keep, no_think_rescue).
2. **Voice** — the wording of every Suspenders-voiced string (system prompt,
   nudge prose, markers, Anchor framing). Highest leverage per voice.rs.
3. **Anchor structure** — what the Anchor carries beyond Plan+task.
4. **Tool shape** — a tool's schema, its own error/result wording, Result-Cap
   cut behavior.
5. **Governor behavior** — new triggers, new kicks, new Intervention kinds
   (a new Intervention *kind* is a visible design decision per ADR-0026).

---

## 001 — Voice Verify step: "write a test for new behavior"

**Fixture:** `f2-csv-comments` — a small Rust CSV parser (7 tests green) asked
to add comment-line support (`#`-prefixed lines skipped). Fuzzy, no oracle:
the prompt deliberately did not mention testing or verifying, to stress
self-verification.

**Observed failure (baseline, no tweak):** the model finished in 9 passes,
declared done, ran `cargo test` (existing 7 passed) — but wrote **zero** tests
for the new behavior. The implementation was catastrophically broken
(`parse("# comment\nx,y\n")` → `[["# a comment","x","y",""]]` — the comment
was not skipped and the data row was merged into it). The model declared a
broken, untested feature complete.

**Diagnosis (guidance, not capability):** the system prompt's Verify step
said only "run the tests or the compiler." The model read existing-green as
verification. The Verify Governor's trigger ("writes unverified by
`run_command`") was satisfied — `cargo test` ran — but the tests did not
cover the new code. "Verified" ≠ "covered." See the parked insight below.

**Tweak (surface: Voice):** two surgical edits to `SYSTEM_PROMPT` in
`voice.rs`:
- Verify step: appended "If you added new behavior, write a test that
  exercises it and run the full suite - existing tests passing alone does not
  confirm new code works."
- Rules: appended to the "fix code under test" rule — "Adding new tests for
  new behavior is always correct and expected." (chill-relief: the rule may
  have discouraged test-writing for a small model that over-reads it.)

**N=5 (tweaked config):**

| run | passes | final cargo test | verdict |
|-----|--------|------------------|---------|
| 1   | 22/25  | 13 pass, 0 fail  | correct |
| 2   | 18/25  | 14 pass, 0 fail  | correct |
| 3   | 25 (cap) | 13 pass, **2 fail** | did not converge |
| 4   | 25 (cap) | 13 pass, 0 fail  | correct but barely (still iterating at the cap) |
| 5   | 12/25  | 11 pass, 0 fail  | correct, fast |

**Verdict: partial win — the targeted failure cleared, a new failure surfaced.**

- **False-completion (the targeted failure): cleared 5/5.** Every run wrote
  new tests (11–15 total vs baseline 7). No run declared broken code done.
  The Voice tweak did its job.
- **Budget-convergence (newly exposed): 3/5 clean, 4/5 a near-miss, 1 hard
  fail.** The model now does the right thing (writes tests, iterates to
  correct) but on a 9B that iteration is expensive — run 3 burned all 25
  passes and left 2 failing tests; run 4 only finished green by luck of the
  cap.

**Why N=5 mattered:** a single tweaked run (run 1, 22 passes, clean) would
have looked like an unqualified win. Five runs reveal a real improvement that
*shifted* the failure, not a clean fix. Shipping on one run would have hidden
a 1-in-5 hard failure and a 1-in-5 near-miss.

**Next (cycle 002):** the rework bleed. Watching the logs, the model is
inefficient in its debug loop — it re-reads whole files repeatedly and
rewrites the entire `parse` function on each edit instead of surgical edits,
despite a system-prompt rule already saying "keep edits minimal." The turn
budget bleeds on rework. Cheapest lever: Voice — strengthen the edit-minimal
rule into something a 9B actually obeys.

### Parked insight — the Verify coverage gap

The Verify Governor fires when "writes are unverified by `run_command`" —
i.e., no command has run since the last write. But `cargo test` running
satisfies it even when the test suite does not cover the new code. "Verified"
≠ "covered." This is durable and not yet a decision. It becomes an ADR
candidate only if a real tuning cycle proves that closing the gap (e.g., a
new "untested new code" trigger, or a Voice nudge asking the model to confirm
its tests touch the changed code) measurably helps. Recorded here so it does
not get lost; do not promote to an ADR until a cycle demonstrates it.

---

## 002 — Voice "keep edits surgical" — REJECTED

**Fixture:** Same as 001.

**Tweak (surface: Voice):** Replaced the vague "keep edits minimal" rule with
concrete surgical-edits wording:

> Keep edits surgical. Give edit_file the smallest old_str/new_str that
> captures the change - just the few lines you are changing, plus enough
> context to be unique. Never paste a whole function back to alter a few
> lines; that wastes turns and risks dropping working code. When a fix needs
> several tries, re-read only the region you changed, not the whole file.

**N=5:**

| run | passes | tests | new | result |
|-----|--------|-------|-----|--------|
| 1 | 25 (cap) | 15 | +8 | 13 pass, 2 fail |
| 2 | 25 (cap) | 14 | +7 | 13 pass, 1 fail |
| 3 | 25 (cap) | 7 | +0 | 7 pass (didn't add feature!) |
| 4 | 25 (cap) | 7 | +0 | 7 pass (didn't add feature!) |
| 5 | 24 | 12 | +5 | 12 pass, 0 fail |

**Verdict: REGRESSION.** The tweak backfired badly:
- 2 runs (3-4) didn't even attempt the feature — classic false-completion
  from BEFORE the 001 Verify tweak.
- The model likely over-read "surgical" as "don't change anything."
- Hypothesis wrong; revert to 001 config.

---

## 003 — Setpoint: turn_limit 25 → 32 (with Verify-tweak from 001)

**Fixture:** Same as 001.

**Tweak (surface: Setpoints):** Bumped `turn_limit` from 25 to 32 in
`session.rs` to give the model more runway when doing the right thing
(writing tests, iterating to correct). Kept the 001 Verify-tweak in Voice.

**N=5:**

| run | passes | tests | new | pass | fail | verdict |
|-----|--------|-------|-----|------|------|---------|
| 1 | 17/32 | 11 | +4 | 11 | 0 | correct |
| 2 | 24/32 | 10 | +3 | 10 | 0 | correct |
| 3 | 32 (cap) | 12 | +5 | 11 | 1 | incomplete |
| 4 | 28/32 | 13 | +6 | 13 | 0 | correct |
| 5 | 26/32 | 12 | +5 | 12 | 0 | correct |

**Verdict: IMPROVEMENT over 001.** 4/5 clean (vs 001's 3/5), 1 incomplete
(with 1 failing test, not 2). The model uses more passes (17–32, avg ~25)
and mostly converges within the extra runway. The 1 incomplete run is still
a regression (the model didn't converge even with 32 passes), but it's less
severe than the 001 hard-fail. The stochasticity of a 9B means 5/5 may not
be achievable on this fixture; the next lever would be turn_limit → 40 or
the "done" bar lowered (e.g., all tests green = done even if no new tests).

**Current best config:** Voice Verify-step from 001 + turn_limit=32.

---

## 004 — Fixture: semantic bug find+fix+test

**Fixture:** `f3-semantic-bug` — a small string-utils crate (`last_index_of`,
`count_chars`, `is_numeric`) with a hidden off-by-one bug in `last_index_of`
(the function returns `position + 1` instead of `position`). The baseline has 8
passing tests that don't catch the bug.

**Prompt:** The agent is told there is a subtle bug, to find it, write a test that
exposes it, fix it, and verify all tests pass. This tests the "find bug without
guidance" capability plus the Verify-tweak's encouragement to write tests.

**Config:** Cycle-003 config (Verify-tweak + turn_limit=32).

**N=5:**

| run | passes | tests | new | pass | fail | verdict |
|-----|--------|-------|-----|------|------|---------|
| 1 | 11/32 | 11 | +3 | 11 | 0 | correct |
| 2 | 16/32 | 9 | +1 | 9 | 0 | correct |
| 3 | 11/32 | 10 | +2 | 10 | 0 | correct |
| 4 | 14/32 | 9 | +1 | 9 | 0 | correct |
| 5 | 14/32 | 10 | +2 | 10 | 0 | correct |

**Verdict: 5/5 CLEAN.** Every run found the bug, wrote at least one new
test that exposed it, fixed the bug, and verified all tests pass. The
Verify-tweak is clearly working — the agent writes tests for new behavior
(even when that behavior is "exposing a hidden bug"). The fixture is
excellent for testing this capability.

**Observations:**
- Runs 2, 4 wrote only 1 new test but still caught the bug (the simplest
  test that exposes the off-by-one was enough)
- Runs 1, 3, 5 wrote 2-3 tests (more thorough edge cases)
- No runs hit the cap — the task is well-scoped for 32 passes
- The prompt explicitly says "write a test that exposes the bug" — this may
  be what drives the test-writing behavior more than the Verify-tweak alone.
  The Verify-tweak is the foundation; the prompt is the immediate trigger.

**Status:** This fixture confirms the current config (Verify-tweak +
turn_limit=32) is working well for the "write tests for new behavior" aspect.

---

## 005 — Fixtures: capability-class baseline (f4/f5/f6)

**Fixtures (new):**
- `f4-analysis` — 5-module CLI task-tracker; task: write ANALYSIS.md
  (purpose, module responsibilities, trace of `add`, where sorting lives)
  without modifying source. Tests codebase analysis.
- `f5-hard-algo` — glob matcher (`*`, `?`, classes, ranges, negation,
  escapes) from a stub against 16 oracle tests; tests may not be modified.
  Tests hard iterative implementation.
- `f6-multifile-bug` — shopcart crate where checkout re-converts an
  already-cents subtotal (100x); symptom given, root cause two modules away.
  Tests cross-module bug fixing.

**Config:** cycle-003 (Verify-tweak + turn_limit=32). No tweak this cycle —
this entry is the baseline for the new capability classes.

**N=5 per fixture:**

| fixture | result | passes used | notes |
|---------|--------|-------------|-------|
| f4-analysis | **5/5 PASS** | 10–15 | frontier audit: zero hallucinated claims in any run; all traces name real functions in true call order |
| f5-hard-algo | **2/5 PASS** | 13–32 | passes: one clean 16/16 in 13 passes, one 16/16 at the 32 cap; fails: 12/16 at cap, and 2 runs left the crate UNCOMPILABLE |
| f6-multifile-bug | **5/5 PASS** | 13–20 | every run fixed checkout.rs (root cause), removed the double conversion, added a regression test mirroring the bug report |

**Verdict:** codebase analysis and multi-file bug fixing are reliable at the
current config. Hard iterative implementation (f5) is the open front: the
model demonstrably has the capability (13-pass clean run) but convergence is
high-variance, and the worst outcomes end broken at the cap.

**Diagnosis (f5 run1, full context-composition audit of the session log):**
- Dead edit inputs dominate: ~66k chars of the final conversation were
  tool_use INPUT payloads (old_str/new_str bodies) — bigger than all tool
  results combined, and worthless once each edit lands. Eviction only
  targets Tool Results, so these never leave.
- Failure-dump pileup: 11/11 cargo test runs failed; four near-identical
  2,660-char failure dumps sat unevicted at the tail third. Signal fraction
  of cargo output ≈ 40% (rest: warnings/Compiling boilerplate, repeated
  verbatim across ~8 runs).
- Stale plan: plan tool called once (pass 5), never updated; all 6 anchors
  re-injected the same outdated "Next step" for the rest of the turn.
- The failure Governor fired 5x ("step back: Nx command exited with error")
  and the model thrashed anyway — the wording names the category but gives
  no strategy; the final third was 5 edits to the same function with the
  same 4 tests failing.
- No eviction/compaction fired (peak ~38k of 64k): this is context QUALITY,
  not overflow.

### Parked insight — Anchors are not persisted to the Session Log

`agent.rs::log_event` records only the 4 nudge event types; `Rider::Anchor`
and endgame riders are live-only. A Resume therefore reconstructs a
Conversation that never contained the anchors the model actually saw.
Fidelity gap; becomes an ADR candidate if a resume-related cycle proves it
matters.

### Parked insight — tool_use inputs are invisible to Eviction

Eviction hollows out Tool Results, but on edit-heavy turns the assistant's
own edit_file inputs are the largest context consumer and never age out.
Candidate mechanics: elide superseded edit inputs the way stale anchors are
elided. Needs a design decision (rewriting assistant history is a bigger
step than rewriting tool results); promote only if a cycle shows wins.

---

## 006 — Voice failure-nudge strategy (inconclusive) → Tool shape: pipefail

**Fixture:** `f5-hard-algo`.

**Tweak A (surface: Voice), cycle 005:** when the dominant failure category
is CommandError, `failure_nudge` now gives a strategy instead of "step back":
"Pick the single simplest failing case, trace its exact input through your
code step by step, and make one targeted fix for it before re-running."

**N=5 (tweak A):** 0/5 — 15/16, 11/16, 2x uncompilable, 9/16, all at the
32-pass cap. BUT the nudge fired only once across all five runs (baseline
run1: five times). **Verdict on A: inconclusive, not credited — the trigger
was blinded, so the wording was never tested.** Kept in place (harmless,
plausibly right) pending a fair test.

**What blinded it (found by auditing the session logs):** the model
habitually verifies with `cargo test 2>&1 | head -N`. Under `sh -c`, the
pipeline's exit code is `head`'s — success. So a red suite arrives as
`is_error: false`: no failure streak (failure Governor silent), and the
Verify Governor counts the run as a PASSING verification. Exit-code
laundering. Piping through `head` also chops cargo's `failures:` section
off the tail — the model hides its own signal, then "verifies" against it.

**Tweak B (surface: Tool shape), this cycle:**
- run_command now executes `bash -o pipefail -c` — a piped command reports
  the producer's failure, not the consumer's success. Mechanical
  correctness for both Governors.
- run_command's description appends: "Do not pipe long output through head
  or tail; output is trimmed automatically."

**N=5 (tweak B, i.e. pipefail + no-pipe hint, with A still in place):**

| run | end state | verdict |
|-----|-----------|---------|
| 1 | COMPILE ERROR (E0308 if/else types) at cap | fail |
| 2 | COMPILE ERROR (E0308 mismatched types) at cap | fail |
| 3 | 16 passed, 0 failed (at cap) | correct |
| 4 | 12 passed, 4 failed at cap | fail |
| 5 | 16 passed, 0 failed (at cap) | correct |

**Verdict: IMPROVEMENT, kept — 3/5 vs baseline 2/5, and verification is now
honest (the new failure-nudge fired 6x in one run vs ~never when blinded).
Not credited as clearing the fixture: the residual failure mode is
convergence at the 32-pass cap — runs that die mid-refactor with type
errors, or plateaued at 12/16. Wording tweak A remains unproven on its own
(rode along in both c005-blinded and c006 runs); it stays because it is
plausibly right and demonstrably harmless.**

**Vet-harness bug (recorded for honesty):** the drive script captured only
`tail -20` of cargo test output, which sometimes lost the unit-test result
line and made green runs look broken (c006 initially scored 1/5; true score
3/5 confirmed by re-applying every run's diff to the clean baseline and
re-running the suite). Also corrected: baseline run4 was a stack-overflow
CRASH (unbounded recursion), not a compile failure. Drive scripts now keep
full cargo output. Frontier-grade judgment applies to the vet half too.

### Corrected f5 scorecard (all runs re-vetted from diffs)

| config | green | failure modes |
|--------|-------|---------------|
| baseline (003 config) | 2/5 | 12/16 plateau; compile error; stack-overflow crash |
| c005 (nudge, blinded) | 0/5 | 15/16 near-miss; 11/16; 2x compile error; 9/16 |
| c006 (+pipefail) | 3/5 | 2x compile error at cap; 12/16 plateau |

12 of 15 runs ended AT the 32-pass cap: for hard implementation tasks the
Turn budget, not ability, is the binding constraint. The near-misses
(15/16, 12/16) are one honest debugging turn away from green.

---

## 007 — Two-turn recovery experiment: INVALID (server shut down mid-batch)

Protocol: same session, turn 1 = the f5 task, turn 2 = "continue until every
test passes" — designed to measure what an auto-continuation/Handoff
mechanic would buy, given that 12/15 f5 runs died at the 32-pass cap.
The model server was turned off during run 1; all c007-* run dirs in
/tmp/fixture-logs are connection failures, not data. Re-run when the server
is back: `TAG=c007 /tmp/run-batch2.sh f5-hard-algo`.

---

## Session close 2026-07-12 — state and handoff

Working config: 003 config + failure-nudge strategy wording (unproven,
harmless) + run_command `bash -o pipefail -c` + no-pipe hint (credited,
006). All changes uncommitted in the working tree; `cargo test` green
(867), clippy clean.

Capability scorecard: codebase analysis (f4) 5/5 audited-clean; multi-file
bug fix (f6) 5/5; hard implementation (f5) 3/5 with the residual failure
being convergence at the 32-pass cap.

Larger fixes written up with evidence in `PROPOSALS.md` (this directory) —
priority order: tool-call input eviction, repeated-result supersession,
auto-continuation/Handoff (validation pre-scripted:
`TAG=c007 /tmp/run-batch2.sh f5-hard-algo`), stale-plan anchor line,
rider persistence, cargo noise shaping.

Fixtures live in /tmp/{f2-csv-comments,f3-semantic-bug,f4-analysis,
f5-hard-algo,f6-multifile-bug} (git repos, PROMPT.txt in each root);
drive scripts /tmp/run-batch.sh and /tmp/run-batch2.sh; per-run artifacts
in /tmp/fixture-logs/. /tmp does not survive a reboot — the fixture specs
are recoverable from this log and PROPOSALS.md.

---

## Implementation session 2026-07-12 — PROPOSALS.md #1–#5 landed

The reboot happened: all /tmp fixtures and drive scripts are gone
(specs recoverable from this log). The model server is back
(qwen/qwen3.5-9b at studio-win.local:8888). Rather than rebuild
fixtures first, the five proposals were designed in a grilling session
and implemented in four sequential agent passes, each gated on
cargo test + clippy:

1. **Rider persistence (#5)** — Entry::Rider logged at injection;
   Resume replays anchors/endgame prompts through the same merge seam
   the live turn uses. Commit 71a5289.
2. **Dead-mass eviction (#1+#2, unified)** — second wave trigger
   (`dead_mass_fraction`, default 0.15) + Supersession classifiers
   (landed write inputs husked; repeated identical run_command/
   read_file results superseded, newest verbatim). ADR-0027.
   Commit 90fae00.
3. **Stale-plan anchor line (#4)** — Ledger plan-recency facts; Anchor
   Governor appends the conditional line when plan exists, passes >
   `plan_stale_after` (default 8), writes since update > 0.
   Commit dc4eb46.
4. **Recovery Turn (#3, both arms)** — eighth Intervention: cap close
   with unverified/failing work opens a Voice-prompted Recovery Turn,
   bounded by `recovery_limit` (default 1). Shapes: Handoff (default,
   compaction-seeded fresh Conversation + final verification verbatim)
   and Continuation. ADR-0028. Commit faf7cb3.

Glossary grew: Dead Mass, Supersession, Recovery Turn, Continuation,
Handoff; Setpoints may now be mechanic-owned; Eviction redefined for
quality-triggered waves. End state: 960 tests green, clippy clean.

**None of these is credited yet.** Every change above is design-
validated only (cycle-006 evidence motivated it; no N=5 has confirmed
it). Next session: rebuild the fixtures (f5-v2 — treat old scorecards
as approximate), re-baseline single-turn at the new config, then run
the c007 protocol as a three-way arm comparison — recovery off vs
Continuation vs Handoff (`SUSPENDERS_RECOVERY_LIMIT=0` /
`SUSPENDERS_RECOVERY_SHAPE`) — and vet dead-mass wave behavior in the
session logs (waves fired? husks imitated? cache churn acceptable?).

---

## 008 — Recovery Turn arm comparison: trigger almost never fired (two holes found and fixed)

**Fixtures rebuilt after the reboot** (specs from LOG.md/PROPOSALS.md):
f4-analysis (tasktrack, 8 tests green), f5-hard-algo v2 (globber, 16
oracle tests incl. backtracking `a*b*c` vs `aXbXbc` and a greedy-trap
`*aa` vs `aaa` — treat pre-reboot f5 scorecards as approximate),
f6-multifile-bug (shopcart, 8 tests green, 100x checkout bug). Smoke:
f6 solved clean in 16 passes/57s at the landed config (9 tests green,
regression test added).

**Protocol:** f5 × 5 runs × three arms on the post-proposals build —
off (`SUSPENDERS_RECOVERY_LIMIT=0`), Continuation, Handoff. Arms ran
concurrently against the one model server from separate fixture copies
and separate `XDG_DATA_HOME`s; two runs died to server-side 500
"Context size has been exceeded" under concurrent KV load (off-run4 at
pass 4, cont-run3's recovery turn) — contention artifacts, not harness
failures; casualties re-run serially.

**Scorecard (green suites):**

| arm | green | recovery fired | end states (red runs) |
|-----|-------|----------------|------------------------|
| off | 0/5 | — | 14/16, 7/16, 15/16, (500 casualty), 13/16 |
| continuation | 0/5 | 1/5 | 15/16, 2/14, 10/16 (T2 died to 500), 5/11, 12/16 |
| handoff | 2/5 | 4/5 | run1 16/16 in 18p single-turn; **run2 capped red → Handoff turn → 16/16 in 14p**; 14/16; 12/16 |

**The vet's real finding — the Recovery Turn's trigger almost never
fires.** All 13 completed T1s ended at the 32-pass cap with a red
suite; recovery fired in only 5. Two holes, both confirmed in code:

1. **Final-pass text settles bypass the judgment.** ADR-0015 strips
   all tools on the final pass, so a capped model settles `end_turn`
   with plain text — and `settle_finish` consulted the recovery
   judgment only on the tool-insistent-markup path. The mechanic's
   designed trigger (`settle_capped`) is nearly unreachable: the
   fires we saw were the runs whose final reply happened to insist on
   tools. The off/cont vs hand firing asymmetry (1/5 vs 4/5) was
   coin-flip tool-insistence, not design.
2. **Filtered-rerun laundering.** `command_failing()` is
   last-command-only; models end capped turns with
   `cargo test one_test_name` (exit 0) after a red full-suite run, so
   the ledger reads green (cont-run1: last command
   `cargo test character_class_range -- --nocapture` after a 15/16
   suite). Cycle-006's exit-code laundering, one level up.

**Tweak (surface: Governor behavior), commit 5a9d6a3:**
`settle_finish` now consults the recovery judgment on the final-pass
text-settle path (the reply enters the Conversation — Handoff seeding
keeps the model's own wrap-up); the judgment's failing arm is now
**Dangling Failure** — any command string whose most recent run this
Turn failed — so a filtered green rerun no longer clears a red suite.
ADR-0028 addendum + CONTEXT.md glossary. 975 tests, clippy clean.
Also landed: Eviction waves now emit `## EVICTION wave:` to headless
stdout — request-time waves were invisible to the vet (found while
building vet.sh; nothing in stdout or the Session Log showed them).

**Evidence the mechanic works when it fires:** hand-run2 converted a
capped 32-pass red T1 into 16/16 green in a 14-pass Handoff turn.
That is the conversion PROPOSALS.md #3 predicted.

**Not credited:** c008 is an invalid arm comparison (the arms differed
by trigger luck, not shape). c009 re-runs cont/hand at the fixed
trigger; off keeps the c008 baseline (recovery-disabled behavior is
unchanged by the fix) with the 500-casualty run re-run.

---

## 009 — Recovery Turn arm comparison at the fixed trigger: CREDITED, Handoff confirmed as default

**Fixture:** f5-hard-algo v2. **Config:** commit 5a9d6a3 (fixed
trigger + Dangling Failure). Arms as in 008; off baseline carried
from c008 with the 500 casualty re-run serially (13/16 at cap —
off stays 0/5).

**N=5 per arm:**

| arm | green | trigger | detail |
|-----|-------|---------|--------|
| off (c008+redo) | 0/5 | — | 14/16, 7/16, 15/16, 13/16, 13/16 — every run capped red |
| continuation | 3/5 | 4/4 capped-red T1s | conversions: 20p, 18p; non-conversions: T2 capped at 14/16, 15/16; plus one clean single-turn 16/16 in 31p (recovery rightly not consulted) |
| handoff | 4/5 | 5/5 capped-red T1s | conversions: 15p, **8p**, 28p, **5p**; one T2 capped at 15/16 |

**Verdict: CREDITED.** Two separable claims, both proven:
1. **The trigger fix (008) cleared its failure mode 9/9** — every
   capped-red T1 across both arms opened a Recovery Turn (c008: 5/13).
2. **The Recovery Turn converts, and Handoff is the right default
   shape.** f5-v2 goes 0/5 → 4/5 with a single Handoff turn. Handoff
   conversions are also cheap — two finished in ≤8 passes, where
   Continuation needed 18–20 and failed twice by plateauing in its own
   bloated context. Fresh-context restart with the compaction skeleton
   beats continuing in place, as PROPOSALS.md #3 predicted; the shipped
   default (`recovery_shape=handoff`) stands.

**Dead-mass vet (008's open item):** waves fire and are visible via
the new stdout event; steady state elides ~32% of the budget per
request view, dominated by husked edit inputs (10+ per run) — exactly
the dead mass PROPOSALS.md #1 targeted. No husk-marker imitation in
any c009 run's assistant text (the ADR-0027 risk). Reading note for
future vets: the printed counts are cumulative view-state per request
(the stored Conversation is never rewritten; `for_request` elides a
clone), so "a wave every pass" past the 15% threshold is the designed
request-time behavior, not churn — an entry elides once and stays
elided in every later view.

**Residual failure mode (the next front):** the plateau — three T2s
(cont 14/16, 15/16; hand 15/16) burned a full extra 32 passes without
closing the last one-or-two oracles. The model converts when its
fresh start finds a sound design fast (5–8 passes) and plateaus when
it re-enters the same debugging tunnel. Candidates, in surface order:
Voice (failure-nudge escalation when the same tests fail across many
passes), Setpoints (`recovery_limit=2` — a second Handoff is another
fresh draw at ~5–28 passes), or fixture-audit first (which oracle
stalls the plateau runs, and is the miss conceptual or mechanical).

**Regression batches at the credited config:** f6 5/5 green (11–17
passes, regression test every run), f4 5/5 delivered ANALYSIS.md
without touching source. A stricter frontier audit of the f4 analyses
than cycle 005's: zero invented functions/modules across all runs,
sorting located correctly 5/5, trace order right in 4/5 — but 3/5
runs fabricate line-number citations (~90% of cited numbers wrong)
and three wrong-caller claims appeared. Parked as its own cycle:
the model cites `file.rs:NN` it can never have seen.

---

## 010 — Voice grounding-first failure nudge: kept (behavior moved, green rate didn't)

**Fixture:** f5-hard-algo v2, default config (Handoff, limit 1).

**Diagnosis (plateau audit of c009-cont-1/4, hand-3, all three
MECHANICAL misses in bracket-class parsing while the memorized `*`
backtracking passed everywhere):** the model debugs its mental model,
not the file — it traces its *intended* code, edits a dead copy of a
function while the live inlined copy executes (hand-3), and when a
debug print fails to appear it concludes "file corruption" instead of
re-reading (cont-1, hand-3). The c005 strategy nudge asks for a trace
but not for grounding, so it changed activity, not outcome.

**Tweak (surface: Voice):** the CommandError-dominant failure nudge
now opens "Stop editing. Re-read the function you are changing with
read_file - after several edits your memory of it is stale and the
file on disk is the only truth." and closes "If a debug print does
not show up in the output, the code you edited is not the code that
runs - find what actually executes before editing again."

**N=5:** 4/5 green — conversions in 7, 23, 28, 12 passes; one T2
plateau at 15/16. Two server-500 casualties (concurrent batches)
re-run serially per protocol; **all batches run serially from now
on** — even two concurrent runs can trip the server's KV pool late
in deep turns.

**Verdict: kept, not credited as an improvement.** Green rate matches
c009-hand (4/5); N=5 cannot resolve a delta. But the prescription
demonstrably lands — in 5 of 6 sampled firings the model's next call
after the nudge is read_file — and the wording encodes a durable
truth about how this model fails. Harmless, plausibly right, kept.

**Capability scorecard at commit 5a9d6a3 + this tweak:** f4 analysis
5/5 (line-number fabrication parked), f6 multi-file bug fix 5/5,
f5 hard implementation 8/10 across c009-hand+c010 with the Recovery
trigger 14/14 on capped-red turns.

---

## 011 — Voice rule: never cite a line number you didn't see — CREDITED

**Fixture:** f4-analysis. **Diagnosis:** read_file returns raw content
with no line numbers, so every `file.rs:NN` the model writes is
structurally a guess — the c009 audit found 3/5 runs fabricating ~90%
of their citations. Tool-shape alternative (numbering read_file lines)
rejected: a 9B would paste numbered lines into edit_file old_str.

**Tweak (surface: Voice):** system-prompt rule — "When you refer to
code, name the file and the function - never a line number. You do
not see line numbers, so any line number you write is made up.
Quoting a line number printed by a compiler or test error is fine."
The carve-out keeps the cycle-002 over-reading failure at bay.

**N=5, same strict frontier audit as c009:** citations dropped 24+ →
1 (run4's "line 39", still wrong); zero invented functions, zero
wrong-caller claims, trace order correct 5/5, sorting placed
correctly 5/5. False claims per run: 0/1/1/2/1 (was ~12/4/1/8/9).
One fully-clean PASS (was zero). **Credited.**

**Residual (capability floor, not guidance):** 3/5 runs claim
`next_id` "returns 0 if empty" — the code is `max().unwrap_or(0) + 1`.
Pattern-matching `unwrap_or(0)` without applying the `+1` is an
arithmetic-comprehension slip; no prompt rule cheaply fixes it.
Parked unless a cycle finds a mechanical angle.

---

## 012 — New fixture f7 baseline: hard-implementation does NOT generalize yet

**Fixture (new):** `f7-hard-algo-2` — run-length + dictionary text
decoder (`N(...)` repeats with nesting, `!k=V;` definitions, `&k;`
substitution, six escapes, seven DecodeError variants) from a stub
against 18 oracle tests; reference implementation verified 18/18
before stubbing. Deliberately different in shape from f5's glob
matcher: stateful bindings + error taxonomy, not just index-walking.

**Config:** c011 config (Handoff default). **N=5:** 0/5 green, and
only 3 valid runs — run1 7/18 (T2 capped), run2 9/18 (T2 capped),
run3 ended with an INFINITE LOOP in decode (oracle tests hung; the
drive script's `timeout 180` caught it — vet lesson: hangs are an end
state cargo alone won't show), run4 2/18 (T2 killed at pass 27 by a
transport error as the model server degraded), run5 invalid (server
gone — the host stopped resolving mid-batch, cycle-007 style).

**Verdict: f5's 8/10 does not transfer to f7 — this is the new open
front.** The Recovery Turn fired every time (3/3 valid capped-red
T1s) but conversions that took 5–28 passes on f5 plateau far from
green here (7–9 of 18). The gap between "memorized recursion shape"
(f5's `*` backtracking passed everywhere) and "novel stateful spec"
(definitions-inside-repeats, error precedence) is the capability
boundary to tune against next. Forensic audit of runs 1–3 queued;
candidates after diagnosis: Plan-quality Governor (does the model
decompose the spec before coding?), Voice on spec-reading, or a
smaller-oracle-first strategy nudge (make the simplest test pass
before the integration case).
