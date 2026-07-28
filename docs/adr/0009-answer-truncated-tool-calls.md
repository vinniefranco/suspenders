# Truncated Tool Calls are answered with an error, not dropped

Supersedes the truncation path of ADR-0004; the Cancellation path there stands.

When a response stops at `max_tokens` with `tool_use` blocks present, the arguments of the last block may be cut mid-JSON - and a cut argument set can still parse as valid JSON that is silently incomplete. Executing it is dangerous (a truncated `write_file` destroys a file); dropping it (ADR-0004) hides the event, and small models tend to re-derive the same overlong response.

New rule: every Tool Call in a truncated response is answered with an error Tool Result, Voice-worded, telling the model the response was cut and the call must be re-issued. None of the batch executes. The loop continues to the next model Pass instead of ending the Run, so recovery happens in-band, bounded by the Run Limit.

ADR-0004 rejected error-answering because "small models treat fabricated errors as failures to retry." For truncation, the retry is the point - the model should re-issue the call. For Cancellation the retry is unwanted, so there the drop rule remains: no Tool Result is fabricated for a call the user killed.

Considered and rejected:

- **Drop and auto-continue.** Avoids fabricated results, but the model cannot see which call was cut, so retries are blind.
- **Execute calls whose JSON parsed cleanly.** A clean parse does not prove the arguments are complete; only the model knows what it meant to send.

Consequence: the pairing invariant now holds two ways - by answering (truncation) or by dropping (Cancellation). Both are constructions, never repairs.
