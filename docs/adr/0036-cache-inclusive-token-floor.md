# The token estimate floors on cache-inclusive API usage; the char estimate stays the Eviction currency

`Conversation::token_estimate` is `ceil(chars / 3.5)` floored by the API's
reported usage. The floor read only `input_tokens`, but per the Anthropic
protocol `input_tokens` is the uncached remainder: the true size of the
previous request is `input_tokens + cache_read_input_tokens +
cache_creation_input_tokens`. Suspenders engineers a warm prompt cache by
design - Eviction's rare waves and the Compaction Keep exist to hold the
request prefix byte-stable (ADR-0006, ADR-0012) - so in steady state the
floor collapsed to the new tail (hundreds of tokens against a six-figure
real context) and the lossy char estimate silently carried every pressure
and Compaction trigger. The bug was invisible precisely when the design
was working.

## Decision

One `Usage` type, one derived fact:

- `content::Usage` carries all four figures the server reports:
  `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
  `cache_creation_input_tokens`. The SSE fold (`parse_usage`,
  `merge_usage`) parses and merges all of them; `message_start` supplies
  the cache figures, present-wins merging already handles that.
- `Usage::context_floor()` is the only reader of the accounting: the sum
  of all four fields, `None` when `input_tokens` is absent (a usage map
  without it is no signal, not a zero floor). Output tokens belong in the
  sum because by the time the floor is consulted the assistant reply has
  been appended to the Conversation and is part of the next prompt.
- `conversation::Usage` and the `usage_of()` mapping are deleted. The
  narrow duplicate existed to state "the Conversation reads one fact of
  the usage"; it dropped `output_tokens` silently at a second seam, and
  the method states the same thing without a place to drop fields. The
  Conversation stores `content::Usage` and reads `context_floor()`.

The floor is a lower bound on `token_estimate` only. The fit check in
`for_request` and every Eviction decision keep the char estimate as their
sole currency: Eviction rewrites the prefix bytes, and the API figure
describes the request as it was before the rewrite, so it cannot measure
what an elision saved.

## Considered options

- **pi's decomposition** (prefix from API usage plus a char-estimated
  tail). Rejected: it makes the API figure a currency Eviction must
  transact in, and that figure cannot see Eviction's effect (above). The
  `.max()` floor gets the accuracy where it matters - trigger decisions -
  without splitting the estimate into two regimes.
- **The `count_tokens` endpoint** as the authoritative estimate.
  Rejected: a network round-trip on a check that runs while shaping every
  request.
- **Widening `conversation::Usage` in parallel** (the minimal diff).
  Rejected: two types mirroring four fields is a standing invitation for
  the next field to be dropped at the conversion, which is exactly how
  `output_tokens` was already lost once.

## Consequences

- The status bar's token figure jumps to the truthful number the first
  time a warm-cache response reports usage. The old, smaller number was
  the bug.
- The floor describes the previous request, so it lags one response;
  the char estimate covers the gap, as it always has for the first
  request of a Session and after Resume (usage is not persisted in the
  Session Log).
