# Suspenders

A terminal coding agent for small local models: a full-screen TUI where a locally-served LLM completes coding tasks in the user's project by calling tools.

> This file is a glossary and nothing else - the ubiquitous language of the
> domain. Implementation and architecture decisions live in `docs/adr/`.

## Language

**Session**:
One run of the Suspenders TUI, from launch to exit, holding exactly one Conversation. Its fixed facts - the Project Root, the Context Budget, the Result Cap, the Turn Limit, the command timeout, the Plugin list, and the model connection (endpoint, token, output cap, temperature) - are resolved and validated once at launch into a Session value; the Conversation and the Active Model are mutable state that live beside that value, owned by the Agent.

**Conversation**:
The ordered message history sent to the model - user messages, assistant messages, and Tool Results.

**Active Model**:
The model identifier the next Turn will call. Owned by the Agent as mutable state beside the Session value - not one of the Session's fixed facts. Seeded from the model connection at launch and changed by the `/model` Slash Command; only the identifier changes, never the endpoint, the output cap, or any figure the Context Budget and Result Cap derive from, so a change needs no re-validation. A change takes effect on the next Turn: an in-flight Turn finishes on the model it captured when it began.
_Avoid_: current model, model connection (the connection is the fixed endpoint and credentials; the Active Model is the mutable choice of which model to call over it).

**Turn**:
One user request and everything the Agent does to answer it - the model may make many Tool Calls within a single Turn.
_Avoid_: iteration, round

**Agent**:
The orchestrator that runs Turns: it streams the model's output, executes Tool Calls, and appends results to the Conversation.

**Pass**:
One model response and the Tool Calls it carries - the unit the loop repeats within a Turn. A Turn is one or more Passes.
_Avoid_: turn (pi and much of the ecosystem call this a turn; in Suspenders a Turn is the whole user request), iteration

**Turn Limit**:
The maximum number of Passes allowed within one Turn. The last permitted Pass is a forced final Pass (ADR-0015): no tools offered, prompted to state what was accomplished, what remains, and whether changes are verified - so a capped Turn ends with the model's conclusion rather than a bare marker. The Pass before it is a Verification Pass (ADR-0016) when writes are unverified: run_command only, so a capped Turn cannot end with unverified changes for lack of opportunity. A model that still answers with Tool Calls ends the Turn on the marker with a distinct stop reason; the user may start a new Turn to continue.
_Avoid_: iteration cap, loop limit

**Endgame**:
The mechanical schedule by which a Turn ends at its Turn Limit, counted in Passes remaining: at 2, the wrap-up warning rides the results tail (or the Verification Pass prompt in its place when writes are unverified); at 1, the Verification Pass narrows the offered Tools to run_command when writes are unverified, and the final-Pass prompt rides the tail; at 0, the final Pass offers no Tools and a tool-insistent reply (real Tool Calls or serialized markup in text) closes on the turn-limit marker instead of passing as a conclusion. The narrowing is enforced at dispatch, not just in the request (ADR-0035): a Tool Call the Pass did not offer is answered with the Voice's refusal and never executes - a mechanic beside the malformed-input sentinel, not a Governor's judgment. Mechanical because small models comply with mechanics, not requests.
_Avoid_: wind-down, wrap-up phase (the wrap-up warning is one step of the Endgame, not its name)

