# The next-speaker check: auto-continue a no-tool-call Pass

The plain ReAct loop (ADR-0045) ends a Run the moment the model returns a
response with no tool calls. Driving qwen-code against the same local
Qwen model exposed the cost of that literal rule: a reasoning model routinely
ends a Pass with only a thinking block (dropped from the final content, so the
Pass looks empty) or with a plain-text reply that announces a next action it
did not execute - "Next, I will read the config." With the literal rule the Run
ends there and the user must type "continue" to make the model do the thing it
just said it would do. This was the #1 user pain, reproduced and validated
against the user's real llama.cpp endpoint.

qwen-code does not end the Run on tool-call absence alone. When a Pass carries
no tool calls it runs a cheap `checkNextSpeaker` (nextSpeakerChecker.ts): given
only the model's last reply, who should logically speak next - the user or the
model? A reply that announces an immediate next action, or that was cut off
mid-thought, means the model should continue; a reply that asks the user a
question or completes a thought means the user speaks next. On a `model` verdict
qwen-code injects `"Please continue."` and loops; on a `user` verdict it ends
the Run. We adopt this check.

## Decision

A no-tool-call Pass consults a next-speaker check before finishing the Run.
The check lives in `src/run/next_speaker.rs` and is a faithful port of
qwen-code's `checkNextSpeaker`.

### The tool-call PRESENCE inversion comes first

Before the check matters, the loop's continuation predicate is corrected to
qwen-code's: a response continues the loop iff it carries at least one tool-use
block, INDEPENDENT of the stop reason (`end_turn`, `tool_use`, `max_tokens`,
`unknown` all continue when a tool-use block is present). The old gate on
`stop_reason == ToolUse` is gone. A `max_tokens` stop with tool-use still takes
the truncated-batch re-issue (ADR-0009); a `max_tokens` stop with NO tool-use is
a truncation and finishes with the truncation marker, NOT through the check - a
cut-off reply must not auto-continue.

### The check itself

Two paths, for the case where a Pass completed normally with no tool calls:

- **Short-circuit (no model call).** If the reply carries nothing speakable -
  zero content blocks, or only thinking blocks (which never enter the
  Conversation) - the model plainly produced nothing to hand back, so the
  verdict is `model` with no request spent. This alone fixes the observed
  empty/thinking-only death cheaply.
- **Side-query (one cheap model call).** Otherwise a transient `LlmRequest`
  carrying `[assistant(the reply), user(CHECK_PROMPT)]` - NO tools, empty
  system, thinking DISABLED (`with_no_think(true)`) - goes through the same
  `deps.complete` seam. Thinking must be off: a reasoning model with thinking on
  spends its whole budget reasoning and never answers (validated against the
  user's endpoint). The reply text is parsed leniently for
  `"next_speaker": "user" | "model"`. An unparseable reply defaults to `user`
  (END the Run) - a bad parse must never risk an infinite loop.

The side-query is a genuine side-query: it builds its own request and never
mutates the main Conversation, so nothing it says is checkpointed, logged, or
streamed to the operator (a no-op stream sink).

The `CHECK_PROMPT` is qwen-code's verbatim, with a final line pinning the reply
to a bare JSON object (`{"next_speaker": "user"}` / `{"next_speaker": "model"}`)
in place of qwen-code's forced response schema, which Suspenders has no
side-query channel for.

### The continuation

On a `model` verdict the loop appends the reply as an assistant message stamped
with the Model's Provenance (a thinking-only/empty reply contributes no block),
then appends a `"Please continue."` user message - a Voice-owned string
(`voice::please_continue()`), unstamped, entering the Conversation as an ordinary
user turn - emits a delivered-steering event, checkpoints, and advances the turn
counter. On a `user` verdict the Run finishes exactly as before.

The continuation is bounded by `max_turns`: the loop's top-of-loop
`turn > max_turns` guard runs before the next request, so a model that keeps
producing no-tool replies loops at most `max_turns` times before the Run closes
on the run-limit marker. No new bound is introduced.

### The skip flag

`skip_next_speaker` (session fact, `SUSPENDERS_SKIP_NEXT_SPEAKER`, file
`skip_next_speaker`, default `true` = the check is skipped) mirrors qwen-code's
`getSkipNextSpeakerCheck`, which defaults to `true`. When `true` a no-tool-call
Pass finishes immediately with no check and no side-query - the pre-check
behavior. When `false` the check runs (the mechanic this ADR describes). The
test config (`SessionConfig::test_defaults`) also has it `true` so the loop and
agent tests exercise the tool loop without a side-query firing on every text
reply; the check's own behavior is covered by the tests that opt back in with
`skip_next_speaker: Some(false)`.

## Consequences

- The `"Please continue."` nudge is the ONE piece of harness-authored
  mid-Conversation text ADR-0045 otherwise forbade. It is admitted deliberately:
  it is not corrective steering (it does not lecture or re-state the goal), it
  fires only on the model's OWN judged intent to continue, and it is qwen-code's
  proven shape.
- Every normal no-tool reply now costs one extra cheap completion (the
  side-query), unless it short-circuits. The empty/thinking-only case - the most
  common failure - costs nothing. The side-query runs with thinking off and a
  tiny expected output, so it is bounded even though `max_tokens` is not
  per-request overridable in `LlmRequest` today (it comes from the Model);
  `no_think` keeps the reply short regardless.
- Preserved unchanged: the tool-call path (chained tool calls never consult the
  check), the loop-detector (ADR-0045), the error algebra (ADR-0002), the
  truncated-batch and re-draw paths (ADR-0009, ADR-0030), and the streaming and
  provenance invariants (ADR-0025, ADR-0037).
