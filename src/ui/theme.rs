//! The Theme core (ADR-0038, CONTEXT.md: **Theme**): a named coloring of the
//! semantic display vocabulary, stated as sparse TOML over the app's own
//! slots. Built-ins (`dark`, `light`) are embedded TOML parsed by the same
//! code as user files; user themes live as `*.toml` in a caller-supplied
//! themes directory, filename stem as identity.
//!
//! This module owns its [`Color`] type and never imports ratatui/crossterm -
//! only `ui::components` and the `ui` adapter touch the terminal (ADR-0019
//! invariant). The mapping from a resolved [`Theme`]'s slots to terminal
//! styles stays in `ui::components`; here live the schema, the parser, and
//! the resolution rule.
//!
//! ## The contract (ADR-0038)
//!
//! - Keys are the slots themselves, sparse: unstated slots fall back to the
//!   built-in `dark` Theme, which is total by construction (the fallback
//!   floor). Colors only - bold/italic/underline are meaning, not a Theme's
//!   to change, so the bold-only Emphasis style has no slot.
//! - A slot value is `#rrggbb` hex or an ANSI-16 name. The built-in `dark`
//!   keeps ANSI names where today's palette uses them, so it stays
//!   terminal-respecting.
//! - One optional top-level `syntax` key names a bundled syntect theme for
//!   code blocks (default flows from `dark`: `base16-ocean.dark`).
//! - Strict per-file: any error - bad TOML, unknown key, unparsable color,
//!   unknown syntax name - rejects the whole file with one human-readable
//!   reason ([`ThemeError`]); `/theme` will show it verbatim. Resilience
//!   (falling back to `dark` with a notice) is the caller's job.

pub mod active;
pub mod color;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use syntect::highlighting::ThemeSet;

pub use active::ActiveTheme;
pub use color::Color;

// ---------------------------------------------------------------------------
// The error - one human-readable reason, shown verbatim by /theme.
// ---------------------------------------------------------------------------

/// Why a Theme was refused. Strict per-file (ADR-0038): every variant rejects
/// whole, never half-applies; the startup fallback and `/theme`'s unselectable
/// listing consume these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    /// The whole-file rejection reason: bad TOML, an unknown key, an
    /// unparsable color, an unknown syntax theme name, or - for a built-in -
    /// a missing slot.
    Invalid(String),
    /// No built-in and no `{name}.toml` in the themes directory.
    NotFound(String),
    /// A user file whose stem is a built-in name: built-ins win and the file
    /// is refused, so a theme name never means two things.
    ShadowsBuiltIn(String),
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Invalid(reason) => write!(f, "{reason}"),
            ThemeError::NotFound(name) => write!(f, "no theme named \"{name}\""),
            ThemeError::ShadowsBuiltIn(name) => {
                write!(f, "shadows the built-in \"{name}\" theme")
            }
        }
    }
}

impl std::error::Error for ThemeError {}

// ---------------------------------------------------------------------------
// The slot inventory - the user-facing schema (ADR-0038: renaming or removing
// a slot is a contract break; adding one is routine).
// ---------------------------------------------------------------------------

