# Slash Commands: a pure-core menu over an extensible registry

The Composer had exactly one job - a submitted draft became a prompt (idle)
or Steering (running) - and text entered the Conversation verbatim. We want
`/`-prefixed commands (CONTEXT.md: **Slash Command**) that drive the harness
instead of the model, and we want adding a command to be a one-line change so
`/model` is merely the first of many (`/theme`, `/compact`, …).

## Decision

Typing `/` in the Composer opens a **menu** of the available commands that
filters as the user types (pi's command palette), rather than parsing the
command only on submit. The menu, its filtering, cursor movement, selection,
and unknown-command handling all live in the **pure `Transcript`** (ADR-0001's
TEA core), where every other Composer rule already lives and is unit-tested -
no crossterm, no I/O.

**A `&'static` registry of command descriptors** is the extension seam. A
descriptor is just `{ name, help, produce-an-Effect }`. The pure core looks a
submitted/selected command up in the registry and emits a new
`Effect::Command(…)` variant; it never learns what any command *does*. The
actual work runs adapter-side (`ui.rs`) in that Effect's arm, because commands
need terminal ownership, network round-trips, or Agent commands - none of which
belong in the synchronous fold.

**One generic inline filterable-list selector** backs both the command menu
and any command's own list (the `/model` model list, a future `/theme` theme
list). It renders as an inline popup anchored above the Composer, folds the
shared `Key` vocabulary, and is a pure component beside `ui::picker`. The
command menu and a command's selector are the same shape - a filterable
single-select overlay - so trailing text after a committed command name filters
that command's list continuously (`/model qw` filters models to "qw").

**Always available, whatever the Agent is doing.** A leading `/` opens the menu
whether idle or running - a running Turn never suppresses it, and a leading `/`
is therefore never Steering text sent to the model. A command's *effect* may
still land at a Turn boundary (a `/model` change applies to the next Turn), but
the menu itself does not gate on Agent state. Unknown `/foo` resolves to a
Transcript info line, never a Turn.

## Considered options

- **Parse the command only on submit** (no live menu) - rejected: the user
  asked for pi's behavior, where `/` opens a discoverable, filtering menu; a
  palette beats memorizing command names, and the menu is the natural home for
  per-command help.
- **Handle commands in the `ui.rs` adapter** - rejected: parsing and menu state
  would then be untested-by-design (ADR-0001 splits the pure core from the
  adapter). Keeping recognition in `Transcript` keeps it unit-tested; only the
  side-effecting work crosses into the adapter, through an `Effect`.
- **A per-command bespoke UI** - rejected: `/model` and `/theme` are both
  "filter a list, pick one." One generic selector means the second command is a
  descriptor plus an Effect arm, not a new widget.
- **Leading `/` as Steering while a Turn runs** - rejected as surprising: typing
  `/model` mid-Turn would steer the model with the literal text.

## Consequences

- Adding a command = one registry entry + one `Effect::Command` arm. The menu,
  filter, and selector are untouched.
- The cost of "always intercept `/`": the user cannot steer the model with text
  that literally begins with `/`. Accepted (and pi-consistent).
- `/model` (ADR-0033) is the first consumer and exercises the async path
  (network fetch behind a selector); it is not special-cased in the spine.
