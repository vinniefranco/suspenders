# Suspenders

A terminal coding agent for small local models: a full-screen TUI where a locally-served LLM completes coding tasks in your project by calling tools.

Big hosted models tolerate a sloppy context. Small local ones don't - their attention rots on stale tool output and superseded edits long before you run out of tokens. Suspenders is built around that constraint. It evicts dead content, compacts old history into summaries, and runs a set of governors that watch the model and intervene when it drifts. The goal is to keep a model small enough to run on your laptop on task long enough to finish one.

It ports an earlier Elixir project (Baud/Breeze); this is the Rust rewrite.

## What it does

Launch it in a project, describe a task, and the model reads code, greps, edits files, and runs commands until the work settles. The TUI streams the model's thinking, collapses tool results into summaries, shows diffs instead of prose for writes, and gates `run_command` behind an approval prompt. Every session is written to an append-only JSONL log you can resume from.

Tools: `read_file`, `write_file`, `edit_file`, `list_files`, `grep`, `run_command`, `explore` (dispatches a read-only scout), `plan`, `web_fetch`.

## How the context stays alive

- **Eviction**: stale content (old tool results, superseded blocks, the bodies of successful writes) is mechanically replaced with an elision marker. Dead mass has its own trigger, separate from budget pressure, because it rots attention first.
- **Compaction**: when the conversation crosses its target, old blocks are summarized by the model and recent history is kept verbatim.
- **Governors**: tunable rules that watch each pass and intervene through a closed set of actions (nudge, refresh the plan anchor, narrow the offered tools, close the turn, open a recovery turn). Anchor, Duplicate, Empty, Failure, Explore, Endgame.
- **Endgame**: a turn ends at its turn limit on a fixed schedule, not on a request. Wrap-up warning, then a verification pass (`run_command` only) if writes are unverified, then a final pass with no tools. Small models comply with mechanics, not asks.
- **Recovery turns**: if a turn caps out with unverified writes or a dangling failure, the harness issues one more bounded attempt. Default shape is Handoff: retire the degraded conversation, seed a fresh one from a structured summary.

The vocabulary is deliberate. `CONTEXT.md` is the glossary, the ubiquitous language of the domain and the single source of truth for what these words mean. Architecture decisions live in `docs/adr/`. The module tree mirrors the glossary (ADR-0022).

## Build

```
cargo build --release
cargo nextest run
```

Stable toolchain (see `rust-toolchain.toml`). A Nix flake is provided: `nix develop` for a dev shell.

## Run

```
suspenders                          # TUI in the current directory
suspenders --root path/to/project   # elsewhere
suspenders --resume latest          # continue the last session
suspenders --resume                 # pick a session from the log
suspenders --headless "do the task" "then this one"   # no TUI, events to stdout, run_command auto-approved
```

`--headless` runs each prompt as a sequential turn in one session. It's the diagnostic harness: use it to watch the agent work without the TUI in the way.

## Configure

Config resolves once at launch, in precedence order: built-in defaults, then `config.json`, then the `SUSPENDERS_*` environment.

Write the default template and edit it:

```
suspenders --write-config           # XDG default path
suspenders --write-config ./suspenders.json --force
```

The model connection points at a local Anthropic-wire-protocol endpoint by default (e.g. `http://localhost:8888/v1`). This talks the Anthropic Messages API over SSE, aimed at whatever local server speaks it. Key knobs:

| Field | Default | Notes |
|---|---|---|
| `base_url` / `token` / `model` | local endpoint | the model connection |
| `max_tokens` | 8000 | output cap; the eviction budget derives from it |
| `temperature` | 0.7 | sampling temperature, omitted if unset |
| `context_budget` | 64000 | token allowance for the conversation |
| `turn_limit` | 32 | max passes in one turn |
| `command_timeout_ms` | 120000 | `run_command` timeout |
| `result_cap` | derived | per-tool-result size ceiling |
| `plugins` | `diff`, `run_command` | presentment + approval-gate plugins |

Each field has a `SUSPENDERS_*` env override (`SUSPENDERS_URL`, `SUSPENDERS_TOKEN`, `SUSPENDERS_MODEL`, `SUSPENDERS_CONTEXT_BUDGET`, and the rest). The setpoints that encode small-model tuning (eviction slack, dead-mass fraction, compaction keep, recovery limit/shape, malformed-retry budget) are overridable the same way.

## Layout

```
src/main.rs        CLI parsing
src/app.rs         composition root: run_tui / run_headless
src/agent.rs       orchestrator: runs turns, spawned over channels (ADR-0017)
src/turn/          the pass loop, batch execution, settlement, governors
src/llm.rs         Anthropic Messages API over SSE; injected Llm trait for tests
src/tools/         the nine tools
src/scout.rs       disposable read-only worker
src/conversation.rs / compaction.rs   history + the summary that shrinks it
src/plugins/       diff presentment, run_command approval gate
src/ui/            ratatui TUI: viewport, composer, transcript, streaming, slash menu
src/voice.rs       every Suspenders-authored string, in one place to tune per model
```
