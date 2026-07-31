//! Managed auto-memory (P5, ADR-0062): a persistent, file-based memory the
//! model builds up across Sessions, ported faithfully from qwen-code v0.16.0's
//! `packages/core/src/memory`. This is the D2 model-driven port: the prompt
//! section that teaches the model to write and recall memory files, plus the
//! store that reads the `MEMORY.md` index and the paths that place it. There is
//! NO `save_memory` tool - qwen removed it, so the model writes memory through
//! the ordinary `write_file`/`edit_file`/`read_file` tools reaching into the
//! memory root (the trust-path allowance in `tool::path`).
//!
//! ## The deviations from verbatim
//!
//! Two, both ASCII-legibility conversions: (1) qwen's prompt text carries
//! em-dashes (U+2014); the house rule is hyphens-everywhere, so every em-dash in
//! the ported prompt is a hyphen here. (2) The `project` type's when_to_save
//! carries a U+2192 arrow (`"Thursday" -> "2026-03-05"`); it became an ASCII
//! `->` (more model-legible). Everything else is copied exactly, including the
//! XML `<types>` blocks and the frontmatter example.
//!
//! ## Cadence (ADR-0062)
//!
//! The index is loaded ONCE at Session start (via [`MemoryStore::load_at`] from
//! the Session's already-resolved `memory_root`, read by `init_agent`) and never
//! refreshed mid-Session. This is faithful to qwen: the
//! model's own writes during a Session land in the files but do not re-enter the
//! prompt until the NEXT Session loads them. No mid-Session refresh, no watcher.
//!
//! ## Shape
//!
//! This module is a LEAF: it imports `std` and [`crate::session`] (for the
//! shared memory-root resolver) only. The prompt builder and truncation are
//! pure string work; the store is a thin fail-open reader.

use std::path::{Path, PathBuf};

/// The index filename inside the memory root (qwen `AUTO_MEMORY_INDEX_FILENAME`).
pub const MEMORY_INDEX_FILENAME: &str = "MEMORY.md";

// The truncation caps (qwen `MAX_MANAGED_AUTO_MEMORY_INDEX_LINES` /
// `_BYTES`): the index is always in-context, so it is bounded before it lands
// in the prompt.
const MAX_MANAGED_AUTO_MEMORY_INDEX_LINES: usize = 200;
const MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES: usize = 25_000;

// ---- Paths ----

/// The memory root for `project_root` (qwen `getAutoMemoryRoot`): delegates to
/// the shared Session resolver, which walks to the canonical git root, slugs it,
/// and places `memory/` under the XDG data base - honoring the
/// `SUSPENDERS_MEMORY_LOCAL` / `SUSPENDERS_MEMORY_BASE_DIR` overrides.
pub fn memory_root(project_root: &str) -> PathBuf {
    PathBuf::from(crate::session::default_memory_root(project_root))
}

/// The index path `<memory_root>/MEMORY.md` (qwen `getAutoMemoryIndexPath`).
pub fn index_path(memory_root: &Path) -> PathBuf {
    memory_root.join(MEMORY_INDEX_FILENAME)
}

// The memory-subtree containment check is NOT duplicated here: the single
// source of truth is `tool::path::contained` / `resolve_path_in` (the
// `== boundary` OR `starts_with(boundary <> SEP)` discipline), reached through
// the ctx's `memory_root` allowance. Two containment algorithms would invite
// drift, so this module owns only the paths, not the check.

// ---- Store ----

/// The loaded memory state at Session start (qwen `MemoryStore`): the memory
/// directory (interpolated into the prompt at every save-protocol site) and the
/// current `MEMORY.md` index body, or `None` when absent/empty (fail-open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStore {
    /// The absolute memory directory as a display string, interpolated into the
    /// prompt everywhere qwen interpolates `memoryDir`.
    pub memory_dir: String,
    /// The `MEMORY.md` index body, or `None` when the file is absent or empty.
    pub index: Option<String>,
}

impl MemoryStore {
    /// Loads the memory state for `project_root` (qwen `readAutoMemoryIndex`,
    /// fail-open): resolves the memory root and reads `MEMORY.md`. A missing or
    /// empty file yields `index: None` (the prompt shows the empty placeholder);
    /// any other read error is also swallowed to `None`, so a memory glitch
    /// never fails launch.
    ///
    /// This is the resolve-then-load convenience; production code loads from the
    /// Session's ALREADY-resolved `memory_root` via [`load_at`](Self::load_at),
    /// so there is one resolution of the memory root, not two that could diverge
    /// if the env shifted between them.
    pub fn load(project_root: &str) -> MemoryStore {
        MemoryStore::load_at(&memory_root(project_root))
    }