/// Declares the slot inventory ONCE and derives every shape that must agree
/// on it: the raw `[colors]` file table (`deny_unknown_fields`, so a typo'd
/// slot rejects the file), the sparse Theme, the total Theme, and the
/// per-slot parse/resolve plumbing.
macro_rules! theme_slots {
    ($($(#[$doc:meta])* $slot:ident),* $(,)?) => {
        /// The raw `[colors]` table as the file states it: every slot an
        /// optional string, unknown keys rejected. A DTO in the
        /// `FileConfig` mold (ADR-0031) - parsing to [`Color`] happens after,
        /// so a bad value names its slot.
        #[derive(Debug, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ColorsFile {
            $($slot: Option<String>,)*
        }

        /// A sparse Theme: exactly what one file states, nothing resolved.
        /// `/theme`'s preview resolves it over the built-in floor with
        /// [`SparseTheme::over`] at selection time.
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub struct SparseTheme {
            /// The bundled syntect theme for code blocks, when stated.
            pub syntax: Option<String>,
            $($(#[$doc])* pub $slot: Option<Color>,)*
        }

        /// A total Theme: every slot carries a color, `syntax` names a
        /// bundled syntect theme. What the presentation boundary draws from.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Theme {
            /// The bundled syntect theme code blocks highlight with.
            pub syntax: String,
            $($(#[$doc])* pub $slot: Color,)*
        }

        impl ColorsFile {
            /// Parses every stated value into a [`Color`]; the first bad one
            /// rejects the whole file, naming its slot (ADR-0038 strictness).
            fn into_sparse(self, syntax: Option<String>) -> Result<SparseTheme, ThemeError> {
                Ok(SparseTheme {
                    syntax,
                    $($slot: parse_slot(stringify!($slot), self.$slot)?,)*
                })
            }
        }

        impl SparseTheme {
            /// Lays this sparse Theme over `base`: every stated slot wins,
            /// everything unstated reads from `base` (ADR-0038 sparsity - a
            /// three-line theme is valid).
            pub fn over(&self, base: &Theme) -> Theme {
                Theme {
                    syntax: self.syntax.clone().unwrap_or_else(|| base.syntax.clone()),
                    $($slot: self.$slot.unwrap_or(base.$slot),)*
                }
            }

            /// The total Theme this file states outright, `Err` naming the
            /// first missing slot. Built-ins only: `dark` is the fallback
            /// floor and must be total (ADR-0038); user themes resolve by
            /// [`SparseTheme::over`] instead.
            fn total(self) -> Result<Theme, ThemeError> {
                Ok(Theme {
                    syntax: self.syntax.ok_or_else(|| missing_slot("syntax"))?,
                    $($slot: self.$slot.ok_or_else(|| missing_slot(stringify!($slot)))?,)*
                })
            }
        }
    };
}

// One line per color decision in ui::components (the single semantic → color
// mapping, ADR-0008). The per-slot docs land on the generated `Theme` and
// `SparseTheme` fields - the user-facing contract; the `fg`/`bg` suffixes
// mark the two halves of a powerline segment pair.
theme_slots! {
    /// Diff added lines in the conversation plane.
    added,
    /// Diff removed lines in the conversation plane.
    removed,
    /// The full-width background tint behind a diff's added lines (ADR-0008):
    /// the semantic meaning as a GitHub-style band, over which the syntect
    /// foreground layers. Subtle - it composites over the terminal ground.
    added_bg,
    /// The full-width background tint behind a diff's removed lines (ADR-0008),
    /// the removed counterpart to `added_bg`.
    removed_bg,
    /// Diff context (unchanged) lines.
    context,
    /// Dimmed secondary text: info lines, hints, quiet chrome.
    muted,
    /// Tool-call/result machinery lines.
    machinery,
    /// Error lines and failure notices.
    error,
    /// qwen `text.primary` (Foreground `#bfbdb6`, Phase 7, ADR-0008): body/info
    /// text and tool names - the pinned default reading colour.
    foreground,
    /// qwen `text.accent` (AccentPurple `#D2A6FF`, Phase 7, ADR-0008): the user
    /// `>` caret and the assistant `✦` marker.
    accent,
    /// qwen `status.success` (AccentGreen `#AAD94C`, Phase 7, ADR-0008): the `✓`
    /// success prefix and the `✓`/`o` tool markers.
    success,
    /// qwen `status.warning` (AccentYellow `#FFD700`, Phase 7, ADR-0008): the `△`
    /// warning prefix and a pending tool-group border.
    warning,
    /// Settled/streaming thinking lines (grey; hidden under compact mode, Ctrl+O).
    thinking,
    /// The live `✦ Thinking` header over the streaming reasoning tail
    /// (ADR-0040): the animated brain, where motion sits during a Run.
    thinking_header,
    /// The lull "waiting" animation + its elapsed timer (the spellcast scenes):
    /// quiet chrome under the running lane, so it reads muted by default. Named
    /// for the lull it fills - a quiet stretch WITHIN a running Run, distinct
    /// from the Agent being Idle.
    lull,
    /// The `>` gutter marking the user's own prompts.
    prompt_gutter,
    /// The dim `│` run-lane spine the agent's whole Run hangs off (ADR-0040):
    /// background chrome, so it recedes like the machinery plane.
    lane_spine,
    /// The Housekeeping marker plane (ADR-0040): Compaction, Result-Cap cuts.
    /// Neutral gray - routine tidying, not a judgment.
    marker_housekeeping,
    /// The Aid marker plane (ADR-0040): a marker that helps the model. Warm
    /// amber, kept clear of error-red. Reserved - no producer emits it since
    /// the nudge apparatus was removed.
    marker_aid,
    /// The Constrain marker plane (ADR-0040): a guard limiting the model - the
    /// loop-detector's run-close. Cool blue, clear of green.
    marker_constrain,
    /// Assistant markdown headings.
    heading,
    /// Assistant markdown list bullets.
    bullet,
    /// Assistant markdown block quotes.
    quote,
    /// Assistant markdown links.
    link,
    /// Inline code spans.
    code,
    /// Code-block text (the fallback fg when syntect has no syntax).
    code_block,
    /// The code-block background, behind `code_block` and syntect fragments.
    code_block_bg,
    /// The Composer popup and modal border.
    popup_border,
}

/// One stated slot value as a [`Color`]; a bad value's reason names the slot.
fn parse_slot(slot: &str, value: Option<String>) -> Result<Option<Color>, ThemeError> {
    match value {
        None => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|e| ThemeError::Invalid(format!("colors.{slot}: {e}"))),
    }
}

fn missing_slot(slot: &str) -> ThemeError {
    ThemeError::Invalid(format!("missing slot \"{slot}\""))
}

// ---------------------------------------------------------------------------
// Parsing - strict per-file (ADR-0038).
// ---------------------------------------------------------------------------

/// A theme file's top level: the optional `syntax` key and the `[colors]`
/// table. `deny_unknown_fields` here rejects stray top-level keys; the nested
/// [`ColorsFile`] rejects unknown slots.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    syntax: Option<String>,
    colors: Option<ColorsFile>,
}

impl SparseTheme {
    /// Pure parse of one theme file (the `FileConfig::parse` mold, ADR-0031):
    /// malformed TOML, an unknown key, an unparsable color, and an unknown
    /// syntax theme name each reject the WHOLE file with one reason. The
    /// message is path-agnostic; callers add the file identity.
    pub fn parse(raw: &str) -> Result<SparseTheme, ThemeError> {
        let file: ThemeFile =
            toml::from_str(raw).map_err(|e| ThemeError::Invalid(e.message().to_string()))?;
        if let Some(syntax) = &file.syntax {
            validate_syntax(syntax)?;
        }
        file.colors.unwrap_or_default().into_sparse(file.syntax)
    }
}

/// The bundled syntect [`ThemeSet`], loaded once and owned HERE: the ONE copy
/// [`validate_syntax`] and the code-fence highlighter both read, so a name
/// that validates is a name the highlighter has - agreement by construction,
/// not coincidence. Only bundled themes are nameable (ADR-0038 defers user
/// `.tmTheme` loading). syntect is not a terminal crate, so exposing it here
/// leaves the ADR-0019 boundary untouched.
pub fn syntax_theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// Rejects a `syntax` value naming no bundled syntect theme; the reason lists
/// what IS available, so the fix is in the message.
fn validate_syntax(name: &str) -> Result<(), ThemeError> {
    let themes = &syntax_theme_set().themes;
    if themes.contains_key(name) {
        return Ok(());
    }
    let available = themes.keys().cloned().collect::<Vec<_>>().join(", ");
    Err(ThemeError::Invalid(format!(
        "syntax: \"{name}\" is not a bundled syntax theme (available: {available})"
    )))
}

// ---------------------------------------------------------------------------
// Built-ins - embedded TOML, so the parser is dogfooded (ADR-0038).
// ---------------------------------------------------------------------------

/// The built-in theme names, in `/theme`'s listing order. Reserved: a user
/// file with one of these stems is refused ([`ThemeError::ShadowsBuiltIn`]),
/// so a built-in is never shadowed. A drift test pins that every name here
/// resolves through [`built_in`].
pub const BUILT_INS: &[&str] = &["dark", "light"];

/// THE built-in name → Theme mapping. [`is_built_in`] and [`load`] derive
/// from it, so a built-in cannot half-exist (listed but unloadable, or
/// loadable but shadowable).
pub fn built_in(name: &str) -> Option<&'static Theme> {
    match name {
        "dark" => Some(dark()),
        "light" => Some(light()),
        _ => None,
    }
}

const DARK_TOML: &str = include_str!("themes/dark.toml");
const LIGHT_TOML: &str = include_str!("themes/light.toml");

/// The built-in `dark` Theme: today's palette (ADR-0038) and the fallback
/// floor every sparse Theme resolves over. Total by construction - the
/// embedded file states every slot, and a test pins both totality and the
/// exact palette.
pub fn dark() -> &'static Theme {
    static DARK: OnceLock<Theme> = OnceLock::new();
    DARK.get_or_init(|| {
        SparseTheme::parse(DARK_TOML)
            .and_then(SparseTheme::total)
            .expect("the embedded dark theme is valid and total")
    })
}

