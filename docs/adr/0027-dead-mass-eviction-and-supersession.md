# Dead-mass Eviction and Supersession: eliding by rot, not just by size

Eviction fired only on Context Budget pressure, but the f5 audit
(docs/tuning/LOG.md 005) showed a Conversation rotting with budget to
spare: ~66k chars of `edit_file` old_str/new_str bodies - dead the
moment each edit landed, the file on disk being the truth - outweighed
every Tool Result combined, and four near-identical failure dumps of
the same verification command sat unevicted, in a window that peaked at
38k of 64k. The run ended 12/16 at the Turn Limit. Context quality, not
overflow - and pressure-gated eviction is dead code against it.

## Decision

Eviction gains a second wave trigger and two classifiers, named in
CONTEXT.md as **Dead Mass** and **Supersession**:

- A wave fires when elidable dead content exceeds `dead_mass_fraction`
  of the Context Budget (Setpoint, default 0.15), even under no budget
  pressure. Waves stay batched and the prefix stays byte-stable between
  them - ADR-0006's hysteresis contract is extended, not weakened.
- Supersession classifies content as dead mechanically, never by
  judgment: a *successful* write's `tool_use` input body is superseded
  by the file on disk (husked to valid JSON keeping the path); older
  results of an identical `(name, input)` `run_command`/`read_file`
  call in the same Turn are superseded by the newest, which survives
  verbatim. A failed edit's input stays until a later successful write
  to the same file supersedes the attempt chain.
- The recency guard extends symmetrically: the paired `tool_use` blocks
  of the last two tool-result exchanges are as untouchable as the
  results themselves.

This is the first rewrite of assistant history - until now only
tool-result user messages and stale Anchors were rewritten. The risk is
the model imitating the husk when composing new calls; mitigations are
that only content behind the recency guard is touched (a small model
imitates the tail, not the middle) and the husk is visibly non-imitable
bracketed marker text.

## Considered options

- **Pressure-gated, as first proposed (PROPOSALS.md #1/#2)**: rejected
  because the motivating runs never crossed the low-water mark - the
  mechanic would provably never have fired on its own evidence.
- **Immediate-on-supersession rewrites**: maximum freshness, rejected
  because a debug loop would rewrite mid-context every Pass, thrashing
  the server prompt cache ADR-0006 exists to protect.
- **Promoting the trigger to a Governor**: rejected - Governors act
  through Interventions, and eliding history is not one and should not
  become one. Instead the CONTEXT.md line was refined: what Eviction
  elides stays correct-or-incorrect, while the cadence of its waves may
  carry tuned thresholds, and a Setpoint may be owned by a named
  mechanic (`dead_mass_fraction` joins `eviction_slack`).

## Consequences

- A dead-mass wave elides all dead content outside the guard; it does
  not stop at the low-water mark - dead content has zero value by
  definition. Budget-pressure waves prefer dead content before eliding
  any live result.
- Validation is a tuning question, not a correctness one: the f5
  scorecard (LOG.md 006) is the benchmark; uncompilable-at-cap and
  plateau runs should convert if the reclaimed window buys real passes.
