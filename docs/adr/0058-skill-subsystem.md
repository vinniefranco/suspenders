# The skill subsystem: disk-discovered SKILL.md skills, one always-loaded tool

Suspenders is a coding agent for small local models; its built-in tool set is small on purpose (ADR-0056). A *skill* is a way to teach the model a specialized capability without adding a tool: a directory containing a `SKILL.md` manifest whose markdown body is instruction text the model reads on demand. This ADR is the skill subsystem (P2c): how a directory of skill files becomes a catalog the model can browse and invoke, and what stays out. It is a faithful port of qwen v0.16.0's skill loader (`skills/skill-load.ts`, `skills/types.ts`, `tools/skill-utils.ts`, `tools/skill.ts`), adapted to Suspenders' conventions.

## Skills live on disk under `.suspenders/skills/`

A skill is a DIRECTORY, `<name>/`, containing a `SKILL.md` file. Skills are discovered from two roots: `<project-root>/.suspenders/skills/` (project) and `~/.suspenders/skills/` (the user's XDG config home, beside `config.json` and `themes/`). We use `.suspenders/` rather than qwen's `.qwen/`, matching every other Suspenders convention. A directory without a `SKILL.md`, and a plain file, are silently skipped (qwen's `loadSkillsFromDir`): each skill must be a directory with a manifest.

A `SKILL.md` is YAML frontmatter between `---` fences followed by a markdown body:

```
---
name: pdf
description: Work with PDF files
when_to_use: When the user asks to read or fill a PDF
---
Instructions the model reads when it invokes this skill...
```

`name` and `description` are REQUIRED; a manifest missing either (or with an empty value) is a parse error and the skill is skipped. `when_to_use` is optional and, when present, is appended to the description in the model-facing catalog. `name` must match qwen's `SKILL_NAME_PATTERN` charset (Unicode letters/digits plus `_ : . -`), so a name with a structurally-unsafe character (`<`, `>`, `/`, whitespace) is rejected at parse time - the name flows verbatim into the model-facing catalog, so rejecting the injection vector at the source is more reliable than escaping at every sink. All other qwen frontmatter keys (`allowedTools`, `hooks`, `model`, `priority`, `disable-model-invocation`, `paths`, `argument-hint`) are PARSED-AND-IGNORED, so a real qwen `SKILL.md` still loads off its `name` + `description`.

## The frontmatter parser is hand-rolled, no YAML dep

We do not add a YAML crate. The landed fields (`name`, `description`, `when_to_use`) are flat `key: value` scalars, so `parse_skill_content` is a manual line scan mirroring qwen's regex `^---\n([\s\S]*?)\n---(?:\n|$)([\s\S]*)$`: normalize BOM + CRLF, split the leading `---` fence from the next `---`-only line (a `---` mid-value or mid-line does not close the block), then parse the frontmatter as flat `key: value` pairs and trim the body. A `key:` with no inline value and `- item` continuation lines (the list/nested shapes of the ignored fields) parse to nothing rather than crashing, and a single pair of surrounding quotes on a scalar is stripped. The split on the FIRST colon lets a value contain a colon (`Ratio 16:9`). This keeps the dependency surface flat; if a future field needs real YAML, that is a distinct decision.

## One always-loaded `skill` tool, the catalog IS the surfacing

There is ONE `skill` tool, not one tool per skill. It holds an `Arc<SkillManager>` (a stateful tool, like `McpTool`) and builds its description dynamically: the qwen scaffold (`Execute a skill within the main conversation` + `<skills_instructions>`) with an `<available_skills>` block spliced in, one `<skill><name/><description/><location/></skill>` entry per discovered skill (name + description XML-escaped via qwen's `escapeXml`, `when_to_use` appended to the description, `<location>` the skill's base directory). An empty catalog shows the "no skills configured" text, reworded to point at `.suspenders/skills/`.

That catalog IS the surfacing mechanism, so the tool is ALWAYS on the wire list the model sees (`always_load() == true`, never deferred). This is deliberately NOT the F3 deferred-tools path (ADR-0054): a deferred tool is hidden until `tool_search` reveals it, but the whole point of the skill tool is that the model reads the skill list off the tool's own description, so hiding the tool would hide the catalog. The `<available_skills>` block is analogous to how the Deferred Tools system-prompt section surfaces deferred tools - a name-and-description listing the model browses - except it rides the tool description rather than the system prompt. The tool is always registered (in `init_agent`), even with an empty catalog, so the model knows skills CAN exist.

Invoking the tool with `{skill: "<name>"}` returns that skill's body wrapped by `build_skill_llm_content` (VERBATIM from qwen: `Base directory for this skill: {baseDir}\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\n{body}\n`), so the model gets the instructions plus the base directory for resolving any script/reference paths. The validate-empty message (`Parameter "skill" must be a non-empty string.`) and the not-found message (`Skill "{name}" not found. Available skills: {names}`) are qwen-verbatim.

## Fail-open discovery, mirroring MCP

`SkillManager::discover(project_root, user_root)` is a LEAF (it imports only `std` + `serde_json`, never agent/run/ui/session), the fail-open front the way `McpManager::connect` is for MCP. It walks each root's immediate subdirectories (sorted, so discovery order is stable), parses each `SKILL.md`, and records a `(skill <name>, reason)` failure for any manifest that fails to parse or validate - the skill is skipped, discovery carries on. An unreadable/absent root (the common no-`.suspenders/skills/` case) is a silent no-op. Project skills are walked before user skills, so on a name collision the project skill wins (qwen's project-over-user precedence); the shadowed user skill is dropped silently, not recorded as a failure.

The Agent runs discovery once in `init_agent` (beside the MCP attach) and surfaces each `failures()` entry as one `Event::extension_error("skill <name>", PreRun, reason)` launch notice - the same fail-open report line an MCP connect failure and an Extension crash take (ADR-0007). A broken `SKILL.md` is a visible skip, never a fatal.

## Deferred (OUT of this ADR)

The following qwen skill features are parsed-and-ignored or omitted, deliberately deferred:

- **`paths:` conditional activation** - qwen gates a skill out of the catalog until a tool call touches a matching file path. Suspenders loads every valid skill unconditionally; `paths:` is parsed-and-ignored. (This also drops qwen's per-tool `SkillTool` change-listener / `refreshSkills` / `setTools` round-trip and the pending-conditional-skill validation branch.)
- **`hooks:`** - session-scoped hooks a skill registers on invocation. Out; Suspenders has no hook system yet.
- **`model` override** - a skill running on a different model than the session. Out; a Run runs on the Active Model (ADR-0033).
- **`priority`** - catalog ordering hint. Out; skills list in discovery order (project-then-user, name-sorted within a root).
- **`disable-model-invocation`** - hides a skill from the model, user-triggerable only. Out; there is no `/skill-name` slash-command surface yet.
- **Extension / bundled skill levels** - qwen loads skills from installed extensions and ships bundled skills. Out; Suspenders loads project + user only.
- **Model-invocable commands (MCP prompts / file commands) merged into the catalog** - qwen unifies these into `<available_skills>`. Out; the catalog is disk skills only.
