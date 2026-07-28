# The Run Limit ends with a forced final Pass

> Amended by ADR-0035: real Tool Calls emitted on the final Pass no longer run - they are refused at dispatch with an error Tool Result, and the Run closes on the turn-limit marker. The tolerance stated below ("those Tools run") is reversed. The text-markup detection amendment is unaffected: serialized markup was never executed, only detected.

Live driving (2026-07-10, Qwen3.5-9B) showed a repeating failure at the
Run Limit: open-ended tasks (evaluate a project, add a feature
tests-first) burn all 25 Passes exploring or debugging and settle at the
turn-limit stop reason with no answer delivered - the user gets a marker,
not a conclusion. 3 of 4 long Runs ended this way; the one success
delivered its conclusion on exactly Pass 25.

A prose warning alone does not fix it. The wrap-up warning (one-shot at
limit - 2) fired correctly and was ignored 2 of 2 times - the model kept
debugging mid-thought until cut off. The same lesson was already learned
twice at other scales: the Explore Nudge needed a mechanical classifier,
and the Scout needed a forced report Pass (ADR-0014) before behavior
actually changed. Small models comply with mechanics, not requests.

## Decisions

### The last permitted Pass offers no Tools

When the Pass reaches the Run Limit, the request carries an empty Tool
list, and the Tool Results message one Pass earlier carries the final-Pass
prompt: Tools are withdrawn, state what was accomplished, what remains
undone, and whether changes are verified. The only move left is the
conclusion. The wrap-up warning stays at limit - 2, so the model gets one
warned Tool Pass before the ending.

A Run that concludes this way ends on the end-turn stop reason with the
model's own status statement as its last message - strictly better than
the marker for the user, the Session Log, and any Run that follows. The
turn-limit / turn-limit-stuck stop reasons and the turn-limit marker
remain for a model that answers the final Pass with Tool Calls anyway
(nothing was offered, but a quirky server can still emit them; those Tools
run and the Run closes exactly as before this ADR).

Amendment (2026-07-10, seen live twice the same day): Tool insistence can
also arrive as TEXT. With no Tools in the request, the server has nothing
to parse against, so the model's call comes through as serialized markup
(`<tool_call>...`) in a plain text block - once leading the response, once
after a one-sentence preamble ("I need to update ...:") - and would settle
as a clean-looking end-turn conclusion. Detection is line-anchored: a
final-Pass reply carrying a line that IS Tool markup closes on the
turn-limit marker path instead, and the markup never enters the
Conversation (kept, it would prime later Runs to emit more of it). The
markup string appearing inline in prose is still a conclusion.

### Interactions left untouched

The finish gates (Verify, Verify-failed), the Empty-response Nudge, and
the no-think rescue all already guard on the Pass being below the Run
Limit, so none of them can loop past the final Pass. An empty final
response closes on the empty marker as before.

## Consequences

- Capped Runs end with a real answer or an explicit "what remains undone"
  statement instead of silence; turn-limit settlements become rare and
  genuinely mean "the model would not stop calling Tools".
- One Tool batch per capped Run is traded for the conclusion. Driving
  evidence says that batch was not saving those Runs (runs 3-4: the
  Pass-25 Tool batches were mid-diagnosis edits that left tests red).
- If a future model reliably obeys prose warnings, the forced Pass simply
  never bites (the model concludes on its own before the limit).