    /// Loads the memory state from an ALREADY-resolved memory root (P5,
    /// ADR-0062): reads `MEMORY.md`, fail-open. This is the production entry -
    /// `init_agent` passes `session.memory_root`, the SAME resolution the
    /// ToolCtx confinement uses, so the dir the model is told to write to is
    /// exactly the dir confinement permits.
    pub fn load_at(memory_root: &Path) -> MemoryStore {
        let memory_dir = memory_root.to_string_lossy().into_owned();
        let index = read_index(memory_root);
        MemoryStore { memory_dir, index }
    }

    /// Ensures the memory directory exists (qwen `ensureAutoMemoryScaffold`,
    /// narrowed): mkdir the memory dir ONLY - the meta.json / extract-cursor
    /// scaffolding is the deferred pipeline (ADR-0062) and is dropped. Returns
    /// the IO error so `init_agent` can surface a fail-open launch notice.
    pub fn ensure_scaffold(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.memory_dir)
    }

    /// The prompt suffix appended after the Deferred Tools section (F5 seam,
    /// ADR-0054/0062): the `\n\n---\n\n` join plus the built managed-auto-memory
    /// prompt. Always non-empty here (the memory system is always on), so the
    /// suffix always leads with the separator - qwen's
    /// `appendManagedAutoMemoryToUserMemory` join for a non-empty prior prompt.
    pub fn prompt_suffix(&self) -> String {
        let managed = build_managed_auto_memory_prompt(&self.memory_dir, self.index.as_deref());
        format!("\n\n---\n\n{managed}")
    }
}

// The fail-open index reader (qwen `readAutoMemoryIndex`): an absent/empty file,
// or any read error, is `None`. A whitespace-only file is treated as empty too,
// so the prompt shows the "currently empty" placeholder rather than a blank
// index block.
fn read_index(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(index_path(root)).ok()?;
    if raw.trim().is_empty() {
        None
    } else {
        Some(raw)
    }
}

// ---- The VERBATIM prompt (qwen prompt.ts; em-dashes -> hyphens, U+2192 -> ->) ----

const DIR_EXISTS_GUIDANCE: &str = "This directory already exists - write to it directly with the write_file tool (do not run mkdir or check for its existence).";

/// The memory-file frontmatter example (qwen `MEMORY_FRONTMATTER_EXAMPLE`).
pub const MEMORY_FRONTMATTER_EXAMPLE: &[&str] = &[
    "```markdown",
    "---",
    "name: {{memory name}}",
    "description: {{one-line description - used to decide relevance in future conversations, so be specific}}",
    "type: {{user, feedback, project, reference}}",
    "---",
    "",
    "{{memory content - for feedback/project types, structure as: rule/fact, then **Why:** and **How to apply:** lines}}",
    "```",
];

