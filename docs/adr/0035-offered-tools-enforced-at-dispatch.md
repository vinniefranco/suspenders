# The offered Tool set is enforced at dispatch, not only in the request

Two places restricted which Tools a model *should* call by shaping the
request, while dispatch executed whatever the model *did* send:

- The Scout's request offered only the read-only subset (`tools::scout_specs`),
  but `tools::run_read_only` dispatched any name through the full registry -
  plugin-free and, for `run_command`, with no Approval gate. A Scout whose
  small model hallucinated a `write_file` or `run_command` Tool Call
  executed it, contradicting CONTEXT.md's "a Scout cannot edit, run
  commands, or dispatch further Scouts".
- The Endgame narrowed the offered Tools at the request-shaping moment
  (ADR-0015/0016: run_command only on the Verification Pass, none on the
  final Pass), but the Turn's batch executed any Tool Call the reply
  carried. A model insisting on `write_file` during its final Pass wrote
  the file. The glossary's close - "a tool-insistent reply closes on the
  turn-limit marker" - happened only after the damage ran.

The Scout hole was never a considered decision. The Endgame hole WAS:
ADR-0015 ("those Tools run and the Turn closes exactly as before") and
ADR-0016 ("gets them executed, same tolerance") documented execution as
deliberate tolerance. **This ADR reverses that tolerance** (both ADRs are
amended at their heads). The tolerance reads differently once the
consequences are traced: a final-Pass `write_file` lands AFTER the
Endgame's whole schedule has run, so a capped Turn can end with a write
no Verification Pass ever had the chance to check; a Verification Pass
spent editing instead of verifying defeats the narrowing's only purpose;
and an insisted `run_command` skips nothing less than the Approval gate's
intent - the user approved a verification, not whatever the model slips
in. Small local models are exactly the population that hallucinates Tool
Calls the request never offered; the harness's whole posture (CONTEXT.md:
Endgame) is that small models comply with mechanics, not requests.

## Decision

The set of Tools a Pass offers is a fact of the Pass, enforced at
dispatch - the same seam as the malformed-input sentinel (mechanics, not
a Governor's judgment):

- `tools::run_read_only` answers any call outside the Scout's read-only
  subset with the Voice's refusal (`voice::scout_tool_refusal`) and never
  runs it. The subset stays single-sourced: dispatch checks the same list
  the offered specs are built from.
- The Turn loop shapes the Pass's Offer (CONTEXT.md) once per Pass at
  the request-shaping moment, after the Governors' NarrowTools
  Intervention: the narrowed specs move into the Offer, and the request
  carries exactly what the Offer holds. `turn::batch` answers any call
  naming a Tool the Offer does not name with the Voice's refusal
  (`voice::tool_not_offered`) and never runs it - before the answering
  arbiter, the Plugin lifecycle, and the Approval gate.

A refusal is an ordinary error Tool Result: it enters the Conversation
(so the model reads what it may call instead) and appears in the
Transcript. On the Ledger it is a TYPED fact, not a sniffed string: the
batch answers every Tool Call as an Answer (CONTEXT.md) whose
constructor pairs the Voice's wording with the ran-fact
(`CallOutcome::Refused`, beside `CallOutcome::Denied` for the Approval
gate), and the Ledger records an Answer through its one recording
method (`Ledger::record`), which for a refusal moves only the
consecutive-failure tally. The write/verification facts and the
run_command outcomes stand. An earlier draft had the Ledger exempt the
refusal by comparing against the Voice's wording; review correctly
flagged that as coupling the Ledger's facts to strings the Voice is
free to re-tune per model, so the denial path was de-sniffed at the
same seam.
Without the routing a refused `run_command` would clear
genuinely-unverified writes and plant a phantom dangling command,
displacing the real failure the Handoff seed carries verbatim (ADR-0028's
guarantee). On a capped Turn
the refusal composes with the existing judgments: a write refused on the
final Pass never lands, so no Recovery Turn opens; a verification Pass
wasted on a refused read leaves the write unverified, so the cap recovers
exactly as if the model had idled the Pass away.

## Considered options

- **Filter inside `tools::execute` with an allowlist parameter** - one
  enforcement point for both seams. Rejected: the Turn's batch does not
  dispatch through `tools::execute` directly (the Plugin lifecycle owns
  execution, ADR-0007), so the check would still need a batch-side twin;
  and the two seams refuse for different reasons with different Voice.
- **Drop tool-insistent blocks from the Conversation entirely** (answer
  nothing). Rejected: ADR-0004's rule - every Tool Call gets exactly one
  Tool Result - keeps roles alternating and the Session Log foldable; a
  voiced refusal also teaches the model the schedule, a bare drop teaches
  nothing.
- **A Governor issuing the refusal as an Intervention.** Rejected: the
  refusal carries no trajectory judgment - the offered set is a wire
  fact, and a call outside it is as mechanical as undecoded JSON. It sits
  beside the malformed-input sentinel, where the batch's moduledoc already
  lists the gates.

## Consequences

- `turn_limit = 1` becomes degenerate by construction: the only Pass is
  the final Pass and offers no Tools. Tests that used it as a shortcut to
  the cap relied on the hole this ADR closes; they were re-shaped to
  "work lands on an offered Pass, a tool-insistent final Pass caps the
  Turn".
- One asymmetry is acknowledged, not fixed: the Scout's forced report
  Pass offers no Tools (scout.rs), but `run_read_only` enforces only the
  static read-only subset, so an insistent `read_file` on the report Pass
  still runs. It is read-only, bounded by the Scout's hard Pass cap, and
  answers in the Scout's own throwaway Conversation - the blast radius
  this ADR exists to close (mutation, commands, gate-skipping) is not
  engaged. If a live run ever shows a Scout looping reads on its report
  Pass, a per-Pass Offer can ride the Scout's state the way the Turn's
  Offer rides its loop state.
- The Scout's read-only guarantee and the Endgame's narrowing are now
  true at the same layer the rest of the harness's mechanics live.
- No Setpoints, no new Intervention kind, no Conversation-shape change:
  a refusal is an ordinary error Tool Result.
