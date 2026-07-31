# Run far below the advertised context window

The target model class (Qwen3.5-9B via a local inference server)
advertises 128k of context, and the obvious move is to set the Context
Budget near it. We deliberately don't: the default Context Budget is 64k
with an 8k reply reserve, a 56k live window.

Three reasons, all properties of small models:

1. **Effective attention is much smaller than the RoPE limit.** A 9B's
   instruction adherence, tool-call fidelity, and recall degrade
   noticeably past ~32k and badly past ~64k. Transcript beyond that point
   is not memory, it is noise the model pays attention-tax on.
2. **Prefill is the latency budget.** A cache-invalidating Compaction
   re-processes the prefix on local hardware. Invalidating a 50k prefix is
   an annoyance; invalidating a 120k prefix is a coffee break per Pass.
3. **KV cache at 128k costs VRAM** that is better spent on a higher
   quality quant of the model itself.

## Decisions

### Dense-small defaults

Context Budget defaults to 64,000 and max tokens to 8,000. Both are
configurable; the numbers encode a philosophy, not a hardware fact.
Occasional max-tokens truncation from the smaller reserve is absorbed by
ADR-0009's re-issue path.

### Fire high, keep low

Compaction's trigger and its keep level are decoupled. A single knob for
both made every post-compaction Conversation sit at the trigger line:
Compaction re-fired at nearly every Run boundary, each time summarizing a
thin slice and paying an LLM call plus a full prefix cache invalidation.
Now the Compaction Target (the trigger) and the Compaction Keep (default
~0.5 of the live window) are independent knobs, so Compactions arrive
rarely, each summarizes a large coherent span, and long append-only
stretches between them keep the server prompt cache warm.

### Downstream sizing follows the window

The Result Cap shrinks from ~1/4 to ~1/16 of the live window (4k-char
floor retained): a single huge Tool Result is an attention sink for a
small model at read time, not just a budget cost at retention time.

## Considered options

Max-window (~120k budget) was rejected for the three reasons above.
Staying at 32k was rejected because it left no room for Compaction to
breathe: the window churned constantly and the summary became
load-bearing memory almost immediately.

## Consequences

- Compaction alone carries the strategy: exploration (grep_search, glob,
  list_directory, read_file) grows the main window inline, and Compaction
  reclaims it when the target is crossed. The model records task state
  through `todo_write`. See CONTEXT.md for the language.
- Thinking stays enabled at every call site (main loop, Compaction),
  uniform behavior over server-dependent toggles; the reserve is sized
  with that cost in mind.
