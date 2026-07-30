# A user config file overlays base defaults, below the environment

The Session's fixed facts resolve in one composition seam
(`session.rs`): `base()` hardcoded defaults, overlaid by `SUSPENDERS_*`
env vars (`try_from_env`), overlaid by `SessionOpts` (the CLI/programmatic
keyword opts), then validated once. Env is the only persistence a user
gets, and it is per-invocation - there is no home for "my model, my
endpoint, my budget" that survives a shell without an exported var or a
wrapper script. Re-exporting `SUSPENDERS_TOKEN` every session is the daily
friction.

The `.suspenders/` folder already exists, but it is **project-scoped**
and git-tracked (`context_files.rs`: `SYSTEM.md`, `AGENTS.md`, …). User
config is a different scope - per-user, secret-bearing, not committed -
so it does not belong there despite the tempting name collision. Session
Logs already established the per-user precedent: they live under
`$XDG_DATA_HOME` (ADR-0010). Config pairs with that as `$XDG_CONFIG_HOME`.

## Decision

A sparse JSON config file at
`$XDG_CONFIG_HOME/suspenders/config.json` (falling back to
`$HOME/.config`, resolved manually to mirror `default_session_dir`) sits
in the composition **between `base()` and the environment**:

```
base()  →  config.json  →  SUSPENDERS_*  →  SessionOpts  →  validate()
```

The file is the user's persistent baseline; the environment still wins
per-invocation over it. The composition entry is renamed `load()` /
`try_load()` (`from_env` becomes the internal env-overlay step it always
was) because the seam is no longer env-only.

**Schema is exactly the env-settable key set.** The file and the env
seam are two serializations of one schema - `base_url`, `token`, `model`,
`max_tokens`, `temperature`, `context_budget`, `compaction_slack`,
`compaction_keep`, `loop_stall_limit`, `malformed_retry_budget`,
`tool_call_style`, `theme`. Fields the env never exposed (`session_dir`,
`llm_module`, `max_turns`, the extensions list) stay out of both; closing
that gap means adding to both seams, not letting the file diverge.

**A `FileConfig` DTO carries the schema** - every field `Option<T>`,
`#[serde(deny_unknown_fields)]`, with an `apply(&self, &mut
SessionConfig)` that overlays only present keys. Parsing splits into a
pure `FileConfig::parse(&str)` (unit-tested with JSON literals) and a
thin impure reader that resolves the path and reads the bytes. The
excluded fields are simply absent from the DTO, so `deny_unknown_fields`
rejects them for free.

**Failure is loud, mirroring the env philosophy.** Malformed JSON
syntax, an unknown/misspelled key, and an out-of-range value are each a
hard error at launch. The range check is free: `validate()` already
range-checks every one of these fields on the final `Session`, so a bad
file value that survives to launch is caught there.

**No auto-create.** An absent file is an empty overlay - `base()`
defaults, no file touched. Auto-writing would bake the current defaults
into a file that then pins them below every future default bump.

**A `--write-config[=PATH]` flag** removes the hand-authoring friction
without auto-create: it writes `base()` defaults (not the current
effective config), **full** (every schema key, self-documenting),
**omitting `token`** so no secret is ever persisted by the tool, refuses
if the target exists unless `--force`, prints the path, and exits before
a Session is built. This requires `Serialize` on `FileConfig`.

## Considered options

- **File above the environment** (`base → env → file`) - rejected: env
  is the natural per-invocation override (`SUSPENDERS_MODEL=… suspenders`
  for a one-off), and a persistent file that silently beats an explicit
  env var is the more surprising order.
- **Project-local `.suspenders/config.json`** - rejected: that folder is
  git-tracked and project-scoped; user config is per-user and
  secret-bearing. Reusing the name across two scopes would make
  `.suspenders/` ambiguous.
- **Bare `~/.suspenders/config.json`** - rejected for `$XDG_CONFIG_HOME`,
  which pairs with the `$XDG_DATA_HOME` Session Logs already use and
  keeps the double-meaning of `.suspenders` out of `$HOME`.
- **`#[derive(Deserialize)]` on `SessionConfig` directly** - rejected:
  its fields are not `Option`, its defaults come from `base()` not
  `Default`, and it carries fields we deliberately exclude. A DTO makes
  the excluded set explicit and parallels the env overlay.
