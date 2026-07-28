//! Slash Command registry and draft parsing - the PURE recognition core behind
//! the Composer's `/`-menu (ADR-0032, CONTEXT.md: **Slash Command**). No
//! ratatui/crossterm and no knowledge of what any command DOES: the pure
//! [`crate::ui::composer::Composer`] looks a command up here and emits an
//! [`Effect::Command`](crate::ui::screen::Effect::Command); the actual work
//! runs adapter-side in that Effect's arm.
//!
//! ## The registry is the extension seam
//!
//! [`COMMANDS`] is a `&'static` slice of descriptors. Adding `/theme` later is
//! one more entry, not a new widget: the menu, filter, and generic selector
//! ([`crate::ui::selector`]) are untouched. Today it holds exactly one entry,
//! `/model`.
//!
//! ## Parsing a slash draft
//!
//! A slash draft is `/name[ rest]`: the FIRST space separates the command token
//! (`name`) from an optional remainder (`rest`). [`parse`] splits it; [`lookup`]
//! resolves a name to a descriptor; [`rows`] runs the (optionally
//! token-filtered) registry into [`SelectorRow`]s so the menu renders through
//! the generic selector.

use crate::ui::selector::SelectorRow;

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
}

/// The available Slash Commands (ADR-0032's `&'static` registry); `/compact`,
/// … are each one more entry.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        help: "choose the model for this session",
        opens_selector: true,
        list_title: "models",
    },
    SlashCommand {
        name: "theme",
        help: "choose the theme for this session",
        opens_selector: true,
        list_title: "themes",
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

/// The registry as [`SelectorRow`]s (value = label = name, hint = help),
/// keeping only commands whose name contains `filter` (case-insensitive
/// substring) so the menu narrows as the command token is typed. An empty
/// filter yields every command.
pub fn rows(filter: &str) -> Vec<SelectorRow> {
    let needle = filter.to_lowercase();
    COMMANDS
        .iter()
        .filter(|c| c.name.to_lowercase().contains(&needle))
        .map(|c| SelectorRow::new(c.name, c.name, Some(c.help.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_slash ----------------------------------------------------------

    #[test]
    fn is_slash_true_only_when_the_trimmed_draft_starts_with_a_slash() {
        assert!(is_slash("/"));
        assert!(is_slash("/model"));
        assert!(is_slash("   /model"), "leading whitespace is ignored");
        assert!(!is_slash(""));
        assert!(!is_slash("model"));
        assert!(
            !is_slash("fix the /bug"),
            "a slash mid-text is not a command"
        );
    }

    // --- parse -------------------------------------------------------------

    #[test]
    fn parse_lone_slash_is_an_empty_name_no_rest() {
        assert_eq!(
            parse("/"),
            SlashDraft {
                name: "".into(),
                rest: None,
            }
        );
    }

    #[test]
    fn parse_a_bare_command_has_no_rest() {
        assert_eq!(
            parse("/model"),
            SlashDraft {
                name: "model".into(),
                rest: None,
            }
        );
        // A partial token still parses (it filters the menu).
        assert_eq!(
            parse("/mod"),
            SlashDraft {
                name: "mod".into(),
                rest: None,
            }
        );
    }

    #[test]
    fn parse_a_trailing_space_yields_an_empty_rest() {
        assert_eq!(
            parse("/model "),
            SlashDraft {
                name: "model".into(),
                rest: Some("".into()),
            }
        );
    }

    #[test]
    fn parse_splits_the_remainder_at_the_first_space() {
        assert_eq!(
            parse("/model qw"),
            SlashDraft {
                name: "model".into(),
                rest: Some("qw".into()),
            }
        );
        // Only the first space is the boundary; later spaces live in rest.
        assert_eq!(
            parse("/model qwen 2"),
            SlashDraft {
                name: "model".into(),
                rest: Some("qwen 2".into()),
            }
        );
    }

    #[test]
    fn parse_ignores_whitespace_before_the_slash() {
        assert_eq!(
            parse("  /model qw"),
            SlashDraft {
                name: "model".into(),
                rest: Some("qw".into()),
            }
        );
    }

    // --- lookup ------------------------------------------------------------

    #[test]
    fn lookup_matches_a_registered_name_exactly() {
        assert_eq!(lookup("model"), Some(&COMMANDS[0]));
        assert_eq!(lookup("theme"), Some(&COMMANDS[1]));
        assert_eq!(lookup("mod"), None, "partial names never resolve");
        assert_eq!(lookup("compact"), None);
        assert_eq!(lookup(""), None);
    }

    #[test]
    fn every_selector_command_states_its_list_title() {
        // The popup titles itself from the descriptor, so a selector-opening
        // command without a plural noun would paint an empty title.
        for c in COMMANDS.iter().filter(|c| c.opens_selector) {
            assert!(!c.list_title.is_empty(), "/{} has no list_title", c.name);
        }
        assert_eq!(lookup("model").unwrap().list_title, "models");
        assert_eq!(lookup("theme").unwrap().list_title, "themes");
    }

    #[test]
    fn model_and_theme_open_a_selector_sub_state() {
        assert!(
            lookup("model").expect("model is registered").opens_selector,
            "committing /model switches the popup to its own value list"
        );
        assert!(
            lookup("theme").expect("theme is registered").opens_selector,
            "committing /theme switches the popup to the theme list (ADR-0038)"
        );
    }

    // --- rows --------------------------------------------------------------

    #[test]
    fn rows_empty_filter_yields_every_command() {
        let rows = rows("");
        assert_eq!(rows.len(), COMMANDS.len());
        assert_eq!(
            rows[0],
            SelectorRow::new(
                "model",
                "model",
                Some("choose the model for this session".into())
            )
        );
    }

    #[test]
    fn rows_filter_is_case_insensitive_substring_on_the_name() {
        assert_eq!(rows("mod").len(), 1);
        assert_eq!(rows("MODEL").len(), 1);
        assert_eq!(rows("ode").len(), 1, "substring, not just prefix");
        assert!(rows("zzz").is_empty());
    }
}
