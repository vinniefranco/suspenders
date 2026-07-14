# A malformed-tool-call generation is re-drawn in-band, not failed

The Turn loop maps every `StopReason::Error` to `finish::fail`
(`turn/loop_.rs`, the `dispatch` Error arm) — a terminal `Failed`
settle. Because a failure is not a Turn-Limit close, the Endgame never
judges it: no Verification Pass, no final Pass, no Recovery Turn. One bad
generation discards the whole Turn's work.

But not every `StopReason::Error` is fatal. The local server emits an SSE
`error` event carrying `Failed to generate a valid tool call` — a
constrained-decoding miss (`llm/stream.rs` sets `error`, the response
carries `StopReason::Error`). At the default temperature 0.7 a re-draw
usually succeeds: the failure is a stochastic sampler transient, not a
deterministic dead end. Observed live killing 2 of 4 f7 runs outright
(LOG.md c013 parked insight → c014 recurrence → c015).

ADR-0009 set the precedent: a truncated tool call continues *in-band,
bounded by the Turn Limit*, rather than ending the Turn. The same
principle applies here — the difference is there is no batch to answer
(no `tool_use` blocks were produced), so the fix is to re-draw the
generation, not to answer it with an error result.

## Decision

A `StopReason::Error` whose error string is classified **retryable** —
a conservative allowlist, the malformed-tool-call class only — re-issues
the SAME request from the SAME Conversation. A re-draw, bounded by a
per-Turn retry budget Setpoint (`malformed_retry_budget`, default 3; 0
disables). The Pass is not advanced and the Conversation is not mutated:
the failed draw produced nothing to keep and nothing for the model to
correct, so the retry is **silent to the model's Conversation**. On
budget exhaustion the existing `finish::fail` runs — the loud failure is
preserved, only deferred.

Every re-draw is **durable and visible**: a `retry` Session Log entry
(the classified error and attempt N of the budget) and a Transcript info
line. Silent to the model, never silent to the operator — and the budget
is a hard ceiling the loop can only decrement, so it cannot spin. This is
the explicit answer to the dead-loop concern: a silent *and* unlogged
retry is rejected outright; a bounded, logged one is not.

Classification is **fail-loud by default**: only the known transient
class retries. Transport errors and `Context size has been exceeded`
(the KV-pool 500) fail loud exactly as today — retrying them is pointless
or a budget problem, not a generation one. The classifier mirrors the
existing string-based `failure_category::classify`.

## Considered options

- **Nudge-and-continue (the ADR-0009 shape)** — append a Voice "re-issue
  your tool call" and advance a Pass. Rejected as the default: a
  constrained-decoding miss is a sampler transient, not a reasoning error
  the model needs to see; nudging costs a Pass, pollutes the context, and
  assumes a self-correction the failure mode does not reflect. Left as a
  possible escalation if the re-draw proves insufficient at N — not built.
- **Per-Pass retry cap instead of a per-Turn budget** — bounded only by
  `turn_limit × cap` (up to ~64 draws). Technically safe, but a per-Turn
  budget is a small absolute ceiling and the tighter dead-loop guard;
  chosen for reassurance and simplicity.
- **Route the error through the settle path so Recovery can act** —
  rejected: Recovery is cap-shaped (it needs a Turn-Limit close and the
  Ledger's unfinished-work facts). A mid-Turn generation error is neither.
  A bounded re-draw fits the failure mode; a Recovery Turn does not.

## Consequences

- The fault model gains a third path beside settle and fail (ADR-0018):
  a bounded in-band re-draw for one transient error class. It lives in
  the loop's `dispatch`, not a Governor — it is a mechanical response to
  a wire event and carries no trajectory judgment.
- A new Setpoint (`malformed_retry_budget`, default 3) resolved once per
  Session, overridable via `SUSPENDERS_MALFORMED_RETRY_BUDGET`; a new
  `retry` Session Log entry. The retry count is transient (a resumed Turn
  settles as failed per Resume rules regardless); the log entry is
  forensic, folded but not restored as state.
- Worst case per Turn: `budget` extra model calls, each logged, then the
  same loud failure as today.
- Validation pending: drive f7 (where it recurs) at N≥3 and confirm the
  turn-error deaths clear without introducing a visible retry loop.
