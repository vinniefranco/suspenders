# One canonical Run-stop vocabulary

Why a Run stopped is one type: `stop_reason::StopReason`, a zero-dependency leaf at the crate root. It spans the LLM-reported reasons that ride through a completed Run (`end_turn`, `tool_use`, `max_tokens`, `stop_sequence`), the reasons the Loop mints (`turn_limit` at the Run Limit, `turn_limit_stuck` when the loop-detector trips), Settlement's synthetic reasons for failed/cancelled Runs (`error`, `unknown`), and the one open-vocabulary member: `Custom(atom)`, the string a Hook's `continue:false` names (ADR-0066). The Loop returns it, Run Settlement writes it, the Session Log codes it, the settlement event carries it to the UI, and Voice reads it to pick a close marker - all the same value, unmapped.

There are exactly two seams:

- **The wire maps in once.** `llm::response::StopReason` stays a distinct type: it is the LLM boundary's fact about one response (`Event::MessageEnd` still carries it, and the dispatch decisions over a Pass read it). It enters the canonical vocabulary at one place - the `From` impl beside the wire type, a total name-for-name embedding - reached only at the Run's finish. No other stop-reason mapping exists anywhere.
- **The Session Log serializes the canonical names.** The settled entry's `stop_reason` field writes `StopReason::as_str` and parses with `from_str`. The pre-unification names (`end_turn` ... `turn_limit_stuck`, `error`, `unknown`) are stable, so an existing on-disk log Resumes unchanged; an unrecognized name parses as `Custom` - the only writer of novel names is a Hook, and its atom must round-trip verbatim, not degrade to `unknown`.

The Run-outcome type is likewise one: `run::settlement::Outcome` (`Ok(Conversation, StopReason)` / `Failed` / `Error` / `Down`). The Loop constructs `Ok`, `Failed`, and `Error` directly (naming `context_budget_exhausted` itself, the one reason it can produce); the Agent's watcher mints `Down` when the Run task died without replying. `Settlement::settle` folds that outcome straight into the broadcast `event::Event`, the Session Log entry, and the Rollover decision in one match - there is no settlement-local event enum and no translator module between the Loop and the fold.

`Event::RunFinished` carries the canonical reason, so the display is never lossy: a Run-Limit stop notes `turn stopped: :turn_limit`, a stalled loop `:turn_limit_stuck`, a Hook stop its own atom. (The Conversation-side truth was already there - the Voice's run-limit close marker; the event now agrees with it.)

Considered and rejected:

- **A per-layer stop enum with hand-written translators** (the prior shape: wire, canonical, log, loop `OutcomeStop`, settlement, and an `agent/settle` module folding between them). Five spellings of one fact; the final hop collapsed `turn_limit`/`turn_limit_stuck` and Hook atoms to `unknown`, so the UI could not tell the user why the Run stopped.
- **Degrading the Hook atom to `Unknown`** at the loop/settlement boundary. The atom IS the reason the Run ended; the closed vocabulary gains one open member instead.
- **Folding the wire type into the canonical one.** The wire enum is a serde fact of the LLM boundary (adapters decode into it, dispatch reads it per Pass); the canonical type is a Run-lifecycle fact. Merging them would put `turn_limit` and Hook atoms on the wire type's serde surface, where no adapter can produce them.
