# Eviction overshoots to a low-water mark (hysteresis)

We run against local inference servers where prompt prefix caching is the
difference between a snappy Turn and seconds of prefill: the server reuses its
KV cache up to the first token that differs from the previous request, and
recomputes everything after. Eviction rewrites the contents of old Tool
Results, so every elision invalidates the cache from that point on - usually
near the very start of the Conversation.

The original rule ("evict until the estimate fits the target, then stop")
made this a per-request tax: once a Session neared the Context Budget, every
request grew past the target, elided exactly one more old Tool Result, and
paid a near-full re-prefill. On a quantized ~27B model with a six-figure
context window, that is the dominant latency in the loop.

Eviction now overshoots: once triggered, it elides down to a low-water mark,
`target - eviction_slack * context_budget` (config `eviction_slack`, a
fraction, default 0.2). Elisions arrive in rare waves; between waves the
message list is byte-stable except for appends, which is exactly the shape the
prefix cache wants. One wave pays one re-prefill and buys many cheap requests.

## Considered Options

- **Stop at the target (status quo).** Minimal information loss per request,
  maximal cache loss per Session. Rejected: the model can re-run a tool for
  an elided result, but nothing can un-pay a prefill.
- **Evict whole Turns instead of Tool Results.** Coarser waves, but violates
  CONTEXT.md: Eviction targets only Tool Results; the user's instructions in
  past Turns survive.
- **Server-side cache-reuse only.** Some runtimes can salvage chunks after a
  divergence point by KV shifting, and users should enable it - but it is
  runtime-specific, degrades quality slightly (RoPE shift), and is not
  universally available. We keep our own requests cache-friendly regardless of
  the runtime behind the endpoint.

## Consequences

A wave elides Tool Results that would have survived under the minimal rule -
information the model might still have wanted is replaced by the elision
marker sooner. Accepted: the marker tells the model how to get it back
(re-run the tool), while prefill time is unrecoverable. `eviction_slack: 0.0`
restores the old minimal behaviour.
