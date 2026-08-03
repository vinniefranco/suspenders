//! Slash Command registry and draft parsing - the PURE recognition core behind
//! the Composer's `/`-menu (ADR-0032, CONTEXT.md: **Slash Command**). No
//! ratatui/crossterm and no knowledge of what any command DOES: the pure
//! [`crate::ui::composer::Composer`] looks a command up here and emits an
//! [`Effect::Command`](crate::ui::screen::Effect::Command); the actual work
//! runs adapter-side in that Effect's arm.
//!
//! ## The two-layer registry is the extension seam
//!
//! [`COMMANDS`] is a `&'static` slice of BUILT-IN descriptors. Alongside it
//! sits a DYNAMIC command-source layer resolved at launch (ADR-0032/0058): a
//! runtime `&[`[`SkillCommand`]`]` the adapter feeds in from the discovered
//! [`crate::skills::SkillManager`], one entry per discovered skill. The registry
//! the pure core ranks and resolves over is the UNION of the two, projected to a
//! borrowed [`CommandRef`] so the fuzzy palette and lookup treat a built-in and
//! a skill command identically - the pure core never learns what a skill command
//! DOES (it emits the command-agnostic
//! [`Effect::Command`](crate::ui::screen::Effect::Command); the adapter maps it,
//! ADR-0019). Adding a command later is one more entry in either layer, not a new
//! widget: the fuzzy `/` palette ([`crate::ui::completion`], ADR-0051 System B)
//! ranks the union, and a selector-opening built-in's numbered `›` dialog
//! ([`crate::ui::selection`], System A) reuses the shared list.
//!
//! ## Parsing a slash draft
//!
//! A slash draft is `/name[ rest]`: the FIRST space separates the command token
//! (`name`) from an optional remainder (`rest`). [`parse`] splits it; [`lookup`]
//! resolves a name to a built-in descriptor, and [`lookup_ref`] resolves it over
//! the union (built-ins PLUS the runtime skill layer). The palette ranking lives
//! in [`crate::ui::completion::rank`], reading the union (name + `alt_names`).

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
    SlashCommand {
        // `/plan` (ADR-0067, qwen planCommand.ts): enter/exit Plan mode, or with
        // a trailing prompt enter Plan and submit it. Fire-and-run (no selector);
        // the help is qwen's verbatim `description`.
        name: "plan",
        help: "Switch to plan mode or exit plan mode",
        opens_selector: false,
        list_title: "",
        alt_names: &[],
        completion_priority: 0,
    },
];

/// One runtime (dynamic-source) command descriptor - the DYNAMIC layer beside
/// the `&'static` [`COMMANDS`] (ADR-0032/0058). Today the one source is
/// discovered Skills: the adapter builds one of these per discovered skill from
/// the [`crate::skills::SkillManager`] and feeds the list into the Composer at
/// launch (the same way history/header/theme launch data reaches the UI). A
/// skill command is always fire-and-run (`opens_selector` is not carried - a
/// skill never opens a value selector), so committing it emits a plain
/// [`Effect::Command`](crate::ui::screen::Effect::Command) the adapter maps to
/// the submit-prompt injection. The pure core stays command-agnostic: it ranks
/// and resolves this exactly like a built-in and never learns it is a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    /// The `/<name>` token, the discovered skill's name.
    pub name: String,
    /// The one-line menu help - the skill's `description`.
    pub help: String,
    /// The skill's `argument-hint`, shown after the command name in the palette
    /// (qwen `/<name> <argument-hint>`), or `None`. Display-only.
    pub argument_hint: Option<String>,
}

/// A borrowed, source-agnostic VIEW of one command - the union projection the
/// pure ranking/lookup fold over (ADR-0032's two-layer registry). A `&'static`
/// [`SlashCommand`] and a runtime [`SkillCommand`] both project to this, so
/// [`crate::ui::completion::rank`] and [`lookup_ref`] treat a built-in and a
/// skill command identically. `alt_names`/`completion_priority` are the
/// built-in-only ranking extras (a skill carries neither); `argument_hint` is
/// the skill-only completion annotation (a built-in carries none). The core
/// reads these fields to rank/render - never to decide what the command DOES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRef<'a> {
    pub name: &'a str,
    pub help: &'a str,
    pub opens_selector: bool,
    pub alt_names: &'a [&'a str],
    pub completion_priority: i32,
    /// The skill's `argument-hint` (dynamic layer only), shown after the name in
    /// the completion menu. `None` for a built-in.
    pub argument_hint: Option<&'a str>,
}

impl<'a> CommandRef<'a> {
    /// Projects a `&'static` built-in descriptor into the union view.
    pub fn from_static(command: &'a SlashCommand) -> Self {
        CommandRef {
            name: command.name,
            help: command.help,
            opens_selector: command.opens_selector,
            alt_names: command.alt_names,
            completion_priority: command.completion_priority,
            argument_hint: None,
        }
    }

    /// Projects a runtime skill descriptor into the union view (fire-and-run, no
    /// alt names, no priority nudge; its `argument-hint` rides along).
    pub fn from_skill(skill: &'a SkillCommand) -> Self {
        CommandRef {
            name: &skill.name,
            help: &skill.help,
            opens_selector: false,
            alt_names: &[],
            completion_priority: 0,
            argument_hint: skill.argument_hint.as_deref(),
        }
    }
}

/// The whole command registry as a source-agnostic union (ADR-0032): every
/// built-in [`COMMANDS`] entry first, then every runtime skill command whose
/// name does NOT collide with a built-in, each as a borrowed [`CommandRef`].
/// Built-ins lead AND win a name collision - a skill cannot shadow `/model`, so
/// a same-named skill is dropped from the union (it would otherwise rank as a
/// duplicate row and route to the wrong handler). The order is the ranking's
/// stable `original_index` basis, so it must not change between the rank and
/// lookup passes on one draft.
pub fn commands_ref(skills: &[SkillCommand]) -> Vec<CommandRef<'_>> {
    COMMANDS
        .iter()
        .map(CommandRef::from_static)
        .chain(
            skills
                .iter()
                .filter(|s| lookup(&s.name).is_none())
                .map(CommandRef::from_skill),
        )
        .collect()
}

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

/// The BUILT-IN descriptor whose name matches `name` exactly, if any. Resolves
/// only the `&'static` layer: a selector-opening command (`/model`, `/theme`)
/// is always a built-in, so the Composer's "does this open a selector?" and
/// "is this `/mcp`?" tests read this. A skill command is never selector-opening,
/// so [`lookup`] returning `None` for a `/<skill>` token is correct - a `/<skill>`
/// is resolved by ranking it in the [`commands_ref`] union, then committed
/// fire-and-run (the adapter maps its [`Effect::Command`](crate::ui::screen::Effect::Command)
/// to the skill's submit-prompt injection).
pub fn lookup(name: &str) -> Option<&'static SlashCommand> {
    COMMANDS.iter().find(|c| c.name == name)
}

#[cfg(test)]
#[path = "../../tests/ui/slash.rs"]
mod tests;