- **Lenient unknown keys** (forward-compatible) - rejected for
  `deny_unknown_fields`: a silently-dropped typo (`max_token`) is the
  most frustrating config failure. Version skew is a non-issue for a
  single local binary.
- **Writer dumps the effective config** (base + active env) - rejected:
  non-deterministic ("why is `temperature` 0.9 in my file?") and it would
  risk writing an env-sourced `token` to disk. `base()` defaults are a
  clean, reproducible template.

## Consequences

- `from_env`/`try_from_env` become the internal env-overlay step;
  `load`/`try_load` is the public composition entry `Session::new` calls.
- The resolver now touches the filesystem, but only on the `Session::new`
  path. Tests use `test_defaults()` + `Session::build` and never hit the
  file seam, so the suite stays hermetic; new tests cover
  `FileConfig::parse`/`apply` with literals.
- `token` can now be persisted per-user (the point of the file), but the
  tool never writes it - the user adds the key by hand or keeps it in
  env.
- The file and env seams must be kept in lockstep: a new user-tunable
  knob is added to both, or to neither.

## Amendment (ADR-0033): `/model` may create and sparse-write the file

The **No auto-create** rule above is about *launch* - the resolver never
fabricates a file behind the user's back. The `/model` Slash Command
(ADR-0033) is the one sanctioned exception: an explicit model pick is a
deliberate act (the spirit of `--write-config`), so it will **create
`config.json` if absent** and persist the choice by a **sparse
read-modify-write** - parsing the existing file (or starting empty),
setting only the `model` key, and writing it back. The user's other keys
are preserved and `token` is still never written by the tool. Because the
file sits below the environment, a write while `SUSPENDERS_MODEL` is set is
accompanied by a warning that the env var will override it next launch.

## Amendment (ADR-0037): the schema opens for Providers

Providers (ADR-0037) change the schema without keeping compatibility:

- `model` becomes a scoped `provider/model-id` (e.g.
  `anthropic/claude-fable-5`, `lmstudio/qwen3.6-27b`).
- The flat `base_url` and `token` keys retire. A `providers` table declares
  custom hosts - `{ "base_url", "api", optional "context_window", optional
  "token" }` per entry - while built-in Providers need no entry at all: their
  credential comes from their own environment key (`ANTHROPIC_API_KEY`, …),
  not from this file. An entry's `context_window` beats the global
  `context_budget` figure for that Provider's models (ADR-0037).
- **"Schema is exactly the env-settable key set" narrows to the scalar
  knobs.** The `providers` table is file-only - structure the env cannot
  express - so the file/env lockstep rule now governs the scalar keys that
  remain in both seams, and `SUSPENDERS_MODEL` carries the scoped id.
- `context_budget` remains, reinterpreted (ADR-0037): the window for
  Models the Catalog does not know, and an optional global cap - no longer
  the budget itself, which derives from the captured Model per Run.

Loud failure, `deny_unknown_fields`, no-auto-create, and the `/model`
sparse-write exception (which still never writes any `token`) all stand.

## Amendment (ADR-0056): `mcp_servers`, a file-only map like `providers`

MCP servers (ADR-0056) add one more file-only map to the schema:

- **`mcp_servers`** - keyed by user-chosen name, each entry an external MCP tool
  server. A stdio entry carries `command` (+ optional `args`/`env`/`cwd`); an
  HTTP entry carries `http_url` (+ optional `headers`). Optional `timeout_ms`,
  `trust`, `include_tools`, `exclude_tools` per entry. Like `providers`, it is a
  file-only structure the env cannot express, so it lives outside the file/env
  lockstep; it overlays whole (replace), not merge.
- The Suspenders-native key is snake_case **`mcp_servers`**, diverging from
  qwen-code's camelCase `mcpServers` - a config port stays in Suspenders' idiom.
- Each entry is `deny_unknown_fields` like the rest of the schema, so a typo'd
  key is a loud parse error. A malformed *transport* (both `command` and
  `http_url`, or neither) is a loud launch failure at `Session::build`, distinct
  from the fail-open connect path (a server that resolves but will not connect is
  skipped at attach, never rejected here - ADR-0056).
- No credential is ever persisted by the tool: static `headers` (a bearer token
  the user writes by hand) are the only auth, OAuth is out of scope.
