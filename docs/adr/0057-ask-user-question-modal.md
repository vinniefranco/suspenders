# The ask_user_question modal is a question round-trip parallel to approval

The model sometimes needs a live decision from the user mid-run - a preference, a clarification, a direction to take - and asking in plain prose loses that behind free text the model then has to re-parse. qwen's `ask_user_question` tool solves this with a structured modal: one to four questions, each a short header chip plus two to four options, and the user's picks come back as a formatted answer string. This ADR ports that faithfully.

## The round-trip is parallel to the Approval round-trip (ADR-0049, ADR-0055)

The question path mirrors the Approval path at every seam, because they are the same shape: a tool reaches back to the host for a decision the model cannot make, over the Agent mpsc, and parks until the user answers.

```
ask_user_question::run
  -> ctx.caps.questioner.ask(questions)        (the Questioner capability, ADR-0055)
  -> AgentQuestioner mints a question id, sends RunMsg::AskQuestion{id, questions, reply}
  -> Agent ask_question handler: insert reply into question_replies, broadcast Event::QuestionRequest
  -> Screen::apply_question builds PendingQuestion{cursor, per_question, answers, collecting_other}
  -> handle_question_key drives the per-question SelectionList
  -> AgentCommand::AnswerQuestion(id, Ok(answers) | Err(decline))
  -> ui::run_agent_command -> agent.answer_question(id, answers)
  -> Command::AnswerQuestion -> Agent answer_question handler: remove reply, send answers
  -> AgentQuestioner reads the reply, emits Event::QuestionResolved, returns to the tool
  -> the tool formats the answers VERBATIM and returns them as its content
```

Every arrow has an Approval twin (`RequestApproval`/`Approve`, `ApprovalRequest`/`ApprovalResolved`, `PendingApproval`/`handle_approval_key`/`resolve_approval`). The Questioner is the SECOND real consumer of the Capability Context (ADR-0055) after web_fetch's SideQuery, and unlike SideQuery it travels the Agent mpsc like the Approver (a question is an Agent-relayed, user-owned decision, not a bare Llm call).

## There is NO auto/standing path - every question opens a modal

The one deliberate DIVERGENCE from the Approval shape: `request_approval` consults the Standing Approvals and may answer immediately with an `approval_auto` (the Run cannot tell the difference). A question has no such fold. `ask_question` is unconditionally the "pending" leg - it inserts the reply and broadcasts the request, always. There is no "standing answer", no auto-resolve, no mode. A question is a fresh decision every time; caching an answer would be a lie the next question inherits. So `question_replies` is the whole mechanic (a `HashMap<id, oneshot>`), with none of the `Approvals` fold's standing/mode machinery beside it.

## The modal reuses the SelectionList and the composer (ADR-0049, ADR-0051)

The modal is built from the two pieces the Approval modal already established:

- **The numbered `›` dialog (ADR-0051 System B, the `SelectionList`).** Each question gets its own `SelectionList` over its option labels PLUS one auto-appended "Other" row (qwen ALWAYS appends "Other" so the user can answer free-form). Arrows navigate, Enter selects, a digit quick-selects - the exact mechanic the Approval radio drives, unchanged. The modal walks the questions in order via a `cursor`; selecting a real option records its label and advances, and when the cursor passes the last question the round-trip resolves (mirroring `resolve_approval -> clear_approval -> FocusComposer`).
- **The composer, for the "Other" free-form answer.** Selecting the "Other" row sets `collecting_other = Some(i)` and focuses the composer. While collecting, the question gate defers keys to the composer (the answer is a draft), and intercepts the eventual submit/steer effect - the "answer is ready" signal - to fill `answers[i]` from the draft instead of sending a prompt. This reuses the composer as the free-text surface rather than inventing a second text input. Escape while collecting backs out to the radio; Escape on the radio declines the whole round-trip (qwen's `Cancel` outcome = "User declined to answer the questions.").

The modal renders as its own rounded box (`box_top`/`box_row`/`box_bottom`) appended BOTTOM-MOST in the pending body, so the top-clip never eats the questions - the same rule the open Approval follows, since the Run is waiting on the USER. Unlike an Approval (which attaches INSIDE a confirming ToolCall's box, ADR-0049), a question is not tied to a transcript ToolCall, so it draws standalone. Every row is padded to the box width (measure==draw, ADR-0029). The sticky "Current tasks" box and composer focus both key off `pending_question.is_some()` (with the `collecting_other` exception for focus), exactly as they key off `pending_approval.is_some()`.

## Tool-level validation, VERBATIM messages

suspenders' schema-level `tool::validate` only checks required/unknown/string-type, so the shape rules qwen enforces in `validateToolParams` (1-4 questions, header <= 12 chars, 2-4 options, non-empty strings) are re-implemented in the tool's `run` BEFORE it reaches the capability. Every message is VERBATIM from qwen (`Question {i+1}: "header" must be 12 characters or less.` and the rest), so a model tuned against qwen's messages sees the same corrections. The answer formatting is VERBATIM too: `**{header}**: {value}` per answer, joined by newlines, wrapped in `User has provided the following answers:\n\n{...}`. The degraded (headless/non-interactive) and decline strings are the exact qwen strings.

The tool is kept ALWAYS-VISIBLE (qwen `shouldDefer: false`) - it does not override `should_defer`, so it stays on the wire list - so the model reaches for the structured clarification UX instead of asking in plain prose.

## multi_select scope

Single-select is implemented fully faithful: one option label per question, joined-label semantics reserved for `multi_select`. `multi_select: true` is accepted, validated, and shaped through the capability, but the modal renders it as a SINGLE-select radio - the STUB. The `SelectionList` is a single-active-row radio (ADR-0051); a faithful multi-select would need a toggle+confirm mode (space toggles, Enter confirms, join the selected labels) it does not have today, and building that mechanic was out of scope for the must-have (single-select faithful). A `multi_select` question therefore yields exactly one label like any other, rather than a joined set. This is the one documented deviation; the capability signature already carries the joined-label contract (`answer_value` is a single String per question, joined for multi-select), so a later phase adds the toggle mode to the `SelectionList` without touching the round-trip.

## Considered and rejected

- **A standing/auto answer path like Approvals.** A cached answer would be applied to a later, different question the model asked for a fresh reason. A question is always a fresh decision; there is no safe "auto". So the Agent has no `Questions` fold beside `Approvals` - just the reply map.
- **A second text-input widget for "Other".** The composer already IS the text surface, focused and drawn every frame. Routing the free-form answer through it (intercepting the submit) reuses that instead of a parallel input with its own cursor/editing rules.
- **Attaching the modal to a transcript ToolCall like the Approval.** A question is not a gated action on a specific call the user reads the command of; it is a standalone prompt. Drawing it as its own box keeps it independent of the tool-group rendering and the newest-live-ToolCall attachment (ADR-0049).
- **Full multi_select now.** See the scope section: single-select faithful is the must-have; the toggle mechanic is deferred with the contract already in place.
