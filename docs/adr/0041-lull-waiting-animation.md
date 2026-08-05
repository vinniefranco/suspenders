# The Lull: a whimsical waiting animation for the quiet of a running Run

A Run is not always streaming. Against a slow local model there are long
stretches where the Run is running yet nothing streams - waiting on the first
token, or a tool executing - and a screen with no motion is indistinguishable,
at a glance, from a hung process. This ADR records the Lull - the animation that
fills that silence - and the decisions that keep it cheap, safe, and deterministic.

## Decision

**A Lull is a quiet stretch WITHIN a running Run - not the Agent being Idle.**
The word matters: `Status::Idle` already means "no Run running", and the status
bar has an idle segment. The Lull is the opposite - the Run *is* running, it is
just momentarily silent. The naming is pinned in CONTEXT.md so the distinction
does not erode.

**The Lull draws on the spinner line, not as a row of its own.** The fullscreen
transcript body (ADR-0046) shows one spinner line while the Agent is Running;
the current Lull scene is that line's phrase content, displaced by the thought
subject when Thinking streams. The lull clock resets whenever output streams -
the gate is `Screen::has_live_stream()`, one predicate (reasoning OR answer text
streaming) - so the phrase and the clock can never disagree about whether a Lull
is happening.

**The clock is tick-counted, never wall-clock.** The adapter already ticks ~10fps
and repaints while a Run runs (the spinner). The Lull rides that same tick:
a `quiet_ticks` counter resets whenever output streams and increments on every
quiet tick; a `lull_seq` counter bumps on each 0→1 edge (a new Lull begins). No
`Instant`, no timer thread - the codebase bans `Date::now`/`rand`-style
nondeterminism, and tick-counting keeps the whole thing reproducible and testable.
The animation clocks (spinner, quiet_ticks, lull_seq) are consolidated into one
`Anim` value object, so the render path takes a single animation argument and a
future clock is a field, not another parameter.

**Scenes are picked pseudo-randomly per Lull, deterministically.** A *Scene* is a
frame list plus its pace; the `SCENES` registry holds them, and adding one is a
single entry (the extension seam). Each new Lull picks a Scene by hashing its
`lull_seq` (a SplitMix64 finalizer) modulo the registry size - random-feeling
wait-to-wait, but a pure function of a counter, so the lull→scene map is fixed and
unit-testable. Within a Lull the frame advances with elapsed quiet time.

**An elapsed timer sits to the left of the animation.** `2m 03s` in a fixed-width
field so the animation column never jitters as the label grows. Ticks become
seconds at exactly one place - the display boundary - via `TICK_MS`; the pure
`lull` module stays in tick-space. The timer is the "still going, and here's how
long" signal directly, which is why the Scene choice does not also need to escalate.

**Only a wait past ~5 seconds earns the whimsy.** `SETTLE_TICKS` (=50 at
100ms/tick) gates the whole row: a brief gap between tokens never flashes a Scene,
and the timer opens at `5s`. Short, common lulls stay quiet; only a slow-enough
wait gets the animation.

**Single-row is a hard Scene invariant.** Each frame is one visual row, and the
row is truncated to the content width before drawing. Mixed half/full-width glyphs
(the kaomoji Scene) cannot be trusted to a stable display width across frames - the
same class of bug the `🧠` emoji width once caused for the spine - so safety comes
from single-row placement plus truncation, not from asserting equal frame widths.

The Lull draws in its own `lull` theme slot (ADR-0008: one semantic → one color),
so the whimsy is themed independently and can be brighter than the muted chrome.

## Considered and rejected

- **Escalate the Scene by elapsed time** (advance to a livelier Scene the longer a
  Lull runs, so a Scene change signals "still going, just slow"). Rejected in favor
  of random-per-Lull plus the explicit timer: the timer already carries the
  "how long" signal precisely, and random keeps a long session's waits feeling
  fresh rather than marching a fixed escalation ladder.
- **A real RNG (`rand`, or seeding from the clock).** Rejected: nondeterministic
  and untestable, and the repo bans `Date::now`/`Math.random`-class calls. Hashing
  a per-Lull counter gives the same variety with a fixed, provable map.
- **Multi-row ASCII scenes.** Rejected: a width wobble on any row breaks the
  measure==draw contract (ADR-0029). Single-row + truncate is the safe
  envelope; richer art is not worth reintroducing the emoji-width bug.
- **A separate animation timer/thread.** Unnecessary: the adapter's existing
  running tick already repaints ~10fps, and the Lull clock is just two counters
  advanced in that same tick - no new machinery, and it stays deterministic.
- **Reusing the `muted`/`thinking` theme slot.** A dedicated `lull` slot keeps
  ADR-0008's semantic→color mapping honest and lets the animation be brightened or
  themed without dragging the muted chrome with it.

## Consequences

The Lull is entirely adapter-local and cheap to reverse: a pure `ui::lull`
module (no ratatui, ADR-0019) that maps the clock to the glyph to draw, the
spinner line reading it for its phrase, and the lull-clock maintenance in the
one existing tick arm. The store is untouched - the Lull is a render-time
projection, nothing persisted. The animation state travels as the `Anim`
value object through the render path, and `TICK_MS` is `pub(crate)` so the
display side turns ticks into seconds at the same cadence the adapter ticks.
One theme slot (`lull`) sits at ADR-0008's vocabulary chokepoint; both built-in
themes carry it (light is total, ADR-0038). The spinner line is measured and
drawn at `content_area.width` and truncated to one visual row, so measure and
draw cannot desync (ADR-0029).
