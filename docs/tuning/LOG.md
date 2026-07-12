# Tuning Log

The drive-vet-tune loop (CONTEXT.md: the loop ADR-0026 formalized the *output*
of; this is its *drive* and *vet* halves). One entry per tuning cycle: the
fixture, the observed failure, the tweak, the N=5 confirmation, the verdict.

The loop:
- **Drive** - fixture in `/tmp` as a git repo; run headless.
- **Vet** - stdout + Session Log + `git diff` + frontier-grade judgment;
  "did it finish / burn turns / was the code good."
- **Tune** - whatever surface the vet points at: Setpoints, Voice, Anchor
  structure, Tool shape, Governor behavior.
- **Repeat** - N=5 before a tweak is credited; `git reset --hard && git
  clean -fd` between runs. Tuning is guidance, not muscle (no sampling sweeps).

A tweak is credited only when the same failure mode clears across five runs at
the same config. A single run is a coin flip on a stochastic 9B; the N=5 rule
exists to stop phantom-chasing.

## Surfaces (in priority order, not a menu)

1. **Setpoints** - cadences and thresholds (turn_limit, anchor_interval,
   nudge-from-passes, explore-every, stuck recency, eviction_slack,
   compaction_keep, no_think_rescue).
2. **Voice** - the wording of every Suspenders-voiced string (system prompt,
   nudge prose, markers, Anchor framing). Highest leverage per voice.rs.
3. **Anchor structure** - what the Anchor carries beyond Plan+task.
4. **Tool shape** - a tool's schema, its own error/result wording, Result-Cap
   cut behavior.
5. **Governor behavior** - new triggers, new kicks, new Intervention kinds
   (a new Intervention *kind* is a visible design decision per ADR-0026).

---

## 001 - Voice Verify step: "write a test for new behavior"

**Fixture:** `f2-csv-comments` - a small Rust CSV parser (7 tests green) asked
to add comment-line support (`#`-prefixed lines skipped). Fuzzy, no oracle:
the prompt deliberately did not mention testing or verifying, to stress
self-verification.

**Observed failure (baseline, no tweak):** the model finished in 9 passes,
declared done, ran `cargo test` (existing 7 passed) - but wrote **zero** tests
for the new behavior. The implementation was catastrophically broken
(`parse("# comment\nx,y\n")` → `[["# a comment","x","y",""]]` - the comment
was not skipped and the data row was merged into it). The model declared a
broken, untested feature complete.

**Diagnosis (guidance, not capability):** the system prompt's Verify step
said only "run the tests or the compiler." The model read existing-green as
verification. The Verify Governor's trigger ("writes unverified by
`run_command`") was satisfied - `cargo test` ran - but the tests did not
cover the new code. "Verified" ≠ "covered." See the parked insight below.

**Tweak (surface: Voice):** two surgical edits to `SYSTEM_PROMPT` in
`voice.rs`:
- Verify step: appended "If you added new behavior, write a test that
  exercises it and run the full suite - existing tests passing alone does not
  confirm new code works."
- Rules: appended to the "fix code under test" rule - "Adding new tests for
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

**Verdict: partial win - the targeted failure cleared, a new failure surfaced.**

- **False-completion (the targeted failure): cleared 5/5.** Every run wrote
  new tests (11–15 total vs baseline 7). No run declared broken code done.
  The Voice tweak did its job.
- **Budget-convergence (newly exposed): 3/5 clean, 4/5 a near-miss, 1 hard
  fail.** The model now does the right thing (writes tests, iterates to
  correct) but on a 9B that iteration is expensive - run 3 burned all 25
  passes and left 2 failing tests; run 4 only finished green by luck of the
  cap.

**Why N=5 mattered:** a single tweaked run (run 1, 22 passes, clean) would
have looked like an unqualified win. Five runs reveal a real improvement that
*shifted* the failure, not a clean fix. Shipping on one run would have hidden
a 1-in-5 hard failure and a 1-in-5 near-miss.

**Next (cycle 002):** the rework bleed. Watching the logs, the model is
inefficient in its debug loop - it re-reads whole files repeatedly and
rewrites the entire `parse` function on each edit instead of surgical edits,
despite a system-prompt rule already saying "keep edits minimal." The turn
budget bleeds on rework. Cheapest lever: Voice - strengthen the edit-minimal
rule into something a 9B actually obeys.

### Parked insight - the Verify coverage gap

The Verify Governor fires when "writes are unverified by `run_command`" -
i.e., no command has run since the last write. But `cargo test` running
satisfies it even when the test suite does not cover the new code. "Verified"
≠ "covered." This is durable and not yet a decision. It becomes an ADR
candidate only if a real tuning cycle proves that closing the gap (e.g., a
new "untested new code" trigger, or a Voice nudge asking the model to confirm
its tests touch the changed code) measurably helps. Recorded here so it does
not get lost; do not promote to an ADR until a cycle demonstrates it.

---

## 002 - Voice "keep edits surgical" - REJECTED

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
- 2 runs (3-4) didn't even attempt the feature - classic false-completion
  from BEFORE the 001 Verify tweak.
- The model likely over-read "surgical" as "don't change anything."
- Hypothesis wrong; revert to 001 config.

---

## 003 - Setpoint: turn_limit 25 → 32 (with Verify-tweak from 001)

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

## 004 - Fixture: semantic bug find+fix+test

**Fixture:** `f3-semantic-bug` - a small string-utils crate (`last_index_of`,
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
Verify-tweak is clearly working - the agent writes tests for new behavior
(even when that behavior is "exposing a hidden bug"). The fixture is
excellent for testing this capability.

**Observations:**
- Runs 2, 4 wrote only 1 new test but still caught the bug (the simplest
  test that exposes the off-by-one was enough)
- Runs 1, 3, 5 wrote 2-3 tests (more thorough edge cases)
- No runs hit the cap - the task is well-scoped for 32 passes
- The prompt explicitly says "write a test that exposes the bug" - this may
  be what drives the test-writing behavior more than the Verify-tweak alone.
  The Verify-tweak is the foundation; the prompt is the immediate trigger.

**Status:** This fixture confirms the current config (Verify-tweak +
turn_limit=32) is working well for the "write tests for new behavior" aspect.
