# Compaction: summarize old messages when the Context Budget runs low

When the Conversation exceeds the Context Budget, building a request fails
with a context-budget-exhausted error - the Run fails and the Session is
dead. Small local models with tight context windows hit this cliff much
sooner than large hosted models.

Compaction is the sole context-reclaim mechanism: when the Conversation
crosses its token target, call the LLM to summarize old Runs into a
structured markdown summary, then replace them in the Conversation. The
summary captures what was accomplished, what decisions were made, and
what files were touched, so the model can continue without the full
verbatim history.

## Decisions

### Compaction is an effect, not a pure fold

The fit-check (`Conversation::for_request`) is purely mechanical and lives
inside the Conversation as a pure function. Compaction requires calling
the LLM to produce a semantic summary, which is an effect. It therefore
lives in its own Compaction module, invoked through a method on the Run's
effect trait (the Deps trait, ADR-0011) by both paths.

> Amended 2026-07-10: the proactive path originally ran on the Agent
> actor at Run start. That put a long LLM summarization call inside the
> Agent's command handler - the first time a live Session actually
> crossed the target, the synchronous summarization blocked the caller
> until it timed out, and the Agent was deaf to cancel/status for the
> whole call. Both paths now run INSIDE the spawned Run task (a tokio
> task), never on the Agent actor, through the same compaction method on
> the Deps trait; the proactive check happens in the Run loop before the
> first Pass. The Agent stays responsive to commands.

The Conversation gains two pure helpers - one to prepare Compaction (find
the cutoff, extract file ops) and one to apply it (replace old messages
with the summary) - but the actual LLM call is owned by the Compaction
module.

The Compaction module manages its own state (the previous summary and
accumulated file ops) so the Agent holds one compaction field instead of
two inline fields plus a closure.

### Two lines of defense

1. **Proactive** (Run start): before the Run's first Pass, check
   whether the Conversation's token estimate already exceeds the
   Compaction Target. If so, compact now rather than risking a cliff
   mid-Run. (Amended 2026-07-10: runs in the Run task, not the Agent,
   see above.)
2. **Reactive** (Run loop): while building a request, when the fit-check
   returns context-budget-exhausted, invoke the Deps compaction method
   and retry. If Compaction also fails, the budget is truly exhausted.

### The keep level is its own knob, decoupled from the trigger

Compaction's trigger (the Compaction Target) and how much recent
Conversation survives a Compaction (the Compaction Keep) are separate
knobs. Sharing one knob had made the post-compaction size sit at the
trigger line, so Compaction re-fired at nearly every Run boundary; the
keep level is now decoupled from the trigger ("fire high, keep low"). See
ADR-0013.

The Compaction Keep is set well below the Compaction Target so
Compactions arrive rarely and each summarizes a large, coherent span of
finished work. A keep of 0.0 gives maximal Compaction - only the reply
reserve survives.

The cut point is always adjusted backward to the nearest run-start user
message (one whose first content block is text), so no Run is split
across the Compaction boundary.

### Silent LLM call: Compaction bypasses the Run loop

The Deps compaction method calls the LLM completion path directly - not
through a Run - so the user never sees a "compacting..." phase in the
Transcript. The Compaction request has no Tools and uses a dedicated
system prompt.

### File tracking across Compactions

The Agent accumulates read files and modified files across Compactions.
Each Compaction extracts file operations from the tool-use blocks in the
messages being compacted and merges them with the previous accumulation.
The merged list is included in the Compaction prompt so the next
Compaction has full file context.

### Session Log interaction

Both Compaction paths (Proactive at Run start and Reactive at the budget
cliff, both via the Deps compaction method in the Run task) write a
compaction entry - summary, skip count, tokens before, file ops, original
task - to the Session Log. The summary is the model's narrative ALONE;
the mechanical facts (file ops, original task) ride as their own elements
so the fold recomposes a byte-identical summary message. On Resume, the
fold discards everything folded before this entry and emits just the
reconstructed summary message, then continues folding normally from the
entries after it. The raw log entries before the Compaction marker
survive in the file for forensic access but are invisible to the fold.

## Consequences

- The Session no longer hits a hard cliff: Compaction buys a
  semantically compressed view of the Conversation that keeps the model
  productive with much less context.
- Compaction is lossy (the summary omits details), but the Session Log
  preserves the full verbatim history for any future analysis or
  navigation.
- Compaction costs one LLM round-trip, but this is far cheaper than
  restarting a dead Session. The Compaction overhead is bounded: the
  summarization prompt feeds at most the Compaction Keep's worth of
  content.
- The Compaction Target (trigger) and Compaction Keep (recency) are
  independent knobs, decoupled so Compactions fire high and keep low: rare,
  deep summaries with long append-only stretches between them.
- Multiple Compactions across a long Session chain their summaries: each
  Compaction's output becomes the previous summary fed into the next
  Compaction's prompt, so the model's context is a telescoping view of
  the Session's history.