/// The four type blocks (qwen `TYPES_SECTION_INDIVIDUAL`).
pub const TYPES_SECTION_INDIVIDUAL: &[&str] = &[
    "## Types of memory",
    "",
    "There are several discrete types of memory that you can store in your memory system:",
    "",
    "<types>",
    "<type>",
    "    <name>user</name>",
    "    <description>Contain information about the user's role, goals, responsibilities, and knowledge. Great user memories help you tailor your future behavior to the user's preferences and perspective. Your goal in reading and writing these memories is to build up an understanding of who the user is and how you can be most helpful to them specifically. For example, you should collaborate with a senior software engineer differently than a student who is coding for the very first time. Keep in mind, that the aim here is to be helpful to the user. Avoid writing memories about the user that could be viewed as a negative judgement or that are not relevant to the work you're trying to accomplish together.</description>",
    "    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge</when_to_save>",
    "    <how_to_use>When your work should be informed by the user's profile or perspective. For example, if the user is asking you to explain a part of the code, you should answer that question in a way that is tailored to the specific details that they will find most valuable or that helps them build their mental model in relation to domain knowledge they already have.</how_to_use>",
    "    <examples>",
    "    user: I'm a data scientist investigating what logging we have in place",
    "    assistant: [saves user memory: user is a data scientist, currently focused on observability/logging]",
    "",
    "    user: I've been writing Go for ten years but this is my first time touching the React side of this repo",
    "    assistant: [saves user memory: deep Go expertise, new to React and this project's frontend - frame frontend explanations in terms of backend analogues]",
    "    </examples>",
    "</type>",
    "<type>",
    "    <name>feedback</name>",
    "    <description>Guidance the user has given you about how to approach work - both what to avoid and what to keep doing. These are a very important type of memory to read and write as they allow you to remain coherent and responsive to the way you should approach work in the project. Record from failure AND success: if you only save corrections, you will avoid past mistakes but drift away from approaches the user has already validated, and may grow overly cautious.</description>",
    "    <when_to_save>Any time the user corrects your approach (\"no not that\", \"don't\", \"stop doing X\") OR confirms a non-obvious approach worked (\"yes exactly\", \"perfect, keep doing that\", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter - watch for them. In both cases, save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later.</when_to_save>",
    "    <how_to_use>Let these memories guide your behavior so that the user does not need to offer the same guidance twice.</how_to_use>",
    "    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave - often a past incident or strong preference) and a **How to apply:** line (when/where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>",
    "    <examples>",
    "    user: don't mock the database in these tests - we got burned last quarter when mocked tests passed but the prod migration failed",
    "    assistant: [saves feedback memory: integration tests must hit a real database, not mocks. Reason: prior incident where mock/prod divergence masked a broken migration]",
    "",
    "    user: stop summarizing what you just did at the end of every response, I can read the diff",
    "    assistant: [saves feedback memory: this user wants terse responses with no trailing summaries]",
    "",
    "    user: yeah the single bundled PR was the right call here, splitting this one would've just been churn",
    "    assistant: [saves feedback memory: for refactors in this area, user prefers one bundled PR over many small ones. Confirmed after I chose this approach - a validated judgment call, not a correction]",
    "    </examples>",
    "</type>",
    "<type>",
    "    <name>project</name>",
    "    <description>Information that you learn about ongoing work, goals, initiatives, bugs, or incidents within the project that is not otherwise derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work the user is doing within this working directory.</description>",
    "    <when_to_save>When you learn who is doing what, why, or by when. These states change relatively quickly so try to keep your understanding of this up to date. Always convert relative dates in user messages to absolute dates when saving (e.g., \"Thursday\" -> \"2026-03-05\"), so the memory remains interpretable after time passes.</when_to_save>",
    "    <how_to_use>Use these memories to more fully understand the details and nuance behind the user's request and make better informed suggestions.</how_to_use>",
    "    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation - often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>",
    "    <examples>",
    "    user: we're freezing all non-critical merges after Thursday - mobile team is cutting a release branch",
    "    assistant: [saves project memory: merge freeze begins 2026-03-05 for mobile release cut. Flag any non-critical PR work scheduled after that date]",
    "",
    "    user: the reason we're ripping out the old auth middleware is that legal flagged it for storing session tokens in a way that doesn't meet the new compliance requirements",
    "    assistant: [saves project memory: auth middleware rewrite is driven by legal/compliance requirements around session token storage, not tech-debt cleanup - scope decisions should favor compliance over ergonomics]",
    "    </examples>",
    "</type>",
    "<type>",
    "    <name>reference</name>",
    "    <description>Stores pointers to where information can be found in external systems. These memories allow you to remember where to look to find up-to-date information outside of the project directory.</description>",
    "    <when_to_save>When you learn about resources in external systems and their purpose. For example, that bugs are tracked in a specific project in Linear or that feedback can be found in a specific Slack channel.</when_to_save>",
    "    <how_to_use>When the user references an external system or information that may be in an external system.</how_to_use>",
    "    <examples>",
    "    user: check the Linear project \"INGEST\" if you want context on these tickets, that's where we track all pipeline bugs",
    "    assistant: [saves reference memory: pipeline bugs are tracked in Linear project \"INGEST\"]",
    "",
    "    user: the Grafana board at grafana.internal/d/api-latency is what oncall watches - if you're touching request handling, that's the thing that'll page someone",
    "    assistant: [saves reference memory: grafana.internal/d/api-latency is the oncall latency dashboard - check it when editing request-path code]",
    "    </examples>",
    "</type>",
    "</types>",
    "",
];

