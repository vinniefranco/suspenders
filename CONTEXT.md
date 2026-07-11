# Suspenders

A terminal coding agent for small local models: a full-screen TUI where a locally-served LLM completes coding tasks in the user's project by calling tools.

> This file is a glossary and nothing else — the ubiquitous language of the
> domain. Implementation and architecture decisions live in `docs/adr/`.

## Language

**Session**:
One run of the Suspenders TUI, from launch to exit, holding exactly one Conversation. Its fixed facts - the Project Root, the Context Budget, the Result Cap, the Turn Limit, the command timeout, the Plugin list, and the model connection - are resolved and validated once at launch into a Session value; the Conversation is mutable state that lives beside that value, owned by the Agent.

**Conversation**:
The ordered message history sent to the model - user messages, assistant messages, and Tool Results.

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
The mechanical schedule by which a Turn ends at its Turn Limit, counted in Passes remaining: at 2, the wrap-up warning rides the results tail (or the Verification Pass prompt in its place when writes are unverified); at 1, the Verification Pass narrows the offered Tools to run_command when writes are unverified, and the final-Pass prompt rides the tail; at 0, the final Pass offers no Tools and a tool-insistent reply (real Tool Calls or serialized markup in text) closes on the turn-limit marker instead of passing as a conclusion. Mechanical because small models comply with mechanics, not requests.
_Avoid_: wind-down, wrap-up phase (the wrap-up warning is one step of the Endgame, not its name)

**Tool**:
A named capability with a JSON schema that the model can invoke (v1: read_file, list_files, edit_file, write_file, grep, run_command).

**Tool Call**:
A structured `tool_use` block emitted by the model requesting one Tool execution.
_Avoid_: invocation (the legacy text-protocol term), tool request

**Tool Result**:
The structured outcome of executing a Tool Call, returned to the model as a `tool_result` block.

**Thinking**:
The model's reasoning stream, displayed but never fed back into the Conversation.
_Avoid_: reasoning content, chain of thought

**Transcript**:
The display-side history of a Session - everything the user saw, in order: user prompts, assistant text, collapsed Thinking, Tool Call and Tool Result summaries, and info lines. Not the Conversation: Thinking and info lines live in the Transcript but never in the Conversation.
_Avoid_: message list, chat log

