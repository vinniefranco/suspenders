# Larger fixes - evidence-backed proposals

Written at the end of the 2026-07-12 tuning session (LOG.md cycles 005–007).
Each item cites the runs that motivate it. None of these is a Setpoint or
Voice tweak; each is a design change big enough to deserve its own cycle -
implement one, prove it on the f5 scorecard (LOG.md 006), then ADR it.

Ordered by expected leverage.

## 1. Tool-call inputs are invisible to Eviction (the biggest one)

**Problem.** Eviction reclaims Tool *Results*, but the assistant's own
`tool_use` blocks stay verbatim forever. On edit-heavy turns those inputs -
`edit_file` old_str/new_str bodies - are the single largest context
consumer, and they are dead weight: once an edit lands, the file on disk is
the truth and the paste-buffer that produced it teaches the model nothing.

**Evidence.** Context-composition audit of f5 baseline run1 (session log
`20260712-032538-1.jsonl`): ~66k chars of tool_use inputs in the final
Conversation - more than all Tool Results combined (~49k), in a turn that
ended 12/16 at the cap. The model was re-reading and re-pasting the same
function while its window filled with its own stale paste history.

**Design.** Extend Eviction to assistant `tool_use` inputs: behind the same
low-water mark, replace an old edit_file input with a valid-JSON husk -
`{"path": "src/lib.rs", "elided": "[edit body elided - the file on disk
holds the result]"}` - keeping the most recent K edits verbatim (the model
may legitimately reference its last change). Same wave timing as today, so
the prefix stays byte-stable between waves and the prompt cache holds.

**Risks.** Rewriting assistant history is a bigger step than rewriting tool
results: the model may imitate the elided shape when composing new calls.
Mitigations: only elide calls that are already behind the eviction mark
(the model imitates the tail, not the middle); keep the husk visibly
non-imitable (bracketed marker text). Validate on f5: uncompilable-at-cap
and plateau runs should convert if the freed window actually buys passes.

## 2. Repeated-command result supersession

**Problem.** A debug loop runs the same verification command many times;
every failing dump stays. The old dumps carry no information the newest one
lacks, and near-identical failure text repeated down the window teaches a
small model that failure text is the register to write in.

**Evidence.** Same audit: four near-identical 2,660-char `cargo test`
failure dumps unevicted in the final third; 11/11 command runs failed.
Signal fraction of cargo output ≈ 40% (the rest is warnings/Compiling
boilerplate, repeated verbatim across ~8 runs).

**Design.** When run_command executes a command string identical to a
previous call in the same Turn, elide the previous result to
`[superseded by a newer run of this command below]` at the moment the new
result lands. Mechanical (correct-or-incorrect, no Setpoint) - a mechanic
in the Eviction family, not a Governor. The same rule fits `read_file` of
an identical path (audit: 18.1k of 20.9k read chars were redundant whole-file
re-reads).

**Risks.** Mid-turn history rewrites invalidate the prompt-cache prefix at
the rewrite point; the append of the new result already does that, so the
marginal cost is near zero. The model occasionally compares old vs new
output - rare for identical commands, and the newest dump survives.

## 3. Auto-continuation at the Turn Limit (or Handoff)

**Problem.** For hard implementation tasks the 32-pass budget, not ability,
is the binding constraint - and a Turn that dies at the cap can end broken
(mid-refactor compile errors) with no recovery opportunity.

**Evidence.** 12 of 15 f5 runs ended at the cap; the failures include
near-misses (15/16, 12/16 twice) that are one honest debugging turn from
green, and two compile-error end states (c006 runs 1–2) that a single
"make it compile again" turn would likely repair. Meanwhile the same model
solved the same fixture in 13 passes when its first design was sound -
variance, not capability.

**Design (two shapes, measure before choosing).**
- *Continuation:* when a Turn settles at its Turn Limit with writes
  unverified or the last verification failing, the harness auto-submits one
  bounded continuation Turn in the same Conversation ("continue until the
  suite passes"), at most once or twice per task.
- *Handoff:* instead of continuing the bloated Conversation, retire it and
  seed a fresh one with the compaction skeleton (task verbatim, Plan -
  which the harness already owns verbatim - files touched, decisions, next
  step). Fresh-context restart with a structured handoff beats in-place
  compaction even on frontier models; on a 9B the gap should be wider.

**Validation already scripted.** `TAG=c007 /tmp/run-batch2.sh f5-hard-algo`
runs the two-turn protocol (task, then "continue until every test passes")
N=5 - cycle 007 was invalidated by the server going down; re-run it first.
If two-turn conversion is strong, Continuation is the cheap win and Handoff
the follow-up comparison (same protocol, fresh session seeded from the
summary instead of a second turn).

## 4. Stale Plan riding every Anchor

**Problem.** The Anchor exists to put the goal where the model attends, but
it carries the Plan *as last written*. A plan set once at pass 5 and never
updated means the tail of a 32-pass turn keeps re-reading an outdated
"Next step" - authoritative-looking wrong guidance, refreshed every 5
passes.

**Evidence.** f5 baseline run1: plan tool called once; all six Anchors
carried the identical stale plan (steps 3–4 unchecked at turn end, "Next
step: implement glob_match" while the model was 20 passes deep in
debugging it).

**Design.** The Turn Ledger already records Tool Calls per Pass; add
passes-since-plan-update as a fact. A Governor (the Anchor Governor is the
natural owner) appends one Voice line to the Anchor when the Plan is older
than N passes with edits since: `[this plan has not changed in N passes -
if it no longer matches reality, update it with the plan tool]`. Setpoint
N, default ~8. Cheap; measurable on f5 (does the model course-correct
instead of tunneling).

## 5. Anchors and endgame riders are not in the Session Log

**Problem.** `agent.rs::log_event` persists the four nudge event types but
not `Rider::Anchor` or the endgame riders - so a Resume reconstructs a
Conversation the model never actually saw (the anchors vanish).

**Evidence.** Found during the run1 audit: six anchors visible in live
behavior, zero in the JSONL; reconstructed only from `anchor_interval`.

**Design.** Log riders as Conversation events like nudges. Pure fidelity
fix (ADR-0010's linearity is preserved); do it before any Resume-dependent
tuning cycle, or Resume experiments will be quietly wrong.

## 6. Cargo output noise shaping (smaller)

41% of the audited cargo output was warnings/Compiling boilerplate repeated
verbatim across runs. A run_command-focused Plugin could strip repeated
warning blocks (same hash as the previous run's) at the after-execution
hook. Deferred: supersession (#2) removes most of the duplication for free;
revisit only if post-#2 audits still show boilerplate crowding.
