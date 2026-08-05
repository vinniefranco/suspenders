# Suspenders

A local-first terminal coding agent: a full-screen TUI where an LLM - a locally-served small model by default, any configured provider's model when you switch - completes coding tasks in your project by calling tools.

Big hosted models tolerate a sloppy context. Small local ones don't - their attention rots on stale tool output and superseded edits long before you run out of tokens. Suspenders is built around that constraint. It compacts old history into summaries before the window fills, and pairs that with a strong static prompt and a plain ReAct loop tuned for small local models. The goal is to keep a model small enough to run on your laptop on task long enough to finish one.

It ports an earlier Elixir project (Baud/Breeze); this is the Rust rewrite.

## What it does

Launch it in a project, describe a task, and the model reads code, greps, edits files, and runs commands until the work settles. The TUI streams the model's thinking, collapses tool results into summaries, shows diffs instead of prose for writes, and gates `run_shell_command` behind an approval prompt. Every session is written to an append-only JSONL log you can resume from.

Tools (named to match qwen-code, so a small local model calls them without translation): `read_file`, `write_file`, `edit`, `notebook_edit`, `list_directory`, `glob`, `grep_search`, `run_shell_command`, `web_fetch`, `todo_write`, `ask_user_question`, `skill`, `agent`, plan-mode tools, and `tool_search` for on-demand discovery - plus any MCP tools you attach (`suspenders mcp add`, the `/mcp` dialog).

## The unit of work

A **run** is one user request and everything the agent does to answer it. Within a run the loop repeats a **pass** - one model response and the tool calls it carries - until the work settles or the run hits its limit. (Most of the ecosystem calls a pass a "turn"; Suspenders reserves "run" for the whole request. See `CONTEXT.md`.)

## How the context stays alive

- **Compaction**: when the conversation crosses its target, old blocks are summarized by the model and recent history is kept verbatim. This is the only context-reclaiming path (ADR-0045).
- **Compaction slack**: `compaction_slack` carves headroom below the Context Budget, lowering the compaction target so a summary fires before the window is truly full.

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
suspenders --headless "do the task" "then this one"   # no TUI, events to stdout, run_shell_command auto-approved
```

`--headless` runs each prompt as a sequential run in one session. It's the diagnostic harness: use it to watch the agent work without the TUI in the way.

## Models and providers

The LLM boundary is a set of **providers**, each speaking one wire **Api** through a hand-written adapter (`anthropic-messages`, `openai-completions`). Out of the box Suspenders points at a `local` provider (`http://localhost:8888/v1`, Anthropic Messages over SSE), aimed at whatever local server speaks it. Built-in providers (anthropic, openai, ...) come from a generated Catalog and need only their environment key; custom providers (a local LM Studio, a private proxy) are declared in config and discover their models live. The active model is a scoped `provider/model-id`; switch it live with the `/model` slash command and the change lands on the next run.

## Configure

Config resolves once at launch, in precedence order: built-in defaults, then the user `config.json`, then the workspace `.suspenders/config.json`, then the `SUSPENDERS_*` environment.

Write the default template and edit it:

```
suspenders --write-config           # XDG default path
suspenders --write-config ./suspenders.json --force
```

Keys in `config.json`:

| Field | Default | Notes |
|---|---|---|
| `model` | `local/qwen/Qwen3.6-27B-MTP-GGUF` | the active model, scoped `provider/model-id` |
| `providers` | `local` → `localhost:8888/v1` | a table of providers, each with `base_url`, `api`, and optionally `token` / `context_window` |
| `max_tokens` | 8000 | output cap; the reply reserve derives from it |
| `temperature` | 0.7 | sampling temperature, omitted if unset |
| `context_budget` | model window | optional global cap on the conversation's token allowance; unset means each model's own window is the budget |
| `theme` | `dark` | color theme; `/theme` switches it live |

The setpoints that encode small-model tuning (`compaction_slack`, `compaction_keep`, `malformed_retry_budget`, `loop_stall_limit`, `tool_call_style`) are config keys too, each with a `SUSPENDERS_*` env override.

## Layout

```
src/main.rs        CLI parsing (plus the `suspenders mcp` subcommands)
src/app.rs         composition root: run_tui / run_headless
src/agent.rs       orchestrator: runs runs, spawned over channels (ADR-0017)
src/run/           the pass loop, batch execution, hook firing, settlement
src/llm/           Api adapters (anthropic-messages, openai-completions), providers, the Catalog (ADR-0037)
src/tools/         the tools
src/hooks/         the Hook subsystem: config-driven lifecycle interception (ADR-0066)
src/skills/        Skill discovery for the `skill` tool's catalog (ADR-0058)
src/mcp/           the MCP client: transports, adapters, the manager (ADR-0056)
src/conversation.rs / compaction.rs   history + the summary that shrinks it
src/ui/            ratatui fullscreen TUI: screen, transcript, composer, themes (ADR-0046)
src/voice.rs       every Suspenders-authored string, in one place to tune per model
```
