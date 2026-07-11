# Scouts run no-think, and the cap's last Pass forces the report

ADR-0013 kept thinking enabled at every call site (main loop, Scout,
Compaction): uniform behavior over server-dependent toggles. Live driving
on 2026-07-10 (Qwen3.5-9B, llama.cpp b9870) broke that uniformity for the
Scout: every explore dispatch in a real Session died in ~4s with
"[scout returned no findings]", and the resulting consecutive-failure
streak ended the Turn as turn-limit-stuck with no evaluation delivered.

A direct A/B probe (same tasks, same server) was decisive:

- **Thinking on, 5/5 fatal** (3 live dispatches + 2 probe trials): the
  known Qwen3.5 quirk - the model fails to exit thinking into prose after
  Tool Results - kills the Scout at Pass 2. Its final text is empty, so
  the report is empty. The main Turn loop survives this quirk via the
  Empty-response Nudge and the no-think rescue; the Scout loop has
  neither.
- **Thinking off**: accurate, well-structured reports (7 fast Passes,
  11.7s wall vs the 30-90s think-era Scouts), except one trial that kept
  exploring until the Pass cap fired with empty partial findings.

So thinking is not load-bearing for Scout search quality, and on this
model class it is fatal. The probe's one no-think failure exposed a
second, independent gap: nothing ever tells a tool-hungry Scout to stop
searching and write.

## Decisions

### Scouts run without thinking by default

A Scout no-think Session setting (default true) makes every Scout request
carry the no-think request flag - the same wire field the Empty-response
Nudge's rescue uses (a chat-template argument that disables thinking). The
mechanism lives in the Scout as a plain no-think option (default false);
the policy lives in the Session, which supplies it from config. Turn it
off for strict Anthropic-compatible servers that reject unknown request
fields - the caveat is identical to the no-think rescue's.

This supersedes ADR-0013's "thinking stays enabled at every call site"
for exactly this call site. The uniformity argument assumed thinking was
merely slow; the probe showed it is fatal for a worker whose entire value
is its final text. The main loop and Compaction keep thinking.

### The last permitted Pass is the forced report Pass

When a Scout reaches its Pass limit, the request offers NO Tools and the
Conversation gains a Voice-owned user message telling the Scout to report
now: the only move left is the report. Exploration gets one fewer than
the Pass limit; the cap now yields a report built from what the Scout has
seen instead of a partial-findings error. The pass-cap error outcome
remains for the degenerate paths (a Scout that outgrows its own Context
Budget, or a server that returns Tool Calls that were never offered).

## Consequences

- explore becomes reliable on Qwen3.5 instead of failing every dispatch,
  and Scouts get faster (no thinking tax per Pass), so the Explore Nudge's
  push toward Scouts is once again pushing toward something that works.
- An empty report now carries the Scout's last accumulated text as the
  partial, so a late failure still returns whatever was found.
- If the upstream think-exit bug is fixed (periodic re-probe, see the
  fine-tuning agenda), disabling Scout no-think restores ADR-0013 behavior
  without code changes.
