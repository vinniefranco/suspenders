//! Slash Command registry and draft parsing - the PURE recognition core behind
//! the Composer's `/`-menu (ADR-0032, CONTEXT.md: **Slash Command**). No
//! ratatui/crossterm and no knowledge of what any command DOES: the pure
//! [`crate::ui::composer::Composer`] looks a command up here and emits an
//! [`Effect::Command`](crate::ui::screen::Effect::Command); the actual work
//! runs adapter-side in that Effect's arm.
//!
//! ## The registry is the extension seam
//!
//! [`COMMANDS`] is a `&'static` slice of descriptors. Adding a command later is
//! one more entry, not a new widget: the fuzzy `/` palette
//! ([`crate::ui::completion`], ADR-0051 System B) ranks the registry, and a
//! selector-opening command's numbered `›` dialog ([`crate::ui::selection`],
//! System A) reuses the shared list.
//!
//! ## Parsing a slash draft
//!
//! A slash draft is `/name[ rest]`: the FIRST space separates the command token
//! (`name`) from an optional remainder (`rest`). [`parse`] splits it; [`lookup`]
//! resolves a name to a descriptor. The palette ranking lives in
//! [`crate::ui::completion::rank`], reading this registry (name + `alt_names`).

/// One command descriptor: the name typed after `/` and the one-line help the
/// menu shows. The Effect the command produces is NOT here - the pure core
/// emits a command-agnostic [`Effect::Command`](crate::ui::screen::Effect::Command)
/// and the adapter decides what it does.
///
/// `opens_selector` is the one bit of shape the pure core reads: a command like
/// `/model` opens a second filterable list (its own values) after it is
/// committed, so committing it does NOT clear the draft - it switches the same
/// inline popup to the command's selector sub-state (ADR-0033). A command
/// without a selector (`false`) is fire-and-run: committing it emits the
/// [`Effect::Command`](crate::ui::screen::Effect::Command) and clears the
/// draft. The core still never learns what the command DOES; it only learns
/// whether a selector sub-state follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub help: &'static str,
    pub opens_selector: bool,
    /// The selector popup's title - the plural noun for the command's own
    /// values ("models", "themes"). Stated here because pluralization is
    /// English grammar: the painter looks it up instead of conjugating the
    /// command name.
    pub list_title: &'static str,
    /// Alternate names the fuzzy `/` palette also matches against (qwen
    /// `SlashCommand.altNames`): the name and each alt name are ranked, and
    /// the best of them names the command in the suggestion list. Empty for a
    /// command with no aliases.
    pub alt_names: &'static [&'static str],
    /// A static ranking nudge (qwen `SlashCommand.completionPriority`): a
    /// higher value floats the command up the palette within a strength tier,
    /// ABOVE recency (qwen `compareRankedCommandMatches`, useSlashCompletion.ts
    /// 240-248). `0` for a command with no priority (the default).
    pub completion_priority: i32,
}

/// The available Slash Commands (ADR-0032's `&'static` registry); `/compact`,
/// … are each one more entry.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        help: "choose the model for this session",
        opens_selector: true,
        list_title: "models",
        alt_names: &[],
        completion_priority: 0,
    },
    SlashCommand {
        name: "theme",
        help: "choose the theme for this session",
        opens_selector: true,
        list_title: "themes",
        alt_names: &[],
        completion_priority: 0,
    },
    SlashCommand {
        name: "mcp",
        help: "manage MCP servers",
        // NOT a flat selector (`opens_selector: false`): `/mcp` opens the
        // navigation-stack McpDialog overlay (ADR-0065 Phase E), a distinct
        // System-A overlay the Composer holds apart from the flat CommandSelector.
        // Committing it is fire-and-run (it emits the effect and clears the
        // slash draft); the overlay it opens is the McpDialog, not a filterable
        // value list.
        opens_selector: false,
        list_title: "",
        alt_names: &[],
        completion_priority: 0,
    },
];

/// A parsed slash draft: the command token and an optional remainder. For
/// `"/mod"` → `{ name: "mod", rest: None }`; for `"/model qw"` →
/// `{ name: "model", rest: Some("qw") }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashDraft {
    pub name: String,
    pub rest: Option<String>,
}

/// True when the draft, ignoring leading whitespace, begins with `/` - the
/// signal the Composer is in Slash Command mode.
pub fn is_slash(draft: &str) -> bool {
    draft.trim_start().starts_with('/')
}

/// Splits a slash draft into its command token and optional remainder. The
/// leading `/` (and any whitespace before it) is stripped; the FIRST space
/// after the token is the boundary - everything past it is `rest`. A token with
/// no following space (or nothing after `/`) has `rest: None`; a trailing space
/// with no remainder (`"/model "`) yields `rest: Some("")`, so a committed
/// command with a bare trailing space still seeds an empty sub-filter.
pub fn parse(draft: &str) -> SlashDraft {
    let body = draft.trim_start().strip_prefix('/').unwrap_or(draft);
    match body.split_once(' ') {
        Some((name, rest)) => SlashDraft {
            name: name.to_string(),
            rest: Some(rest.to_string()),
        },
        None => SlashDraft {
            name: body.to_string(),
            rest: None,
        },
    }
}

/// The descriptor whose name matches `name` exactly, if any.
pub fn lookup(name: &str) -> Option<&'static SlashCommand> {
    COMMANDS.iter().find(|c| c.name == name)
}

#[cfg(test)]
#[path = "../../tests/ui/slash.rs"]
mod tests;