/// The built-in `light` Theme: the light-background counterpart proving the
/// schema covers both polarities (ADR-0038). Resolved over [`dark`] like any
/// other theme (its file states every slot anyway; a test pins that).
pub fn light() -> &'static Theme {
    static LIGHT: OnceLock<Theme> = OnceLock::new();
    LIGHT.get_or_init(|| {
        SparseTheme::parse(LIGHT_TOML)
            .expect("the embedded light theme is valid")
            .over(dark())
    })
}

fn is_built_in(name: &str) -> bool {
    built_in(name).is_some()
}

// ---------------------------------------------------------------------------
// Discovery + loading. The themes directory path comes from the caller - the
// XDG resolution lives at the edge, like session.rs's config path (ADR-0031).
// ---------------------------------------------------------------------------

/// Every user theme file in `dir`, sorted by name: `(stem, parse result)`, so
/// a broken file carries its reason into `/theme`'s unselectable listing. A
/// stem colliding with a built-in is refused
/// ([`ThemeError::ShadowsBuiltIn`]) - built-ins win. Built-ins themselves are
/// NOT listed here; they need no discovery.
pub fn discover(dir: &Path) -> Vec<(String, Result<SparseTheme, ThemeError>)> {
    theme_files(dir)
        .into_iter()
        .map(|(name, path)| {
            let parsed = read_theme(&name, &path);
            (name, parsed)
        })
        .collect()
}

