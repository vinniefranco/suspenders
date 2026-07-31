# Suspenders

A terminal coding agent, local-first: a full-screen TUI where an LLM - a locally-served small model by default, any configured Provider's model when chosen - completes coding tasks in the user's project by calling tools.

> This file is a glossary and nothing else - the ubiquitous language of the
> domain. Implementation and architecture decisions live in `docs/adr/`.

## Language

**Session**:
One session of the Suspenders TUI, from launch to exit, holding exactly one Conversation. Its fixed facts - the Project Root, the Run Limit, the command timeout, the Middleware and Presenter set, the Provider set (each Provider's endpoint, credential, and Api), and the sampling temperature - are resolved and validated once at launch into a Session value; the Conversation and the Active Model are mutable state that live beside that value, owned by the Agent. The Context Budget and the Result Cap are not fixed facts: they derive from the Model each Run captures.

**Conversation**:
The ordered message history sent to the model - user messages, assistant messages, and Tool Results.

**Active Model**:
The Model the next Run will call, named by a scoped identifier - `provider/model-id`. Owned by the Agent as mutable state beside the Session value - not one of the Session's fixed facts. Seeded from config at launch and changed by the `/model` Slash Command. A change takes effect on the next Run: each Run captures the whole Model (window, output cap, pricing, compat) when it begins, and the Context Budget, Result Cap, and reply reserve derive from that capture - so a switch needs no re-validation, and an in-flight Run finishes on the Model it captured.
_Avoid_: current model, model connection (retired with the single-connection era; the Provider owns the endpoint and credential, the Active Model is the mutable choice of which Model to call).

**Provider**:
A host that serves models: an identifier, a base URL, a credential, and the Api its Models speak. Built-in Providers (anthropic, openai, …) come from the Catalog and need only their environment key; custom Providers (a local LM Studio, a private proxy) are declared in config and discover their Models live via the host's models endpoint. Part of the Session's fixed facts. A Provider the user cannot yet call never vanishes from the selector - it appears unavailable, naming the environment key that would enable it.
_Avoid_: backend, vendor, connection (the retired single-connection term)

**Api**:
A wire protocol an adapter speaks - `anthropic-messages`, `openai-completions`. The seam of the LLM boundary: Suspenders hand-writes one adapter per Api, and every Provider is data that selects one. Host quirks within an Api are per-Model compat facts, never subclasses.
_Avoid_: protocol (ambiguous with transport), driver

**Model**:
The facts of one model at one Provider: scoped identifier, Api, context window, output cap, pricing, and compat quirks. Read from the Catalog for built-in Providers; synthesized from live discovery plus config for custom ones. The Run captures a Model when it begins, and the budget figures derive from that capture.
_Avoid_: model card, model info

**Catalog**:
The generated registry of known Providers and their Models, produced from models.dev by a committed generator and embedded in the binary. The baseline that live discovery overlays; custom Providers live beside it, not in it.
_Avoid_: model list (that's a live models-endpoint answer), registry (implementation term)

**Provenance**:
The Provider and Model stamped on every assistant message as it enters the Conversation, persisted in the Session Log. Read at request-shaping: history whose Provenance matches the target Model replays verbatim; history from elsewhere is normalized (tool-call identifiers rewritten to the target Api's rules, orphaned Tool Calls answered in the Voice) so a Conversation crosses Providers without a restart.
_Avoid_: origin, source model

**Run**:
One user request and everything the Agent does to answer it - the model may make many Tool Calls within a single Run. A Run is one or more Passes.
_Avoid_: iteration, round, turn (the ecosystem's "turn" is one Pass, not the whole request; Suspenders names the request cycle a Run, matching OpenAI's Run - a call that processes across many steps until it needs input again). Renamed from "Turn" (2026-07): the old name reassigned the ecosystem's "turn" to the outer level and misled newcomers.

**Agent**:
The orchestrator that drives Runs. The loop is a plain ReAct loop (ADR-0045): each Pass builds one request (the static system prompt, the full Conversation, the full Tool registry - no per-Pass narrowing), streams the response, executes any Tool Calls, appends the results, and repeats until the model returns no Tool Calls, bounded by the Run Limit. It injects nothing to steer the model; the only runtime intervention is the passive Loop-detector.

**Pass**:
One model response and the Tool Calls it carries - the unit the loop repeats within a Run. A Run is one or more Passes.
_Avoid_: turn (pi and much of the ecosystem call a Pass a "turn"; Suspenders keeps the coined "Pass" so it need not reassign "turn" - the outer request is a Run. Adopting "turn" for a Pass is a possible later step, deferred to avoid a two-way rename), iteration

**Run Limit**:
The maximum number of Passes allowed within one Run (the `max_turns` bound, default generous enough for a real multi-step task). A Run that reaches the limit while the model is still answering with Tool Calls ends on the run-limit marker with a distinct stop reason; the user may start a new Run to continue. The bound is a backstop, not a schedule: it withdraws no tools and injects no steering as it approaches.
_Avoid_: iteration cap, loop limit

**Loop-detector**:
The one runtime intervention: a passive circuit breaker that ends the Run when the model emits the byte-identical tool-call batch `loop_stall_limit` times in a row (default 5). It injects nothing into the Conversation - no corrective text, no steering - it ends the Run with a close marker and emits an Event. The whole point is that a clean context plus a good prompt keeps a small model on task more often than corrective text pulled it back; the detector is the silent safety for the rare genuine runaway (ADR-0045).
_Avoid_: nudge, governor (it judges nothing and injects nothing; it counts identical batches and stops), guard rail

**Tool**:
A named capability with a JSON schema that the model can invoke: read_file, write_file, edit_file, run_command, grep, glob, list_files, web_fetch, and todo_write. Named and shaped to match qwen-code's tools so a small local model calls them without translation (ADR-0045). Every request offers the full registry - there is no per-Pass narrowing.

**Tool Call**:
A structured `tool_use` block emitted by the model requesting one Tool execution.
_Avoid_: invocation (the legacy text-protocol term), tool request

**Tool Result**:
The structured outcome of executing a Tool Call, returned to the model as a `tool_result` block.

**Answer**:
How the Run's batch answered one Tool Call: the Tool Result the model will read plus the typed fact of whether the call ran - Ran (executed, and the outcomes that read as runs: a Middleware halt, a malformed-input answer) or Denied (the Approval gate). Built only through constructors that pair the Voice's wording with the fact so the two cannot drift: the batch states the fact.
_Avoid_: response (a Response is the model's), reply (also the model's), outcome alone (the ran-fact is one part of an Answer)

**Thinking**:
The model's reasoning stream, displayed but never fed back into the Conversation. Shown qwen-style in two parallel forms (verified against qwen-code v0.16.0): a persisted history item - a grey `✦` bullet with a markdown body that stays in the Transcript above the answer permanently, exactly like assistant text but dimmed - AND a transient subject line on the spinner (the latest subject replaces the waiting phrase while the model thinks, and vanishes when the Run goes idle). There is no per-item expand/collapse and no Ctrl-T; the only visibility control is a global compact toggle that hides Thinking and Tool output together.
_Avoid_: reasoning content, chain of thought

**Lull**:
A quiet stretch within a running Run - the Agent is Running but nothing is streaming (waiting on the first token, or a Tool executing). Distinct from Idle, which is no Run running at all. A Lull that outlasts a short settle (~5s) shows a whimsical waiting phrase with an elapsed timer and running token count on the spinner line; it vanishes the instant Thinking or answer text streams again (ADR-0041). Display-only, never in the Conversation.
_Avoid_: idle (that is no Run at all - the opposite state), stall, hang (a Lull is healthy waiting, not a fault)

**Transcript**:
The display-side history of a Session - everything the user saw, in order: user prompts, assistant text, Thinking, Tool Call and Tool Result summaries, and info lines. Not the Conversation: Thinking and info lines live in the Transcript but never in the Conversation. Rendered WHOLE into the fullscreen viewport from the model every frame (ADR-0046): settled items above, the still-changing Pending tail below, re-wrapped at the current size on every resize. History that runs off the top is reached by app-owned scrolling (mouse wheel, PageUp/PageDown, Home/End), not the terminal's own scrollback.
_Avoid_: message list, chat log

**Pending tail**:
The still-changing bottom of the Transcript - in-flight Tool Calls awaiting approval or a result, the streaming assistant message and its Lull spinner - sitting above the Composer and the status line. It is not a separate repainting zone: the whole Transcript renders each frame (ADR-0046), and the Pending tail is simply the items that are not yet final. The body is bottom-anchored and top-clips on overflow (qwen `overflowDirection:"top"`); when the user scrolls up it detaches from the tail and streaming output no longer yanks the view down. Every non-append correction - a Tool Result absorbing its paired Call line, Steering promoted to a user line - happens while the item is still in the Pending tail.
_Avoid_: Pending Region / inline viewport (retired with the inline model, ADR-0046), scroll pane, live lane

**Settle**:
The moment a Transcript item becomes final and immutable - a Tool Call gaining its Result, a streaming message ending. A settled item is never edited afterward, so corrections happen while the item is still in the Pending tail. (Historical: the retired inline model additionally "Committed" settled items into native scrollback - see ADR-0046; there is no Commit step now, the whole Transcript is redrawn each frame.)
_Avoid_: flush, Commit (the inline Commit-to-scrollback step is retired, ADR-0046)

**Screen**:
The pure UI core - the fold root owning everything the terminal shows: the Transcript (settled items plus the Pending tail), the Composer, the Approval prompt, the scroll intent, and the status-bar figures. Folds keys and Session events into new state plus effects; the adapter executes the effects and redraws the whole fullscreen frame from the model. Keys route through it in a fixed order: the Approval gate first, then the Composer's first refusal, then the Screen's own arms. Display-side only: nothing in the Screen enters the Conversation.
_Avoid_: Transcript (that's the display history the Screen owns, not the whole core), UI state (too vague), model (TEA jargon; the domain names the thing)

**Composer**:
The input area of the TUI where the user authors the next prompt. A leading `/` opens the fuzzy `/` **palette** (System B); a committed selector-opening command replaces it with its numbered **dialog** (System A). Both overlays are Composer states - the draft filters them, backspacing out of one re-enters the other, and Escape empties the Composer - not modals: only an Approval prompt takes keys away from the Composer. Otherwise a submitted draft starts a Run when the Agent is idle or becomes Steering when a Run is running. Display-side only: its draft is never part of the Conversation until submitted.
_Avoid_: input line (it is not a line; drafts may span many), prompt (that's what a submitted draft becomes), selector modal (an overlay is a Composer state; nothing in the UI is a centered modal - an Approval prompt is an inline block in the Pending tail that merely captures keys)

**Slash Command**:
A directive the user invokes from the Composer, never sent to the model. Typing `/` opens the fuzzy **palette** (System B, ADR-0051): color-only rows ranked by qwen's strength ladder (exact > prefix > segment > fuzzy), no marker, no numbers; Tab or Enter accepts. A selector-opening command (`/model`, `/theme`) then commits to its numbered `›` **dialog** (System A): the shared `SelectionList`, arrows + digit quick-select. The two systems are DISTINCT (never merged). Always available whatever the Agent is doing - a running Run never suppresses the palette, though a command's effect may land at a Run boundary (a model change applies to the next Run). Distinct from a prompt (which starts a Run) and from Steering (mid-Run user text that joins the Conversation unadorned): a Slash Command enters neither the Conversation nor the Transcript as user text; it drives the harness or the Session (e.g. choosing the model), and the Transcript may show its outcome as an info line. The set of commands is open - adding one is routine.
_Avoid_: command (that's the Agent's internal actor message), directive, colon-command; conflating the palette (System B, fuzzy) with a dialog (System A, numbered)

**Theme**:
A named coloring of the semantic display vocabulary - which colors the Transcript's semantics draw in, stated sparsely so a Theme lists only what it changes and everything unstated reads from the built-in default. Colors only: what a thing means, and the emphasis that meaning carries (bold, italic, underline), is never a Theme's to change. Display-side only - a Theme never enters the Conversation. Chosen live through the `/theme` Slash Command; the choice outlives the Session.
_Avoid_: color scheme (narrower; a Theme also names the code-highlighting look), skin, style (overloaded with the terminal's style type)

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
The token allowance the Conversation must fit within, derived each Run from the captured Model's context window. Config supplies the window for Models the Catalog does not know, and may cap it globally.

**Result Cap**:
The size ceiling one Tool Result may occupy in the Conversation, derived from the Context Budget the Run captured. Oversized Tool Results are cut before they enter the Conversation: run_command keeps its start and end (the exit code lives at the end), every other Tool keeps its start.
_Avoid_: output limit, truncation (reserved for the server's failure mode)

**Cancellation**:
The user aborting a running Run; the Conversation records a clean partial state.

**Run Settlement**:
How an ended Run enters the Conversation. Every Run settles exactly one way: completed, failed, or cancelled (a crash settles as a failure). A Run that did not complete settles on its partial state, closed with a marker so roles keep alternating.
_Avoid_: completion (reserved for the model's response), run management

**Middleware**:
A unit of extension that wraps a Tool Call's execution at two points: before the Tool executes (may adjust or deny the call, short-circuiting it) and after it executes (may transform the Tool Result the model sees). Middleware wrap one another, first-registered outermost - the onion familiar from Tower and Rack. Middleware never add Tools; they wrap existing ones. A denial still produces exactly one Tool Result, voiced by that Middleware. The execution-path counterpart to a Presenter (the display-path role); one registered extension may fill either role or both.
_Avoid_: plugin (its everyday meaning is "adds a capability"; a Middleware wraps an existing Tool and never adds one, the opposite connotation), hook (a hook is one attachment point in the lifecycle, not the unit), interceptor, filter

**Presenter**:
The display-side role an extension may fill: at Presentment it may substitute a richer Transcript item (a diff instead of a one-line summary) and carry an Artifact. Named apart from Middleware because it sits at a different seam - Middleware in the Tool Call's execution path, Presenter in the Transcript's display path - and a given extension often fills only one: a diff view is a Presenter only, a token counter a Middleware only. A Presenter never touches the Conversation.
_Avoid_: renderer (that's the terminal drawing the Transcript; a Presenter decides what the item is), formatter, plugin

**Presentment**:
The act of turning a Session event into a Transcript item. A Presenter may substitute a richer item at Presentment - a diff instead of a one-line summary - but Presentment only ever shapes the Transcript, never the Conversation.
_Avoid_: rendering (that's the terminal drawing the Transcript; Presentment decides what the item is)

**Artifact**:
Display-side data a Presenter derives from a Tool Call - a diff, an annotation - carried alongside the Tool Result to Presentment. An Artifact never enters the Conversation and costs no Context Budget.
_Avoid_: metadata, attachment

**Voice**:
Every Suspenders-voiced string the model reads: the system prompt, the compaction prompt, and every marker (run limit, cancellation, Result Cap cuts, malformed input, the run-close markers, and the error answers to a truncated or orphaned Tool Call). The Voice no longer authors any mid-Conversation steering - the nudge, anchor, and endgame apparatus is gone; the loop-detector's run-close marker is the only intervention text and it merely ends the Run. The boundary is voice, not arity: wording may be parameterized, but Suspenders authors it. Strings a Tool produces about its own execution stay in that Tool; strings a Middleware produces about its own decisions stay in that Middleware. Owned by one module so wording can be tuned per model in one place.
_Avoid_: prompt strings, constants, "static strings" (parameterized wording still belongs), Steering Vocabulary (legacy name; Steering now means mid-Run user input)

**Steering**:
User input delivered to a running Run. It never aborts in-flight work: the current model response and Tool Calls finish, then the steering text joins the Conversation before the next model call. Steering is the user's voice, not Suspenders's - it enters the Conversation unadorned, never marked. Distinct from Cancellation, which ends the Run.
_Avoid_: interrupt (nothing is interrupted), injection (mechanism, not meaning), follow-up (Steering that misses its Run rolls over; there is no separate follow-up concept)

**Rollover**:
What happens to Steering the Run ended before delivering: it auto-submits as the next Run's prompt. Cancellation discards it instead - cancel means stop everything.

**todo_write**:
The model's structured task list, held by the harness outside the Conversation and updated through the `todo_write` Tool (replacing the freeform `plan`). It is the model's voice - Suspenders never authors its content - and it survives Compaction verbatim because the harness owns it, not the summary. Shaped and named to match what a small model saw in training, so it calls it without translation (ADR-0045). Purely a record the model keeps for itself: the harness neither reads its checkbox structure to drive behavior nor injects it back into the Conversation. Displayed qwen-style in two forms: the `todo_write` Tool Call in the Transcript shows the list it wrote (circle glyphs, not brackets); and a live "Current tasks" summary box pinned above the Composer tracks current progress - numbered, the in-progress item floated to the top, completed items struck through, the rest shown to a cap with an "... and N more" tail.
_Avoid_: plan (the retired freeform name), scratchpad (free-form; the task list is structured), notes, Current tasks (that is one of its two display forms, not the record)

**Session Log**:
The durable, append-only record of one Session: its fixed facts, then every Conversation event in order, written as each happens. The Session Log records the Conversation, not the Transcript - Thinking and info lines are never in it.
_Avoid_: history file, transcript file (the Transcript is display-side and is not what's persisted)

**Resume**:
Reconstructing a Conversation from a Session Log so a new Session can continue where a crashed or exited one stopped. A log that ends mid-Run settles that Run as failed - a crash settles as a failure, discovered at Resume rather than at the moment of death.

## Relationships

- A **Session** has exactly one **Conversation**, one **Transcript**, and one **Project Root**
- A **Session**'s fixed facts are resolved and validated once at launch; every **Run** and **Tool Call** reads them from that value, never from ambient configuration
- The **Active Model** is the one thing a **Run** does NOT read from the fixed **Session** value: the **Agent** owns it mutably and each **Run** captures its whole **Model** when it begins, so a `/model` change lands on the next **Run**, never mid-flight
- A **Session** holds a fixed **Provider** set; the **Active Model** names one Provider's **Model**, and every request travels over that Provider's endpoint and credential through the adapter its **Api** selects
- The **Context Budget**, the **Result Cap**, and the reply reserve derive from the **Model** the **Run** captured, recomputed at each Run's start
- Every assistant message carries **Provenance**; request-shaping replays history that matches the target **Model** verbatim and normalizes the rest, so a **Conversation** crosses **Providers** without a restart
- A **Conversation** is a sequence of **Runs**
- A **Run** is one or more **Passes**; the **Run Limit** bounds the **Passes**
- A **Run** contains zero or more **Tool Calls**, each producing exactly one **Tool Result**
- Each **Pass** builds one request carrying the full **Tool** registry - there is no per-Pass narrowing; the **Agent** repeats **Passes** until the model returns no **Tool Calls** or the **Run Limit** is reached
- A **Tool Call** for run_command requires an **Approval** before execution, unless a **Standing Approval** covers its exact command string
- A **Standing Approval** belongs to the **Session** - it does not survive restart and never widens beyond the identical command string
- **Compaction** is the sole context-reclaim mechanism; when the **Conversation** cannot fit the **Context Budget** even after Compaction, the **Run** fails loudly and an over-budget request is never sent
- Every **Tool Result** is cut to the **Result Cap** before it enters the **Conversation**; the cap derives from the **Context Budget** the **Run** captured
- The system prompt, the compaction prompt, and every marker belong to the **Voice**; the **Voice** authors no mid-Conversation steering
- The **Loop-detector** ends a **Run** when the model repeats the byte-identical **Tool Call** batch `loop_stall_limit` times; it injects nothing into the **Conversation** and appends only a run-close marker
- The **Approval** gate encodes the user's judgment, not a tuned learning
- **Middleware** wrap every **Tool Call**'s execution at two points, before and after, wrapping one another first-registered outermost; a **Presenter** shapes the same Tool Call's **Presentment** on the display side
- A **Middleware** may adjust or deny a **Tool Call** before execution, and an **Approval** always shows the final, middleware-adjusted command
- A **Middleware** denial still produces exactly one **Tool Result**, voiced by that Middleware
- An **Artifact** a **Presenter** derives travels with its **Tool Result** to **Presentment** and appears only in the **Transcript**, never the **Conversation**
- A **Middleware** or **Presenter** failure never fails the **Run** and never reaches the model; the **Transcript** records it as an info line and the Tool Call proceeds without that extension
- A **Session** draws with exactly one active **Theme**; the `/theme` **Slash Command** changes it live, and the choice outlives the Session
- A **Theme** shapes only how the **Transcript** and the **Screen**'s chrome are colored, never what anything means; a broken Theme is refused whole, and the Session falls back to the built-in default rather than drawing half-right
- A **Run** ends in exactly one **Run Settlement**: completed, failed, or cancelled
- **Steering** is delivered after a Tool Call batch completes and before the next model call; a Run that ends first triggers **Rollover**, a **Cancellation** discards it
- **Steering** belongs to the user's voice and is never part of the **Voice**
- The model's **todo_write** task list is held by the harness outside the **Conversation** and survives **Compaction** verbatim; it is the model's own record and never re-injected into the **Conversation**
- **Compaction** fires at the **Compaction Target** and retains the **Compaction Keep**; the two are decoupled so Compactions are rare and deep
- A **Session** appends every **Conversation** event to its **Session Log** as it happens; **Resume** folds that log into a new Session's **Conversation**
- **Resume** requires the same **Project Root**; every other Session fact yields to the resuming Session's, and the **Transcript** notes what changed
- A **Tool Call** in a truncated response is answered with an error **Tool Result** (re-issue it) and none of its batch executes; only **Cancellation** drops a **Tool Call** from the **Conversation**, and the **Transcript** still shows it
- **Thinking** belongs to a **Run** and appears in the **Transcript**, but never enters the **Conversation**
- Every **Tool Call** executes within the **Project Root**

## Example dialogue

> **Dev:** "When the model hits the **Context Budget** mid-**Run**, do we drop old **Runs**?"
> **Domain expert:** "No - **Compaction** replaces old finished **Runs** with one LLM summary of what was accomplished, so the user's instructions survive as summary and the working tail stays verbatim. It is the only reclaim mechanism; there is no mechanical eviction."
>
> **Dev:** "And if the user cancels while a run_command **Approval** modal is open?"
> **Domain expert:** "**Cancellation** wins: the **Tool Call** is recorded as cancelled, no **Tool Result** is fabricated, and the **Run** ends."

## Flagged ambiguities

- "Turn" was the outer request cycle, but the ecosystem (pi, Anthropic, OpenAI) calls one **Pass** a "turn" - a standard word reassigned to the opposite level, a false friend for newcomers - resolved 2026-07: the outer cycle is now a **Run** (matching OpenAI's Run), and **Pass** keeps its coined name. This was a half rename by design: it removes the collision one-way without reassigning "turn". Adopting "turn" for a **Pass** (the full alignment) is a possible later step, deferred because it is a two-way rename sharing the token. "Run" brushes against run_command and the informal "run of the TUI" (now a **Session**); context disambiguates.
- **Middleware** and **Presenter** were one term, "Plugin", covering both the Tool Call execution wrap and the display-side enrichment - resolved 2026-07: split to name the two seams honestly. "Plugin" connoted "adds a capability", the opposite of what these do (they wrap existing Tools); the execution wrap is textbook **Middleware** (Tower/Rack onion), and the display role is a **Presenter**. One registered extension may fill either role or both. Supersedes the naming in ADR-0007 (plugin-lifecycle); the lifecycle itself is unchanged.

- The whole steering apparatus (Governor, Nudge, Anchor, Endgame, Recovery Run, Scout, and mechanical Eviction/Dead Mass/Supersession) was removed 2026-07 in favor of a plain ReAct loop, a strong static system prompt, and a passive **Loop-detector** (ADR-0045). The wager: a clean, predictable context keeps a small local model on task more often than corrective text pulled it back. Those terms are retired from the language; git history holds their design.

- "invocation" previously meant a parsed text-protocol tool request (`extract_invocations`); with native tool calling it is retired in favor of **Tool Call**.
- the **Compaction Target** trigger and the **Compaction Keep** recency were once one knob; resolved 2026-07: they are decoupled - fire high, keep low.
- "toggling thinking" was read as enabling/disabling the model's **Thinking** when it once meant expanding/collapsing settled Thinking items - resolved 2026-07-29 by the qwen-code port (ADR-0046) and verified against qwen-code v0.16.0: there is no Ctrl-T and no per-item expand/collapse. Thinking persists as a dimmed `✦` history item (markdown body) AND shows a transient subject on the spinner line; a single global compact toggle hides Thinking and Tool output together. Whether the model thinks at all remains a request-level knob (today fixed: on for the main Conversation) with no user-facing toggle.
- the **Compaction Keep** is configured and validated in token-space (a fraction of the live window), but the cutoff walk accumulates raw chars, so the executed keep is ~3.5x smaller than the configured fraction - discovered 2026-07-21, dates to the original port. Deliberately preserved and pinned by test for now; whether to fix the units (and retune the default) is an open tuning decision.

## Compaction

**Compaction**:
Replacing old messages in the Conversation with a structured LLM-generated
summary so the model can continue working within the Context Budget. The sole
context-reclaim mechanism: it calls the LLM to extract what was accomplished,
what decisions were made, and what files were touched. Semantic, not mechanical
- there is no bespoke elision path beside it.
_Avoid_: summarization, context compression (those describe the mechanism,
not the policy)

**Compaction Keep**:
The amount of recent Conversation that survives a Compaction verbatim. Set
well below the Compaction Target so Compactions arrive rarely and each one
summarizes a large, coherent span of finished work - many Runs then run
append-only between Compactions, keeping the server's prompt cache warm.
Its own knob, deliberately decoupled from the trigger: fire high, keep low.
_Avoid_: keep_recent (implementation name), recent-budget

**Compaction Target**:
The token estimate at which Compaction fires. The Conversation is proactively
compacted at a Run boundary when its estimate exceeds this target, and
reactively mid-Run when the fit-check finds the request still over budget.
_Avoid_: compaction threshold (ambiguous with Compaction Keep)

**Proactive Compaction**:
Compaction triggered at the start of a Run, before its first Pass, when the
Conversation's token estimate already exceeds the compaction target. Runs in
the Run task like every Compaction, so the Agent stays responsive. Prevents
hitting the budget cliff during the Run.

**Reactive Compaction**:
Compaction triggered mid-Run when building a request finds the Conversation
over budget (the fit-check returns exhausted) - the last recovery attempt
before the Run fails.
