# A plain ReAct loop and a strong static prompt, tuned for small local models

We drove qwen-code (Qwen's fork of Google's Gemini CLI) against Qwen3-Coder on
a local inference server and it was markedly more productive than Suspenders on
the same model class: it completed a from-scratch Game of Life in one session
with roughly one failed tool call out of forty-odd, where Suspenders stalled,
looped, and mis-fired tool calls on the same task. Suspenders had accumulated a
large steering apparatus meant to help a small model along - and the simpler
harness won. This ADR records why, and the decision to adopt qwen-code's shape.

## Why the simpler harness is more productive with a small model

The gap came from five things, only the first of which is a bug. The rest are
the point.

1. **Meet the model where it emits.** Qwen3-Coder does not reliably fill the
   provider's structured `tool_calls` channel; it emits tool calls as
   Hermes/Qwen XML text in the content stream
   (`<tool_call><function=NAME><parameter=KEY>...`). qwen-code parses that.
   Suspenders' OpenAI-completions adapter only read the structured channel, so
   a well-formed tool call arrived as prose and was never executed - the model
   looked like it was failing ~40% of its calls when it was in fact calling
   correctly. This single mismatch accounted for most of the observed gap. A
   harness for a model must speak the dialect that model actually speaks.

2. **A clean context beats corrective nudging.** Injecting harness-authored
   corrective text to steer a drifting model backfires on a 9B model. Every
   injected token is attention-tax on a model whose effective attention is
   already the scarce resource (ADR-0013), and injected imperatives are
   themselves a pattern the model imitates, pulling it further off task - and
   each one invalidates the prompt cache. qwen-code injects nothing. It trusts
   one strong static system prompt and lets the model's own trajectory run,
   intervening only to terminate a genuine runaway loop - and even then it
   terminates silently, it does not lecture.

3. **Show, don't tell.** Small models pattern-match far more than they reason
   from rules. qwen-code's prompt carries Core Mandates and a concrete
   Understand -> Plan -> Implement -> Verify workflow, but its real teaching is
   a handful of worked examples that demonstrate the exact rhythm of good tool
   use. For a small model, one worked example outweighs a paragraph of rules.

4. **Predictability is a feature.** A harness that makes local decisions the
   model experiences as the ground shifting underfoot - tools offered one Pass
   vanishing the next, unrequested instructions appearing mid-task - defeats a
   model with shaky state-tracking. qwen-code's loop is boringly uniform - same
   system prompt, same full tool set, every turn, until the model stops calling
   tools. Uniformity is itself a productivity feature.

5. **Use the tools the model was trained on.** Qwen3-Coder has seen
   `todo_write`, `glob`, `read_file`, `run_shell_command` and their shapes in
   training. Naming and shaping our tools to match lets the model call them
   without translation; a bespoke novel tool costs the model fluency.

The through-line: a small local model is a fixed, somewhat fragile collaborator.
Productivity comes from removing friction between it and the work - speaking its
dialect, keeping its context clean and predictable, and showing it the shape of
good behavior - not from a control layer that tries to correct it mid-thought.

## Decision

Adopt qwen-code's shape. Concretely:

### The loop is a plain ReAct loop

Each turn builds one request (the static system prompt, the full Conversation,
the full tool registry - no per-Pass narrowing), streams the response, executes
any tool calls, appends the results, and repeats. Continuation is gated on
tool-call PRESENCE, not the stop reason: any response carrying a tool-use block
continues the loop. When a turn returns NO tool calls the loop does not end
outright - it consults the next-speaker check (ADR-0043), which may inject
`"Please continue."` and continue, or end the Run. A `max_turns` bound
(default 100, generous enough for a real multi-step task) backstops the whole
loop, the next-speaker continuation included. There is no per-Pass steering
machinery of any kind.

### The OpenAI-completions adapter recovers text-emitted tool calls

When the structured `tool_calls` channel is empty, the adapter parses the
content text for Hermes/Qwen XML (and the JSON-in-tags variant) and promotes it
to real Tool Calls, forcing `stop_reason` to ToolUse. Structured calls always
win; text parsing is the fallback. A `SUSPENDERS_TOOL_CALL_STYLE` knob
(`auto`/`structured`/`text`) and `top_p`/`top_k` sampling round out the
small-model fit.

### The system prompt is strong, static, and example-bearing

Core Mandates, an Understand -> Plan -> Implement -> Verify workflow, a Tone
section, and worked examples - adapted to our tool names. The battle-tuned
Suspenders rules that still hold (never fabricate tool output; name the file and
function, never a line number; fix the code under test, not the tests; grow new
work in verified increments; run commands whole with quiet flags) are folded in.
The Voice authors no mid-Conversation steering.

One narrow exception: **Plan Mode** (ADR-0067) injects a standing read-only
reminder into every request while the user is in that explicit mode. That is
not reactive steering - it is not corrective text injected INTO the
Conversation in response to model drift; it is a mode invariant injected at
request-shaping, ephemeral (never stored), and gated on a mode the user
deliberately entered. qwen re-injects it every turn for the same reason a small
model needs the standing rule to hold; suspenders matches that.

### The only runtime intervention is a passive loop-detector

A circuit breaker terminates the Run when the model emits the byte-identical
tool-call batch `loop_stall_limit` times (default 5). It injects nothing into
the context - it ends the Run with a close marker and emits an Event. One
silent safety, nothing else.

### Context is managed by Compaction alone

`for_request` is a pure fit-check; when the Conversation exceeds the budget,
Compaction (the LLM-summary path, ADR-0012) reclaims it. There is no mechanical
elision path beside it. The dense-small budget (ADR-0013) stands.

### The tool set matches qwen-code

`read_file`, `write_file`, `edit`, `run_shell_command`, `grep_search`, `glob`,
`list_directory`, `web_fetch`, and `todo_write` (a structured task list
replacing a freeform plan) - qwen's wire names, with descriptions in qwen-code's
concrete, guidance-rich style. The model explores inline with
`grep_search`/`glob`/`list_directory`/`read_file`. Later ADRs (0054-0068) grow
the registry to qwen's fuller tool surface on the same principle.

## Consequences

- The teardown deleted the steering apparatus and its ADRs - 0006 (eviction
  hysteresis), 0014 (Scout), 0015 (final pass), 0016 (verification pass), 0026
  (Governors), 0027 (dead mass / eviction / supersession), 0028 (recovery Run),
  and 0035 (offered-tools enforcement) - and revised ADR-0012/ADR-0013 to drop
  their Eviction framing. (The 0043 number is now the next-speaker check, which
  refines this loop's no-tool-call ending rather than adding machinery.)
- A stuck small model receives no corrective text; the loop-detector terminates
  it instead. This is deliberate - the wager is that a clean context plus a good
  prompt keeps the model out of the ditch more often than corrective text would
  pull it out.
- Inline exploration grows the main context faster than a delegated search
  would; Compaction absorbs it (ADR-0012).
- Preserved unchanged: the error algebra (ADR-0002), truncated-batch handling
  (ADR-0009), malformed-tool-call re-draw (ADR-0030), provenance and
  orphaned-call answering (ADR-0037), live streaming (ADR-0025), the
  single-owner Agent (ADR-0017), Compaction (ADR-0012), the provider/Api-adapter
  abstraction (ADR-0037), and the whole display pipeline.
