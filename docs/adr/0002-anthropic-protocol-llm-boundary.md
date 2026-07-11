# Anthropic Messages API behind a single LLM boundary

Speak the Anthropic Messages API (`/v1/messages`) as the wire protocol against local servers, and confine all HTTP and SSE implementation to one `llm` module behind an `Llm` trait. Everything else speaks the project's own typed structs.

Local servers serve the Anthropic API natively, with first-class thinking blocks and structured `tool_use` — both load-bearing for a coding agent on reasoning models. One boundary module contains the coupling to the wire format, so any future swap is a one-module change.

## Implementation

reqwest carries the transport, streaming the response body with `bytes_stream()`. The `eventsource-stream` crate handles SSE framing. A **pure `fold_sse` function** turns the sequence of parsed SSE events into a Response — blocks, `stop_reason`, and usage — with no I/O, so it is testable against canned event lists. `fold_sse` absorbs the server quirks: the missing-`index` case, exclusion of thinking blocks from the Conversation, and `tool_use` `input_json_delta` reassembly.

## Error algebra

The boundary **never returns `Err` and never panics** for transport or stream failures. Connection refused, a non-2xx status, an SSE parse failure, and mid-stream death all yield a Response carrying an `Error` stop_reason plus whatever partial content had streamed. Failure is data the Turn loop reads, not an exception it must catch.

Considered and rejected:

- **OpenAI-compat wire format.** No longer a portability win, and it has weaker thinking and tool-use semantics.
- **A higher-level EventSource client with auto-reconnection.** Fights the one-shot stream and hides the mid-stream-cut error path this boundary must expose.
- **Hand-rolled SSE framing.** Reinvents a solved problem.
