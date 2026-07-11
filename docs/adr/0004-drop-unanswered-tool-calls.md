# Unanswered Tool Calls are dropped from the Conversation

> Amended by ADR-0009: the truncation path now answers Tool Calls with an error Tool Result instead of dropping them. The drop rule below applies where no retry Pass exists to answer the call: Cancellation, and responses that ended in a stream error (the Turn settles as failed).

Two paths can leave a `tool_use` block without a paired `tool_result`: max_tokens truncation mid-block, and Cancellation while calls are in flight. An unpaired `tool_use` corrupts the Conversation - strict Anthropic-compatible servers reject the next request, and sloppy ones confuse small models. We resolve both paths with one rule: a Tool Call whose Tool Result never materialized is stripped from the assistant message before it enters the Conversation. The pairing invariant ("every tool_use is followed by its tool_result") holds by construction, never by repair. The Transcript still shows the dropped call; only the Conversation omits it.

Considered and rejected:

- **Fabricate an error Tool Result** (`[truncated]` / `[cancelled by user]`). Keeps the full record, but CONTEXT.md is explicit that no Tool Result is fabricated on Cancellation, and small models treat fabricated errors as failures to retry.
- **Fail the Turn.** Loud, but discards good text the model produced before the cut, and punishes the user for a tuning problem (max_tokens) or their own Escape key.

Consequence: code in the Turn/Agent layer deliberately deletes blocks the model emitted. That is not a bug; do not "fix" it by passing them through.