/// The exclusions block (qwen `WHAT_NOT_TO_SAVE_SECTION`).
pub const WHAT_NOT_TO_SAVE_SECTION: &[&str] = &[
    "## What NOT to save in memory",
    "",
    "- Code patterns, conventions, architecture, file paths, or project structure - these can be derived by reading the current project state.",
    "- Git history, recent changes, or who-changed-what - `git log` / `git blame` are authoritative.",
    "- Debugging solutions or fix recipes - the fix is in the code; the commit message has the context.",
    "- Anything already documented in QWEN.md or AGENTS.md files.",
    "- Ephemeral task details: in-progress work, temporary state, current conversation context.",
    "",
    "These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it - that is the part worth keeping.",
];

/// The memory-drift caveat line (qwen `MEMORY_DRIFT_CAVEAT`).
pub const MEMORY_DRIFT_CAVEAT: &str = "- Memory records can become stale over time. Use memory as context for what was true at a given point in time. Before answering the user or building assumptions based solely on information in memory records, verify that the memory is still correct and up-to-date by reading the current state of the files or resources. If a recalled memory conflicts with current information, trust what you observe now - and update or remove the stale memory rather than acting on it.";

/// The when-to-access block (qwen `WHEN_TO_ACCESS_SECTION`).
pub const WHEN_TO_ACCESS_SECTION: &[&str] = &[
    "## When to access memories",
    "- When memories seem relevant, or the user references prior-conversation work.",
    "- You MUST access memory when the user explicitly asks you to check, recall, or remember.",
    "- If the user says to *ignore* or *not use* memory: proceed as if MEMORY.md were empty. Do not apply remembered facts, cite, compare against, or mention memory content.",
    MEMORY_DRIFT_CAVEAT,
];

/// The recall-verification block (qwen `TRUSTING_RECALL_SECTION`).
pub const TRUSTING_RECALL_SECTION: &[&str] = &[
    "## Before recommending from memory",
    "",
    "A memory that names a specific function, file, or flag is a claim that it existed when the memory was written. It may have been renamed, removed, or never merged. Before recommending it:",
    "",
    "- If the memory names a file path: check the file exists.",
    "- If the memory names a function or flag: grep for it.",
    "- If the user is about to act on your recommendation (not just asking about history), verify first.",
    "",
    "\"The memory says X exists\" is not the same as \"X exists now.\"",
    "",
    "A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer `git log` or reading the code over recalling the snapshot.",
];