**Composer**:
The input area of the TUI where the user authors the next prompt. Submitting it starts a Turn when the Agent is idle and becomes Steering when a Turn is running. Display-side only: its draft is never part of the Conversation until submitted.
_Avoid_: input line (it is not a line; drafts may span many), prompt (that's what a submitted draft becomes)

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
Replacing the contents of old Tool Results (and superseded Anchors) with an elision marker to bring the Conversation back under the Context Budget. Once triggered, Eviction overshoots to a low-water mark below the budget target, so elisions arrive in rare waves and the request prefix stays stable for server-side prompt caching between them.
_Avoid_: truncation (that's what the server does when we fail), compaction

**Result Cap**:
The size ceiling one Tool Result may occupy in the Conversation, derived from the Context Budget once per Session. Oversized Tool Results are cut before they enter the Conversation: run_command keeps its start and end (the exit code lives at the end), every other Tool keeps its start.
_Avoid_: output limit, truncation (reserved for the server's failure mode)

**Cancellation**:
The user aborting a running Turn; the Conversation records a clean partial state.

**Turn Settlement**:
How an ended Turn enters the Conversation. Every Turn settles exactly one way: completed, failed, or cancelled (a crash settles as a failure). A Turn that did not complete settles on its partial state, closed with a marker so roles keep alternating.
_Avoid_: completion (reserved for the model's response), turn management

**Governor**:
A tunable rule that watches the Pass cycle and intervenes to keep the model on course. Each Governor owns its trigger and its setpoints - the thresholds and cadences tuned from observed model behavior - and acts only through an Intervention. Nudges, Anchors, and the Endgame schedule are all issued by Governors. Compaction and Eviction are budget mechanics, not Governors: they are correct or incorrect, never tuned.
_Avoid_: rider (one delivery shape of a Nudge, not the rule that sends it), heuristic (the informal name; a Governor is the thing itself), rule, policy

**Intervention**:
One of the closed set of actions a Governor may take: replace a Tool Result, annotate one, stand alone as a user message, ride the results tail, narrow the offered Tools, silence Thinking for a Pass, or close the Turn on a marker. The set is deliberately closed: a new Governor is routine, a new kind of Intervention is a visible design decision.
_Avoid_: action (too generic), effect (mechanism, not meaning)

**Setpoint**:
A Governor's tunable value - a threshold, cadence, or cap that encodes a learning about how small models drift. Every Setpoint belongs to exactly one Governor and carries a default; the Session resolves them once at launch, and a Setpoint becomes user-configurable only when a real model has demanded a different value.
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
Every Suspenders-voiced string the model reads: the system prompt, every Nudge, and every marker (elision, turn limit, the wrap-up warning two Passes before it, cancellation, Result Cap cuts, malformed input). The boundary is voice, not arity: wording may be parameterized, but Suspenders authors it. Strings a Tool produces about its own execution stay in that Tool; strings a Plugin produces about its own decisions stay in that Plugin. Owned by one module so wording can be tuned per model in one place.
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
A disposable read-only worker the model dispatches through the explore Tool: it searches the Project Root (grep, list, read) in its own fresh Conversation with a hard Pass cap and returns a structured findings report as an ordinary Tool Result. The cap's last Pass is a forced report Pass (no tools offered - the only move left is the report), and Scouts run without Thinking by default (ADR-0014). The exploration never enters the main Conversation - only the report does. A Scout cannot edit, run commands, or dispatch further Scouts.
_Avoid_: subagent (generic; Suspenders has exactly one delegation shape), task agent, worker

**Session Log**:
The durable, append-only record of one Session: its fixed facts, then every Conversation event in order, written as each happens. The Session Log records the Conversation, not the Transcript - Thinking and info lines are never in it.
_Avoid_: history file, transcript file (the Transcript is display-side and is not what's persisted)

**Resume**:
Reconstructing a Conversation from a Session Log so a new Session can continue where a crashed or exited one stopped. A log that ends mid-Turn settles that Turn as failed - a crash settles as a failure, discovered at Resume rather than at the moment of death.

## Relationships

- A **Session** has exactly one **Conversation**, one **Transcript**, and one **Project Root**
- A **Session**'s fixed facts are resolved and validated once at launch; every **Turn** and **Tool Call** reads them from that value, never from ambient configuration
- A **Conversation** is a sequence of **Turns**
- A **Turn** is one or more **Passes**; the **Turn Limit** counts **Passes**
- A **Turn** contains zero or more **Tool Calls**, each producing exactly one **Tool Result**
- A **Turn** ends at its **Turn Limit** even if the model is still asking for Tools; the **Endgame** schedules how it ends, counted in **Passes** remaining
- A **Tool Call** for run_command requires an **Approval** before execution, unless a **Standing Approval** covers its exact command string
- A **Standing Approval** belongs to the **Session** - it does not survive restart and never widens beyond the identical command string
- **Eviction** targets **Tool Results** and superseded **Anchors**, oldest first, and never the system prompt, recent Turns, or the most recent **Anchor**
- **Eviction** fires in waves: once triggered it elides past the target down to a low-water mark, so between waves the request prefix is byte-stable and the server's prompt cache holds
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

- "invocation" previously meant a parsed text-protocol tool request (`extract_invocations`); with native tool calling it is retired in favor of **Tool Call**.
- "truncation" was used for both server-side context overflow and our own management strategy - resolved: ours is **Eviction**; "truncation" refers only to the server's silent behavior we're preventing.
- the **Compaction Target** was documented as the full budget target while the code fired at the low-water mark - resolved 2026-07: the trigger is the low-water mark, and the keep level is its own decoupled knob, the **Compaction Keep**.
- "toggling thinking" was read as enabling/disabling the model's **Thinking** when it means expanding/collapsing settled Thinking items in the **Transcript** - resolved 2026-07: Ctrl-T is a display expansion toggle; whether the model thinks at all is a request-level knob (today fixed: on for the main Conversation, off for **Scouts**) with no user-facing toggle.

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
