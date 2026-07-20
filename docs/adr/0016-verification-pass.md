# The pass before the final one is a Verification Pass when writes are unverified

> Amended by ADR-0035: Tool Calls outside the Verification Pass's run_command-only offer no longer run - they are refused at dispatch with an error Tool Result. The tolerance stated below ("gets them executed") is reversed.

Live driving (2026-07-10, Qwen3.5-9B, scout-edit fixture) exposed a hole
in the verification pressure: the finish-gate Nudges (Verify,
Verify-failed) only fire when the model VOLUNTARILY finishes. A model that
makes Tool Calls every Pass until the Turn Limit is never verify-nudged,
and by the forced final Pass (ADR-0015) Tools are withdrawn - it CANNOT
verify. Both failure runs ended exactly there: one finished unverified at
Pass 17 on a parroted marker, the other rode all 25 Passes editing (docs
included) and never ran the tests; both left the suite red while looking
settled.

## Decision

When the Pass before the final one arrives with unverified writes (at
least one successful file edit or write with no command run since), that
Pass offers the command-running Tool ONLY. The tail of the Pass before it
carries the Verification Pass prompt in place of the wrap-up warning,
which it subsumes (verify now; the final Pass concludes). The narrowed
Tool list is the enforcement - the same mechanics-over-prose lesson as
ADR-0014's forced report Pass and ADR-0015's tool-less final Pass.

A verified or write-free Turn is untouched: ordinary wrap-up warning, full
Tool list, ADR-0015 Endgame as before.

## Consequences

- The Endgame for an unverified Turn is warning-with-verify-prompt at
  limit - 2, command-only at limit - 1, tool-less conclusion at the limit.
  The final-Pass prompt still asks "whether your changes are verified",
  and the model now has an answer grounded in a real command.
- The unverified state cannot drift between the prompt and the narrowed
  request - no Tool runs in between. A verify-nudged path that skips the
  Tool Results tail still narrows when the request is built; the Verify
  Nudge's own wording ("run the tests or compile") is the prompt in that
  path.
- A model that answers the Verification Pass with other Tools anyway gets
  them executed (same tolerance as final-Pass tool insistence) and the
  ADR-0015 Endgame proceeds; one that answers in text finishes through the
  ordinary gates, where the Verify Nudge (re-armed on progress) still
  owes.
- One editing Pass per unverified capped Turn is traded for a
  verification. Driving evidence says that Pass was not saving those
  Turns - both failures spent their late Passes on doc-coherence edits
  while the suite sat red.