/// Truncates the `MEMORY.md` index to the line/byte caps (qwen
/// `truncateManagedAutoMemoryIndex`): under both caps the trimmed content passes
/// through unchanged; over either, the content is cut and a WARNING footer names
/// the reason.
fn truncate_managed_auto_memory_index(index_content: &str) -> String {
    let trimmed = index_content.trim();
    let lines: Vec<&str> = trimmed.split('\n').collect();
    let line_count = lines.len();
    let byte_count = trimmed.len();
    let was_line_truncated = line_count > MAX_MANAGED_AUTO_MEMORY_INDEX_LINES;
    let was_byte_truncated = byte_count > MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return trimmed.to_string();
    }

    let mut truncated = if was_line_truncated {
        lines[..MAX_MANAGED_AUTO_MEMORY_INDEX_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES {
        // Cut at the last newline at or before the byte cap (qwen's
        // `lastIndexOf('\n', BYTES)`), falling back to the raw cap when there
        // is no earlier newline. The search window is a valid char boundary
        // because the index is line-oriented ASCII-ish text; guard anyway.
        let window = &truncated[..cap_boundary(&truncated, MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES)];
        let cut_at = window.rfind('\n');
        truncated = match cut_at {
            Some(i) if i > 0 => truncated[..i].to_string(),
            _ => truncated[..cap_boundary(&truncated, MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES)]
                .to_string(),
        };
    }

    let reason = if was_byte_truncated && !was_line_truncated {
        format!(
            "{:.1} KB (limit: {:.1} KB) - index entries are too long",
            byte_count as f64 / 1024.0,
            MAX_MANAGED_AUTO_MEMORY_INDEX_BYTES as f64 / 1024.0
        )
    } else if was_line_truncated && !was_byte_truncated {
        format!("{line_count} lines (limit: {MAX_MANAGED_AUTO_MEMORY_INDEX_LINES})")
    } else {
        format!(
            "{line_count} lines and {:.1} KB",
            byte_count as f64 / 1024.0
        )
    };

    format!(
        "{truncated}\n\n> WARNING: MEMORY.md is {reason}. Only part of it was loaded. Keep index entries to one line under ~200 chars; move detail into topic files."
    )
}

// The largest char boundary at or below `cap` in `s`, so a byte-cap slice never
// splits a multibyte char (Rust would panic where JS `slice` would not).
fn cap_boundary(s: &str, cap: usize) -> usize {
    let mut b = cap.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Builds the managed-auto-memory prompt (qwen `buildManagedAutoMemoryPrompt`):
/// the whole `# auto memory` section with `memory_dir` interpolated at every
/// save-protocol site, the type blocks, the exclusions, the two-step save
/// protocol, the recall guidance, the persistence-vs-plans/tasks guidance, and
/// finally the current `MEMORY.md` index (truncated) or the empty placeholder.
pub fn build_managed_auto_memory_prompt(memory_dir: &str, index_content: Option<&str>) -> String {
    let trimmed = index_content.map(str::trim).filter(|s| !s.is_empty());

    let mut lines: Vec<String> = Vec::new();
    let mut push = |s: String| lines.push(s);

    push("# auto memory".into());
    push(String::new());
    push(format!(
        "You have a persistent, file-based memory system at `{memory_dir}`. {DIR_EXISTS_GUIDANCE}"
    ));
    push(String::new());
    push("You should build up this memory system over time so that future conversations can have a complete picture of who the user is, how they'd like to collaborate with you, what behaviors to avoid or repeat, and the context behind the work the user gives you.".into());
    push(String::new());
    push("If the user explicitly asks you to remember something, save it immediately as whichever type fits best. If they ask you to forget something, find and remove the relevant entry.".into());
    push(String::new());
    for s in TYPES_SECTION_INDIVIDUAL {
        push((*s).to_string());
    }
    for s in WHAT_NOT_TO_SAVE_SECTION {
        push((*s).to_string());
    }
    push(String::new());
    push("## How to save memories".into());
    push(String::new());
    push("Saving a memory is a two-step process:".into());
    push(String::new());
    push("**Step 1** - write the memory to its own file (e.g., `user_role.md`, `feedback_testing.md`) using this frontmatter format:".into());
    push(String::new());
    for s in MEMORY_FRONTMATTER_EXAMPLE {
        push((*s).to_string());
    }
    push(String::new());
    push(format!(
        "**Step 2** - add a pointer to that file in `{memory_dir}/MEMORY.md` (the full absolute path). This index file is an index, not a memory - each entry should be one line, under ~150 characters: `- [Title](file.md) - one-line hook`. It has no frontmatter. Never write memory content directly into `{memory_dir}/MEMORY.md`."
    ));
    push(String::new());
    push(format!(
        "- `{memory_dir}/MEMORY.md` is always loaded into your conversation context - lines after {MAX_MANAGED_AUTO_MEMORY_INDEX_LINES} will be truncated, so keep the index concise"
    ));
    push(
        "- Keep the name, description, and type fields in memory files up-to-date with the content"
            .into(),
    );
    push("- Organize memory semantically by topic, not chronologically.".into());
    push("- Update or remove memories that turn out to be wrong or outdated.".into());
    push("- Do not write duplicate memories. First check if there is an existing memory you can update before writing a new one.".into());
    push(String::new());
    for s in WHEN_TO_ACCESS_SECTION {
        push((*s).to_string());
    }
    push(String::new());
    for s in TRUSTING_RECALL_SECTION {
        push((*s).to_string());
    }
    push(String::new());
    push("## Memory and other forms of persistence".into());
    push("Memory is one of several persistence mechanisms available to you as you assist the user in a given conversation. The distinction is often that memory can be recalled in future conversations and should not be used for persisting information that is only useful within the scope of the current conversation.".into());
    push("- When to use or update a plan instead of memory: If you are about to start a non-trivial implementation task and would like to reach alignment with the user on your approach you should use a Plan rather than saving this information to memory. Similarly, if you already have a plan within the conversation and you have changed your approach persist that change by updating the plan rather than saving a memory.".into());
    push("- When to use or update tasks instead of memory: When you need to break your work in current conversation into discrete steps or keep track of your progress use tasks instead of saving to memory. Tasks are great for persisting information about the work that needs to be done in the current conversation, but memory should be reserved for information that will be useful in future conversations.".into());
    push(String::new());
    push(format!("## {memory_dir}/MEMORY.md"));
    push(String::new());
    push(match trimmed {
        Some(body) => truncate_managed_auto_memory_index(body),
        None => {
            "Your MEMORY.md is currently empty. When you save new memories, they will appear here."
                .into()
        }
    });

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The memory-subtree containment check lives in `tool::path` (its
    // `resolve_path_in` tests), not here - this module owns only the paths.

    // ---- prompt builder: VERBATIM content + no em-dashes ----

    // The em-dash the house rule forbids (U+2014). The ported prompt must carry
    // none of them.
    const EM_DASH: char = '\u{2014}';

    fn empty_prompt() -> String {
        build_managed_auto_memory_prompt("/mem", None)
    }

    #[test]
    fn the_prompt_carries_no_em_dashes() {
        // A deviation from verbatim: every U+2014 became a hyphen (the other is
        // the U+2192 arrow that became `->`).
        let full = build_managed_auto_memory_prompt("/mem", Some("- [X](x.md) - hook"));
        assert!(
            !full.contains(EM_DASH),
            "ported prompt must not contain an em-dash (U+2014)"
        );
    }

    #[test]
    fn the_prompt_contains_each_type_block() {
        let p = empty_prompt();
        assert!(p.contains("<name>user</name>"));
        assert!(p.contains("<name>feedback</name>"));
        assert!(p.contains("<name>project</name>"));
        assert!(p.contains("<name>reference</name>"));
    }

    #[test]
    fn the_prompt_contains_the_frontmatter_example() {
        let p = empty_prompt();
        assert!(p.contains("type: {{user, feedback, project, reference}}"));
        assert!(p.contains("name: {{memory name}}"));
    }

    #[test]
    fn the_prompt_interpolates_the_memory_dir_at_every_save_protocol_site() {
        let p = build_managed_auto_memory_prompt("/some/mem/dir", None);
        // The header line.
        assert!(p.contains("persistent, file-based memory system at `/some/mem/dir`"));
        // Step 2's index pointer + the "never write directly" clause.
        assert!(p.contains("add a pointer to that file in `/some/mem/dir/MEMORY.md`"));
        assert!(p.contains("Never write memory content directly into `/some/mem/dir/MEMORY.md`"));
        // The always-loaded truncation note.
        assert!(p.contains("`/some/mem/dir/MEMORY.md` is always loaded"));
        // The index heading.
        assert!(p.contains("## /some/mem/dir/MEMORY.md"));
    }

    #[test]
    fn the_prompt_contains_the_two_step_save_protocol() {
        let p = empty_prompt();
        assert!(p.contains("Saving a memory is a two-step process:"));
        assert!(p.contains("**Step 1** - write the memory to its own file"));
        assert!(p.contains("**Step 2** - add a pointer to that file"));
    }

    #[test]
    fn the_prompt_contains_what_not_to_save() {
        let p = empty_prompt();
        assert!(p.contains("## What NOT to save in memory"));
        assert!(p.contains("Git history, recent changes, or who-changed-what"));
    }

    #[test]
    fn the_prompt_contains_recall_verification() {
        let p = empty_prompt();
        assert!(p.contains("## Before recommending from memory"));
        assert!(p.contains("is not the same as"));
    }

    #[test]
    fn the_prompt_contains_the_persistence_vs_plans_guidance() {
        let p = empty_prompt();
        assert!(p.contains("## Memory and other forms of persistence"));
        assert!(p.contains("use a Plan rather than saving this information to memory"));
        assert!(p.contains("use tasks instead of saving to memory"));
    }

    #[test]
    fn the_empty_index_shows_the_placeholder() {
        let p = empty_prompt();
        assert!(p.contains(
            "Your MEMORY.md is currently empty. When you save new memories, they will appear here."
        ));
    }

    #[test]
    fn a_present_index_body_lands_in_the_prompt() {
        let p = build_managed_auto_memory_prompt(
            "/mem",
            Some("- [Testing](feedback_testing.md) - use a real db"),
        );
        assert!(p.contains("- [Testing](feedback_testing.md) - use a real db"));
        assert!(!p.contains("currently empty"));
    }

    // ---- MemoryStore::load (fail-open) ----
    //
    // These tests redirect the store to a temp base via SUSPENDERS_MEMORY_BASE_DIR
    // and clear it afterward. Safe under nextest (process-per-test) and
    // `--test-threads=1` (serial): each test owns the var for its body and spawns
    // no threads.

    fn with_temp_memory_base<T>(f: impl FnOnce(&str) -> T) -> T {
        let base = std::env::temp_dir().join(format!(
            "suspenders_mem_store_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: process-per-test / serial single-threaded.
        unsafe {
            std::env::remove_var("SUSPENDERS_MEMORY_LOCAL");
            std::env::set_var("SUSPENDERS_MEMORY_BASE_DIR", &base);
        }
        // A project root with no `.git` ancestor keeps the resolver deterministic
        // (it slugs the root itself under the temp base).
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let out = f(&project.to_string_lossy());
        // SAFETY: process-per-test / serial single-threaded.
        unsafe { std::env::remove_var("SUSPENDERS_MEMORY_BASE_DIR") };
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    #[test]
    fn load_absent_index_is_none_and_the_suffix_shows_the_placeholder() {
        with_temp_memory_base(|project| {
            let store = MemoryStore::load(project);
            assert_eq!(store.index, None);
            let suffix = store.prompt_suffix();
            // The suffix leads with the `\n\n---\n\n` join.
            assert!(suffix.starts_with("\n\n---\n\n"));
            assert!(suffix.contains("Your MEMORY.md is currently empty."));
        });
    }

    #[test]
    fn load_with_a_memory_md_puts_the_index_body_in_the_suffix() {
        with_temp_memory_base(|project| {
            let store = MemoryStore::load(project);
            // Scaffold and write an index the way the model would.
            store.ensure_scaffold().unwrap();
            std::fs::write(
                index_path(&memory_root(project)),
                "- [Testing](feedback_testing.md) - use a real db",
            )
            .unwrap();

            let reloaded = MemoryStore::load(project);
            assert_eq!(
                reloaded.index.as_deref(),
                Some("- [Testing](feedback_testing.md) - use a real db")
            );
            let suffix = reloaded.prompt_suffix();
            assert!(suffix.contains("- [Testing](feedback_testing.md) - use a real db"));
            assert!(!suffix.contains("currently empty"));
        });
    }

    #[test]
    fn an_empty_memory_md_is_treated_as_absent_fail_open() {
        with_temp_memory_base(|project| {
            let store = MemoryStore::load(project);
            store.ensure_scaffold().unwrap();
            // A whitespace-only file is fail-open empty.
            std::fs::write(index_path(&memory_root(project)), "   \n\n").unwrap();

            let reloaded = MemoryStore::load(project);
            assert_eq!(reloaded.index, None);
            assert!(reloaded.prompt_suffix().contains("currently empty"));
        });
    }

    #[test]
    fn ensure_scaffold_mkdirs_the_memory_dir_only() {
        with_temp_memory_base(|project| {
            let store = MemoryStore::load(project);
            store.ensure_scaffold().unwrap();
            assert!(std::path::Path::new(&store.memory_dir).is_dir());
            // No meta.json / extract-cursor (the deferred pipeline is dropped).
            assert!(
                !std::path::Path::new(&store.memory_dir)
                    .join("meta.json")
                    .exists()
            );
        });
    }

    // ---- truncate ----

    #[test]
    fn under_cap_index_passes_through_untouched() {
        let body = "- [A](a.md) - one\n- [B](b.md) - two";
        assert_eq!(truncate_managed_auto_memory_index(body), body);
    }

    #[test]
    fn over_the_line_cap_emits_the_warning_footer() {
        let body: String = (0..250)
            .map(|i| format!("- [E{i}](e{i}.md) - hook"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_managed_auto_memory_index(&body);
        assert!(out.contains("> WARNING: MEMORY.md is"));
        assert!(out.contains("250 lines (limit: 200)"));
        // Only the first 200 lines survive.
        assert!(out.contains("- [E0](e0.md)"));
        assert!(!out.contains("- [E249](e249.md)"));
    }

    #[test]
    fn over_the_byte_cap_emits_the_warning_footer_naming_kb() {
        // A handful of very long lines: under the 200-line cap but over 25 KB.
        let long = "x".repeat(5_000);
        let body: String = (0..10)
            .map(|i| format!("- [L{i}]({long})"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_managed_auto_memory_index(&body);
        assert!(out.contains("> WARNING: MEMORY.md is"));
        assert!(out.contains("KB (limit: 24.4 KB) - index entries are too long"));
    }
}
