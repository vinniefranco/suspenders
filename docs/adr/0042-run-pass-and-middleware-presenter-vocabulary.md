# Vocabulary realignment: Run over Turn, Middleware and Presenter over Plugin

The glossary coined its own words where the ecosystem already had settled
ones, raising the barrier for a new contributor. Two coinages were not new
words for new concepts (which a glossary earns its keep by naming) but
standard words either reassigned to a different concept or rejected in favor
of a term whose everyday meaning contradicts the definition. Both mislead on
first read. This ADR realigns them and records why, so a future architecture
review does not re-suggest reverting.

The test applied: replace the coined term with the industry-standard one. If
a real distinction is lost, the coinage stays. If nothing is lost but
familiarity, it is a barrier with no payoff.

## Turn becomes Run; Pass stays

**The false friend.** "Turn" named the whole user request and everything the
Agent did to answer it. But pi, Anthropic, and OpenAI all call one model
response and its Tool Calls a "turn" - which Suspenders calls a **Pass**. The
single most common word in agent-loop code pointed at the opposite level. A
contributor reads `Turn`, imports "one exchange", and is silently wrong about
the outermost control-flow concept. A coined word for a new concept costs one
glossary lookup; a standard word pointing the wrong way costs a persistent
mismatch.

**Resolved:** the outer request cycle is a **Run**, matching OpenAI's Run (a
call that processes across many steps until it needs input again). The inner
step keeps its coined name **Pass**. Compound terms follow: Run Limit, Run
Ledger, Run Settlement, Recovery Run. "A Run has up to N Passes" and "Run
Limit" now read the way the ecosystem talks about max turns per run.

**Half, by design.** This is a one-way rename: `Turn -> Run`, `Pass`
unchanged. The alternative full alignment (`Pass -> Turn` as well) is a
two-way rename sharing the token "Turn" - the word survives, moved from outer
to inner - which cannot be swept mechanically and where any straggler reads
plausibly-but-wrong. The half rename removes the actively-misleading part (the
reassigned standard word) at half the blast radius and none of the two-way
risk. It leaves "Pass" as a coined word for what the ecosystem calls a turn:
category (a) friction, one glossary lookup, no false friend. Adopting "turn"
for a Pass later is not foreclosed; it is deferred.

**The Run collision, accepted.** "Run" brushes against `run_command` and the
informal "a run of the TUI" (now spelled as a **Session**, "one session from
launch to exit"). Context disambiguates; the Session entry was reworded to
drop "one run". The weaker fallbacks were rejected: "Exchange" reads as a
single Pass, "Request" collides with request-shaping.

## Plugin splits into Middleware and Presenter

**The rejected term was the right one.** A "Plugin" wrapped a Tool Call at
three points: before execution (adjust or deny), after execution (transform
the Tool Result), and at Presentment (enrich what the user sees). The
before/after wrap - "Plugins wrap one another, first-registered outermost",
with denial short-circuiting the call - is textbook **middleware** (the
Tower/Rack onion), the exact term CONTEXT.md rejected. Meanwhile "plugin" in
common usage means "adds a capability", the opposite of the definition
("Plugins never add Tools; they wrap existing ones"). The coinage raised the
barrier the standard word would have lowered.

**Two seams, named apart.** The word "Plugin" also unified two genuinely
different roles at two different seams: the execution-path wrap and the
display-path enrichment. Some extensions fill only one (a diff view is
display-only; a token counter is execution-only). Resolved: the execution
wrap is a **Middleware**, the display enrichment a **Presenter**. One
registered extension may fill either role or both. Presentment and Artifact
are unchanged and now belong to the Presenter.

Chosen the domain word **Middleware** over Tower's `Layer`: "middleware" is
recognized across languages; `Layer` is framework jargon. "Hook" stays as the
name for the lifecycle attachment points, now consistent (a Middleware
exposes lifecycle hooks).

## Considered and rejected

- **Leave the lexicon as-is (it is ubiquitous language).** Rejected for these
  two only. Most of the glossary is coined words for novel concepts (Governor,
  Nudge, Anchor, Setpoint, Lull, Dead Mass, Endgame) with no industry
  standard - those earn their keep and are untouched. The realignment targets
  only standard-word collisions.
- **Full Turn/Pass swap now (`Pass -> Turn` too).** Deferred: two-way
  token-sharing rename, high straggler risk, marginal added alignment over the
  half rename.
- **Keep "Plugin" as an umbrella over the two roles.** Rejected: it is the
  term that misleads; the split is the point.
- **Rename in docs only, leave the code.** Rejected as the end state: ADR-0022
  ties the module tree to the domain tree one-to-one, so the glossary and the
  code names must converge. The code rename is staged as follow-up work
  (`turn/ -> run/`, `plugin(s)/ -> middleware/` plus a Presenter seam),
  gated by the suite and clippy; until it lands, glossary and code diverge
  knowingly.

## Consequences

- CONTEXT.md: the Turn entry becomes Run (with an industry-precedent note);
  Pass records the keep-and-defer decision; Plugin splits into Middleware and
  Presenter; Presentment, Artifact, Session, Voice, and the Relationships
  update. Two Flagged ambiguities record both renames.
- ADR-0007 (plugin-lifecycle): its naming is superseded - "Plugin" becomes
  "Middleware"/"Presenter". The lifecycle it describes is unchanged.
- ADR-0011 (turn-loop), ADR-0028 (recovery-turn), ADR-0040 (turn-lane) and
  the ADR-0015/0016 Pass machinery keep their meaning; "Turn" in them now
  reads as "Run". They are historical records, amended by this note rather
  than rewritten.
- Follow-up: the code rename is a separate, gated change. Module directories,
  the `Turn`/`Plugin*` types, and their tests move to the new vocabulary; no
  behavior changes.