**Recovery Turn**:
A Turn the harness opens itself when the previous Turn settled at its Turn Limit with unverified writes, or a Dangling Failure alongside a write that landed this Turn - the work is demonstrably unfinished, so one more bounded attempt is issued rather than leaving a broken state. A Dangling Failure with no writes this Turn is exploration, not unfinished implementation (a read-only task that merely ran a failing command), and opens no Recovery Turn. Issued by the Endgame Governor through the close-and-open-a-Recovery-Turn Intervention; a Setpoint bounds how many may follow one user request. Its prompt belongs to the Voice - the only Turn whose prompt Suspenders authors - and it still serves the original user request. Two shapes: Continuation and Handoff.
_Avoid_: retry (nothing is re-attempted from scratch; unfinished work continues), auto-continue (that's the Continuation shape, not the umbrella)

**Continuation**:
The Recovery Turn shape that keeps the Conversation: the recovery prompt is appended and the model continues with everything it saw before.

**Handoff**:
The Recovery Turn shape that retires the Conversation: Compaction seeds a fresh one - the original task verbatim, the Plan verbatim, files touched, the model's narrative - plus the failing verification result verbatim - the Dangling Failure's own output, the command the recovery prompt names, never merely the last command run - and the recovery prompt starts it clean. The default shape: a fresh context with a structured handoff beats continuing a degraded one, and the gap widens as models shrink.
_Avoid_: restart (the work and its facts carry over; only the rot is left behind)

**Dangling Failure**:
A command string whose most recent run this Turn failed. A passing run clears only its own command string, so a red full-suite run followed by a green filtered rerun still dangles - a capped Turn cannot launder a failure by rerunning a narrower command. The failing arm of the Recovery Turn's trigger, but only alongside a write that landed this Turn; the Verify-failed Nudge keeps judging the last run only.
_Avoid_: command failing (that's the last-run-only fact the Nudge reads), red build (too broad; the failure is per command string)

**Tool**:
A named capability with a JSON schema that the model can invoke (v1: read_file, list_files, edit_file, write_file, grep, run_command).

**Tool Call**:
A structured `tool_use` block emitted by the model requesting one Tool execution.
_Avoid_: invocation (the legacy text-protocol term), tool request

**Tool Result**:
The structured outcome of executing a Tool Call, returned to the model as a `tool_result` block.

**Answer**:
How the Turn's batch answered one Tool Call: the Tool Result the model will read plus the typed fact of whether the call ran - Ran (executed, and the outcomes that read as runs: a Governor's replaced result, a Plugin halt, a malformed-input answer), Denied (the Approval gate), or Refused (the Pass did not offer the Tool, ADR-0035). Built only through constructors that pair the Voice's wording with the fact so the two cannot drift, and recorded on the Turn Ledger through one method: the batch states the fact, the Ledger owns what each fact moves.
_Avoid_: response (a Response is the model's), reply (also the model's), outcome alone (the ran-fact is one part of an Answer)

**Offer**:
The Tools one Pass puts before the model. The narrowed specs move into the Offer at the request-shaping moment (after the Governors' NarrowTools Intervention); the request carries exactly what the Offer holds, and the batch refuses any Tool Call the Offer does not name (ADR-0035). One value with two readers, so the enforced set and the wire set cannot drift. Before the first request is shaped, the Offer offers nothing - and the batch can never run before then.
_Avoid_: allowlist, whitelist (a filter's framing; the Offer is a fact of the Pass), available tools

**Thinking**:
The model's reasoning stream, displayed but never fed back into the Conversation.
_Avoid_: reasoning content, chain of thought

**Transcript**:
The display-side history of a Session - everything the user saw, in order: user prompts, assistant text, collapsed Thinking, Tool Call and Tool Result summaries, and info lines. Not the Conversation: Thinking and info lines live in the Transcript but never in the Conversation.
_Avoid_: message list, chat log

**Screen**:
The pure UI core - the fold root owning everything the terminal shows: the Transcript, the Composer, the Approval modal, and the status-bar figures. Folds keys and Session events into new state plus effects; the adapter executes the effects and draws. Keys route through it in a fixed order: the Approval gate first, then the Composer's first refusal, then the Screen's own arms. Display-side only: nothing in the Screen enters the Conversation.
_Avoid_: Transcript (that's the display history the Screen owns, not the whole core), UI state (too vague), model (TEA jargon; the domain names the thing)

**Composer**:
The input area of the TUI where the user authors the next prompt. A leading `/` opens the Slash Command menu; a committed selector-opening command shows its value list in its place. Both overlays are Composer states - the draft filters them, backspacing out of one re-enters the other, and Escape empties the Composer - not modals: only an Approval takes keys away from the Composer. Otherwise a submitted draft starts a Turn when the Agent is idle or becomes Steering when a Turn is running. Display-side only: its draft is never part of the Conversation until submitted.
_Avoid_: input line (it is not a line; drafts may span many), prompt (that's what a submitted draft becomes), selector modal (the selector is a Composer overlay; the Approval modal is the only modal)

**Slash Command**:
A directive the user invokes from the Composer, never sent to the model. Typing `/` opens a menu of the available commands that filters as the user types; selecting one runs it. Always available whatever the Agent is doing - a running Turn never suppresses the menu, though a command's effect may land at a Turn boundary (a model change applies to the next Turn). Distinct from a prompt (which starts a Turn) and from Steering (mid-Turn user text that joins the Conversation unadorned): a Slash Command enters neither the Conversation nor the Transcript as user text; it drives the harness or the Session (e.g. choosing the model), and the Transcript may show its outcome as an info line. The set of commands is open - adding one is routine.
_Avoid_: command (that's the Agent's internal actor message), directive, colon-command

**Project Root**:
The directory Suspenders was launched from, captured once per Session as a value. Every Tool Call is confined to it: paths must not escape it, and run_command executes in it.
_Avoid_: cwd (that's ambient process state; the Project Root is captured once and passed explicitly)

**Approval**:
The user's explicit gate on a run_command Tool Call before it executes: approve, deny, or approve-always.
_Avoid_: confirmation, permission

**Standing Approval**:
The result of an approve-always answer: run_command Tool Calls whose command string is identical to the approved one are auto-approved for the rest of the Session, without a modal. The Transcript still records each auto-approved run.
_Avoid_: allowlist (implementation term), whitelist

**Context Budget**:
The token allowance the Conversation must fit within for the configured model.

**Eviction**:
Replacing the contents of dead Conversation blocks with an elision marker: old Tool Results, superseded Anchors, results made dead by Supersession, and the input bodies of successful writes. A wave fires when the Conversation presses the Context Budget or when Dead Mass crosses its threshold; once triggered, Eviction overshoots to a low-water mark, so elisions arrive in rare waves and the request prefix stays byte-stable for server-side prompt caching between them.
_Avoid_: truncation (that's what the server does when we fail), compaction

**Dead Mass**:
The total size of Conversation content whose information is already superseded - the input bodies of successful writes (the file on disk holds the result), older results of repeated identical Tool Calls, redundant re-reads, stale Anchors. Dead Mass rots a small model's attention long before the Context Budget is threatened, so it has its own Eviction trigger: when it exceeds its threshold fraction of the Context Budget, a wave fires even with budget to spare.
_Avoid_: bloat, garbage (both too vague to trigger on)

**Supersession**:
The rule that classifies Conversation content as dead: a newer result of an identical Tool Call supersedes the older ones (the newest survives verbatim), and a successful write supersedes its own input body - the file on disk is the truth. Identity is the full Tool Call (name and input), never a judgment call. A failed edit's input is not superseded by its failure; only a later successful write to the same file supersedes the attempt chain.
_Avoid_: deduplication (mechanism, not meaning), pruning

**Result Cap**:
The size ceiling one Tool Result may occupy in the Conversation, derived from the Context Budget once per Session. Oversized Tool Results are cut before they enter the Conversation: run_command keeps its start and end (the exit code lives at the end), every other Tool keeps its start.
_Avoid_: output limit, truncation (reserved for the server's failure mode)

**Cancellation**:
The user aborting a running Turn; the Conversation records a clean partial state.

**Turn Settlement**:
How an ended Turn enters the Conversation. Every Turn settles exactly one way: completed, failed, or cancelled (a crash settles as a failure). A Turn that did not complete settles on its partial state, closed with a marker so roles keep alternating.
_Avoid_: completion (reserved for the model's response), turn management

**Governor**:
A tunable rule that watches the Pass cycle and intervenes to keep the model on course. Each Governor owns its trigger and its setpoints - the thresholds and cadences tuned from observed model behavior - and acts only through an Intervention. Nudges, Anchors, and the Endgame schedule are all issued by Governors. Compaction and Eviction are budget mechanics, not Governors: what they elide is correct or incorrect, never an opinion - though the cadence of their waves carries tuned thresholds.
_Avoid_: rider (one delivery shape of a Nudge, not the rule that sends it), heuristic (the informal name; a Governor is the thing itself), rule, policy

**Intervention**:
One of the closed set of actions a Governor may take: replace a Tool Result, annotate one, stand alone as a user message, ride the results tail, narrow the offered Tools, silence Thinking for a Pass, close the Turn on a marker, or close the Turn and open a Recovery Turn. The set is deliberately closed: a new Governor is routine, a new kind of Intervention is a visible design decision.
_Avoid_: action (too generic), effect (mechanism, not meaning)

**Setpoint**:
A tunable value - a threshold, cadence, or cap that encodes a learning about how small models drift. Every Setpoint belongs to exactly one owner - a Governor or a named mechanic - and carries a default; the Session resolves them once at launch, and a Setpoint becomes user-configurable only when a real model has demanded a different value.
_Avoid_: constant (a Setpoint is tuned, not fixed), config option (most are never exposed), magic number

**Turn Ledger**:
The record of facts about the running Turn, written once as each thing happens: the Tool Calls each Pass carried, per-Tool failure tallies, writes and whether a verification has run since, Passes remaining. The Ledger holds facts, never opinions or setpoints - Governors read it and judge; no Governor reads another Governor's state.
_Avoid_: nudge state (the legacy state bag), shared state, blackboard

**Nudge**:
Suspenders-voiced text the Turn injects into the Conversation to redirect a drifting model: replacing a Tool Result (identical Tool Call repeated), annotating one (repeated failures of the same Tool), standing alone as a user message (changes left unverified, the model finishing while its last run_command failed, or the model's reply arriving empty), or riding the tool-results user message (consecutive Passes spent hand-exploring inline - reading files or shelling out to search - instead of dispatching a Scout). A Nudge never enters the system prompt.
_Avoid_: hint, warning

**Plugin**:
A unit of extension that observes and alters the lifecycle of a Tool Call at three points: before the Tool executes (may adjust or deny the call), after it executes (may transform the Tool Result the model sees), and at Presentment (may enrich what the user sees). Plugins never add Tools; they wrap existing ones.
_Avoid_: hook (a hook is one attachment point in the lifecycle, not the unit of extension), extension, middleware

**Presentment**:
The act of turning a Session event into a Transcript item. Plugins may substitute a richer item at Presentment - a diff instead of a one-line summary - but Presentment only ever shapes the Transcript, never the Conversation.
_Avoid_: rendering (that's the terminal drawing the Transcript; Presentment decides what the item is)

**Artifact**:
Display-side data a Plugin derives from a Tool Call - a diff, an annotation - carried alongside the Tool Result to Presentment. An Artifact never enters the Conversation and costs no Context Budget.
_Avoid_: metadata, attachment

**Voice**:
Every Suspenders-voiced string the model reads: the system prompt, every Nudge, and every marker (elision, turn limit, the wrap-up warning two Passes before it, cancellation, Result Cap cuts, malformed input, the offered-set refusals). The boundary is voice, not arity: wording may be parameterized, but Suspenders authors it. Strings a Tool produces about its own execution stay in that Tool; strings a Plugin produces about its own decisions stay in that Plugin. Owned by one module so wording can be tuned per model in one place.
_Avoid_: prompt strings, constants, "static strings" (parameterized wording still belongs), Steering Vocabulary (legacy name; Steering now means mid-Turn user input)

**Steering**:
User input delivered to a running Turn. It never aborts in-flight work: the current model response and Tool Calls finish, then the steering text joins the Conversation before the next model call. Steering is the user's voice, not Suspenders's - it enters the Conversation unadorned, never marked. Distinct from Cancellation, which ends the Turn.
_Avoid_: interrupt (nothing is interrupted), injection (mechanism, not meaning), follow-up (Steering that misses its Turn rolls over; there is no separate follow-up concept)

**Rollover**:
What happens to Steering the Turn ended before delivering: it auto-submits as the next Turn's prompt. Cancellation discards it instead - cancel means stop everything.

**Plan**:
The model-maintained statement of the current goal, its steps, and their progress, held by the harness outside the Conversation and updated through the plan Tool. The Plan is the model's voice - Suspenders never authors its content - and it survives Compaction verbatim because the harness owns it, not the summary.
_Avoid_: todo list, scratchpad (free-form; a Plan is structured), notes

**Anchor**:
An injected copy of the Plan and the original task statement, placed near the tail of the Conversation and refreshed periodically and immediately after a Compaction, so the goal always sits where a small model actually attends. The framing around an Anchor belongs to the Voice; its Plan content does not. Stale Anchors are ordinary evictable blocks.
_Avoid_: reminder, re-injection (mechanism, not meaning), reorientation nudge (a Nudge is corrective and fires only while its trigger persists; an Anchor is routine)

**Scout**:
A disposable read-only worker the model dispatches through the explore Tool: it searches the Project Root (grep, list, read) in its own fresh Conversation with a hard Pass cap and returns a structured findings report as an ordinary Tool Result. The cap's last Pass is a forced report Pass (no tools offered - the only move left is the report), and Scouts run without Thinking by default (ADR-0014). The exploration never enters the main Conversation - only the report does. A Scout cannot edit, run commands, or dispatch further Scouts - enforced at dispatch (ADR-0035), not just by the offered specs: a hallucinated mutating or command Tool Call is answered with the Voice's refusal and never executes.
_Avoid_: subagent (generic; Suspenders has exactly one delegation shape), task agent, worker

**Session Log**:
The durable, append-only record of one Session: its fixed facts, then every Conversation event in order, written as each happens. The Session Log records the Conversation, not the Transcript - Thinking and info lines are never in it.
_Avoid_: history file, transcript file (the Transcript is display-side and is not what's persisted)

**Resume**:
Reconstructing a Conversation from a Session Log so a new Session can continue where a crashed or exited one stopped. A log that ends mid-Turn settles that Turn as failed - a crash settles as a failure, discovered at Resume rather than at the moment of death.

## Relationships

- A **Session** has exactly one **Conversation**, one **Transcript**, and one **Project Root**
- A **Session**'s fixed facts are resolved and validated once at launch; every **Turn** and **Tool Call** reads them from that value, never from ambient configuration
- The **Active Model** is the one thing a **Turn** does NOT read from the fixed **Session** value: the **Agent** owns it mutably and each **Turn** captures it when it begins, so a `/model` change lands on the next **Turn**, never mid-flight
- A **Conversation** is a sequence of **Turns**
- A **Turn** is one or more **Passes**; the **Turn Limit** counts **Passes**
- A **Turn** contains zero or more **Tool Calls**, each producing exactly one **Tool Result**
- A **Turn** ends at its **Turn Limit** even if the model is still asking for Tools; the **Endgame** schedules how it ends, counted in **Passes** remaining
- A **Tool Call** for run_command requires an **Approval** before execution, unless a **Standing Approval** covers its exact command string
- A **Standing Approval** belongs to the **Session** - it does not survive restart and never widens beyond the identical command string
- **Eviction** targets dead content - old **Tool Results**, blocks dead by **Supersession**, the input bodies of successful writes, superseded **Anchors** - oldest first, and never the system prompt, the two most recent tool-result exchanges, or the most recent **Anchor**
- **Eviction** fires in waves on either of two triggers - Context Budget pressure, or **Dead Mass** crossing its threshold; once triggered it elides down to a low-water mark, so between waves the request prefix is byte-stable and the server's prompt cache holds
- When **Eviction** cannot fit the **Conversation** within the **Context Budget**, the **Turn** fails loudly; an over-budget request is never sent
- Every **Tool Result** is cut to the **Result Cap** before it enters the **Conversation**; the cap derives from the **Context Budget** once per **Session**
- The system prompt, every **Nudge**, and every marker belong to the **Voice**; the **Governors** that fire them own the when, not the wording
- Every **Nudge**, every **Anchor** placement, and every **Endgame** step is issued by a **Governor**; **Compaction** and **Eviction** are not
- A **Governor** acts only through an **Intervention**; when several **Governors** fire at the same moment, one explicit precedence decides which speaks
- Every **Intervention** belongs to exactly one of the three moments of a **Pass** - shaping the request, answering a **Tool Call**, settling a finish - and precedence is decided within a moment, never across moments
- Facts live in the **Turn Ledger**, opinions in exactly one **Governor**; a **Governor** reads the Ledger and its own trigger state, never a sibling's
- A **Governor** judges the Turn's trajectory; a **Plugin** acts on one **Tool Call** in isolation - a decision that needs Turn history belongs to a **Governor**, never a Plugin
- At the Tool Call moment, **Governors** judge what the model sent and what the model will read; **Plugins** shape what actually runs in between
- The **Approval** gate is neither **Governor** nor **Plugin**: it encodes the user's judgment, not a tuned learning
- A **Plugin** wraps every **Tool Call** at three points: before execution, after execution, and at **Presentment**; Plugins wrap one another, first-registered outermost
- A **Plugin** may adjust or deny a **Tool Call** before execution: the **Nudge** for duplicates keys on what the model sent, and an **Approval** always shows the final, plugin-adjusted command
- A **Plugin** denial still produces exactly one **Tool Result**, voiced by that Plugin
- An **Artifact** travels with its **Tool Result** to **Presentment** and appears only in the **Transcript**, never the **Conversation**
- A **Plugin** failure never fails the **Turn** and never reaches the model; the **Transcript** records it as an info line and the Tool Call proceeds without that Plugin
- A **Turn** ends in exactly one **Turn Settlement**: completed, failed, or cancelled
- A **Recovery Turn** opens only off a Turn that settled at its **Turn Limit** with unverified writes, or a **Dangling Failure** alongside a write that landed this Turn (a failing command with no writes is exploration, not unfinished work); its prompt belongs to the **Voice**, and a **Setpoint** bounds how many may serve one user request
- A **Handoff** carries the **Plan** and original task verbatim (harness-owned facts, never trusted to the summary) plus the **Dangling Failure**'s own result verbatim - the command the recovery prompt names, not merely the last one run; a **Continuation** keeps the whole **Conversation**
- **Steering** is delivered after a Tool Call batch completes and before the next model call; a Turn that ends first triggers **Rollover**, a **Cancellation** discards it
- **Steering** belongs to the user's voice and is never part of the **Voice**
- A **Scout** runs its own fresh **Conversation** against the same model connection; its report is an ordinary **Tool Result**, subject to the **Result Cap**, and its exploration never enters the main **Conversation**
- A **Plan** is the model's voice, held by the harness outside the **Conversation**; only its **Anchor** copies enter the **Conversation**
- An **Anchor** is refreshed immediately after every **Compaction** and periodically between them; stale **Anchors** are evictable like any old block
- **Compaction** fires at the **Compaction Target** and retains the **Compaction Keep**; the two are decoupled so Compactions are rare and deep
- A **Session** appends every **Conversation** event to its **Session Log** as it happens; **Resume** folds that log into a new Session's **Conversation**
- **Resume** requires the same **Project Root**; every other Session fact yields to the resuming Session's, and the **Transcript** notes what changed
- A **Tool Call** in a truncated response is answered with an error **Tool Result** (re-issue it) and none of its batch executes; only **Cancellation** drops a **Tool Call** from the **Conversation**, and the **Transcript** still shows it
- **Thinking** belongs to a **Turn** and appears in the **Transcript**, but never enters the **Conversation**
- Every **Tool Call** executes within the **Project Root**
- A **Nudge** belongs to the **Conversation** (the model must see it) and therefore also appears in the **Transcript**; a **Nudge** fires only while its trigger persists, and the **Turn Limit** bounds all of them

## Example dialogue

> **Dev:** "When the model hits the **Context Budget** mid-**Turn**, do we drop old **Turns**?"
> **Domain expert:** "No - **Eviction** only hollows out old **Tool Results**. The user's instructions in past **Turns** survive; a stale file listing doesn't need to."
>
> **Dev:** "And if the user cancels while a run_command **Approval** modal is open?"
> **Domain expert:** "**Cancellation** wins: the **Tool Call** is recorded as cancelled, no **Tool Result** is fabricated, and the **Turn** ends."

## Flagged ambiguities

- ADR-0015/0016 tolerated Tool Calls on narrowed Passes ("those Tools run"); ADR-0035 (2026-07) reversed the tolerance - such calls are refused at dispatch as mechanics beside the malformed-input sentinel, NOT issued by a Governor. The relationship "every Endgame step is issued by a Governor" covers the schedule (warning, narrowing, prompts, closes); the refusal is the enforcement of that schedule, one layer down.

- "invocation" previously meant a parsed text-protocol tool request (`extract_invocations`); with native tool calling it is retired in favor of **Tool Call**.
- "truncation" was used for both server-side context overflow and our own management strategy - resolved: ours is **Eviction**; "truncation" refers only to the server's silent behavior we're preventing.
- the **Compaction Target** was documented as the full budget target while the code fired at the low-water mark - resolved 2026-07: the trigger is the low-water mark, and the keep level is its own decoupled knob, the **Compaction Keep**.
- "toggling thinking" was read as enabling/disabling the model's **Thinking** when it means expanding/collapsing settled Thinking items in the **Transcript** - resolved 2026-07: Ctrl-T is a display expansion toggle; whether the model thinks at all is a request-level knob (today fixed: on for the main Conversation, off for **Scouts**) with no user-facing toggle.
- **Anchors** and Endgame prompts are Conversation events the model actually read, but only Nudges were persisted, so **Resume** rebuilt a Conversation the model never saw - resolved 2026-07: every rider is logged to the **Session Log** like a Nudge.

## Compaction

**Compaction**:
Replacing old messages in the Conversation with a structured LLM-generated
summary so the model can continue working within the Context Budget. Distinct
from **Eviction** (which is purely mechanical - replacing Tool Result content
with a fixed marker): compaction is semantic - it calls the LLM to extract
what was accomplished, what decisions were made, and what files were touched.
_Avoid_: summarization, context compression (those describe the mechanism,
not the policy)

**Compaction Keep**:
The amount of recent Conversation that survives a Compaction verbatim. Set
well below the Compaction Target so Compactions arrive rarely and each one
summarizes a large, coherent span of finished work - many Turns then run
append-only between Compactions, keeping the server's prompt cache warm.
Its own knob, deliberately decoupled from the trigger: fire high, keep low.
_Avoid_: keep_recent (implementation name), recent-budget

**Compaction Target**:
The token estimate at which Compaction fires - the same low-water mark
Eviction settles to. The Conversation is proactively compacted at a Turn
boundary when its estimate exceeds this target, and reactively mid-Turn only
when Eviction alone cannot bring the estimate under budget.
_Avoid_: compaction threshold (ambiguous with Compaction Keep)

**Proactive Compaction**:
Compaction triggered at the start of a Turn, before its first Pass, when the
Conversation's token estimate already exceeds the compaction target. Runs in
the Turn task like every Compaction, so the Agent stays responsive. Prevents
hitting the budget cliff during the Turn.

**Reactive Compaction**:
Compaction triggered mid-Turn when building a request finds the Conversation
still over budget after Eviction has run dry - the last recovery attempt
before the Turn fails.