/// The `*.toml` entries under `dir` as sorted `(stem, path)`. A missing or
/// unreadable directory is an empty set - user themes are optional.
fn theme_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stem = (path.extension().and_then(|e| e.to_str()) == Some("toml"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()?;
            Some((stem, path))
        })
        .collect();
    files.sort();
    files
}

/// One discovered file's parse: the reserved-name rule first, then the strict
/// per-file parse.
fn read_theme(name: &str, path: &Path) -> Result<SparseTheme, ThemeError> {
    if is_built_in(name) {
        return Err(ThemeError::ShadowsBuiltIn(name.to_string()));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| ThemeError::Invalid(e.to_string()))?;
    SparseTheme::parse(&raw)
}

/// Resolves a configured theme name to a total [`Theme`]: a built-in by name,
/// else `{name}.toml` under `dir` laid over the `dark` floor. The typed
/// errors ([`ThemeError::NotFound`] / [`ThemeError::Invalid`]) feed the
/// startup fallback: a missing or broken configured theme falls back to
/// `dark` with a visible notice, never a crash (ADR-0038).
pub fn load(name: &str, dir: &Path) -> Result<Theme, ThemeError> {
    match built_in(name) {
        Some(theme) => Ok(theme.clone()),
        None => load_user(name, dir),
    }
}

/// A user theme by name: read, strict-parse, resolve over the `dark` floor.
fn load_user(name: &str, dir: &Path) -> Result<Theme, ThemeError> {
    let path = dir.join(format!("{name}.toml"));
    if !path.is_file() {
        return Err(ThemeError::NotFound(name.to_string()));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| ThemeError::Invalid(e.to_string()))?;
    Ok(SparseTheme::parse(&raw)?.over(dark()))
}

#[cfg(test)]
#[path = "../../tests/ui/theme.rs"]
mod tests;
