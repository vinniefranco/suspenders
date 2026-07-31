# Managed auto-memory is a model-driven prompt section, not a save_memory tool

Suspenders ports qwen-code v0.16.0's managed auto-memory (P5): a persistent, file-based memory the model builds up across Sessions, so a later Session can see who the user is, how they want to collaborate, what to avoid or repeat, and the context behind the work. The store lives in `src/memory.rs` (a leaf over `std` + `session`), the resolver in `src/session.rs`, and the integration is one prompt-suffix append plus a shared path allowance.

## D2: model-driven only - no save_memory tool

qwen-code removed its `save_memory` tool (its `rememberCommand.ts` reports "that tool was removed"). We port that state faithfully: there is NO `save_memory` tool. The model writes and recalls memory through the ordinary `write_file` / `edit_file` / `read_file` tools reaching into the memory root. What we port is the PROMPT SECTION that teaches the model the two-step save protocol (write a `type:`-frontmatter topic file, then add a one-line pointer to `MEMORY.md`), the four memory types (user / feedback / project / reference), the what-not-to-save exclusions, the recall-verification discipline, and the memory-vs-plans/tasks guidance. The prompt text is copied VERBATIM from `packages/core/src/memory/prompt.ts` with two ASCII-legibility deviations: every em-dash (U+2014) becomes a hyphen, per the house hyphens-everywhere rule, AND the one U+2192 arrow in the `project` type's when_to_save (`"Thursday" -> "2026-03-05"`) becomes an ASCII `->`. A unit test asserts the built prompt carries no U+2014.

## Memory location: global, project-keyed, under the XDG data home

By default the memory root is GLOBAL and project-keyed: `<base>/projects/<slug(canonical_git_root)>/memory`, where `base` is the XDG data home (`~/.local/share/suspenders`, mirroring the Session Logs), the canonical git root is the `.git`-bearing ancestor of the Project Root, and the slug replaces every `[^a-zA-Z0-9]` with `-` (qwen `sanitizeCwd`). Global-by-default keeps memory out of the working tree (it is not the user's code) while still keying it to the project, so two checkouts of one repo share one memory. Two env overrides mirror qwen's:

- `SUSPENDERS_MEMORY_LOCAL=1` -> in-root `<project>/.suspenders/memory` (qwen `QWEN_CODE_MEMORY_LOCAL`).
- `SUSPENDERS_MEMORY_BASE_DIR` -> replaces the base dir (the test seam; qwen `QWEN_CODE_MEMORY_BASE_DIR`).

Deviation (git worktrees): qwen's `findCanonicalGitRoot` resolves a worktree's `.git` FILE (a `gitdir:` pointer) back to the main checkout so sibling worktrees of one repo share a memory. Suspenders takes the documented simplification of returning the worktree's own root (its `.git` is a file, `Path::exists` still finds it), so each worktree keys its OWN memory. This is a conservative, safe divergence: worktrees get separate memory rather than a mis-shared one, and the common non-worktree case is identical.

## The F5 seam: one suffix, loaded once, no mid-Session refresh

The memory prompt is appended in `init_agent` at the same composition point as the Deferred Tools section, joined with `\n\n---\n\n` (qwen `appendManagedAutoMemoryToUserMemory`). This is the F5 prompt-section composition seam becoming real (the ADR-0054 interim note): the Deferred Tools section and the memory suffix now both compose here.

The `MEMORY.md` index is loaded ONCE at Session start (`MemoryStore::load`) and never refreshed mid-Session. This is faithful to qwen: the model's own writes DURING a Session land in the files but do not re-enter the prompt until the NEXT Session loads them. No watcher, no re-read. The index is truncated before it enters the prompt (200 lines / 25 KB caps, with a WARNING footer when cut) so it can never blow the context.

Scaffolding is fail-open, exactly like the MCP attach and skill discovery (ADR-0007's report seam): `init_agent` mkdirs the memory dir; a failure is a visible launch notice, never fatal. We DROP qwen's meta.json / extract-cursor / consolidation-lock scaffolding - that belongs to the deferred pipeline below.

## The trust-path allowance: Project Root OR the resolved memory subtree

The shared path seam (`tool::path::resolve_absolute_in`) requires an ABSOLUTE model-supplied path and confines it to the Project Root OR the trusted memory subtree carried on the `ToolCtx`. The allowance is NARROW and SECURITY-CRITICAL: both boundaries use the same normalized-containment discipline (lexical `..`-collapse, then `== boundary` OR `starts_with(boundary <> SEP)`). The trailing separator is what refuses a `<memory_root>-evil` sibling that merely shares the string prefix (the `isAutoMemPath` shape), and the memory root is normalized too, so a `..` inside it cannot widen the allowance. The allowance lives once in the shared seam, so `write_file` / `edit_file` / `read_file` reach memory uniformly - no per-tool `isAutoMemPath` duplication.

## Auto-approval of memory writes

qwen flips its default permission to 'allow' for a memory write so autonomous memory does not prompt. In Suspenders, `write_file` / `edit_file` are UNGATED by design (ADR-0050 gates only code-execution and outbound-fetch), so a write into the memory dir - like any write - carries no gate at all, while `run_shell_command` still gates. The auto-approval of memory writes is therefore inherent in the ungated write path, not a per-path branch; a test pins that invariant.

## Deferred (OUT of this port)

The extract / dream / recall / forget pipeline is deferred: no background extraction cursor, no consolidation lock, no metadata, no `AutoMemoryMetadata` / `ExtractCursor` / `SourceRef` types, no topic-path helpers beyond the index. Memory is written and recalled entirely model-driven through the file tools. If a background consolidation pipeline is wanted later, it lands as its own phase against this store. The `MemoryType` enum (the four flat `user`/`feedback`/`project`/`reference` values, parsed from a file's `type:` frontmatter) also lands with that extract pipeline - nothing parses frontmatter today, so the model-driven prompt teaches the types as strings and no code enumerates them yet.
