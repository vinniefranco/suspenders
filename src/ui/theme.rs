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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use syntect::highlighting::ThemeSet;

// ---------------------------------------------------------------------------
// Color - this module's own, ratatui-free (ADR-0019).
// ---------------------------------------------------------------------------

/// A Theme color: one of the 16 ANSI names (drawn from the user's terminal
/// palette) or a truecolor RGB value. Mirrors the terminal color model without
/// importing it - `ui::components` translates at the presentation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Rgb(u8, u8, u8),
}

/// The accepted ANSI-16 names, for the rejection message.
const ANSI_NAMES: &str = "black, red, green, yellow, blue, magenta, cyan, gray, dark_gray, \
     light_red, light_green, light_yellow, light_blue, light_magenta, light_cyan, white";

impl std::str::FromStr for Color {
    type Err = String;

    /// Parses `#rrggbb` (case-insensitive hex) or an ANSI-16 snake_case name;
    /// anything else is rejected with the full accepted vocabulary.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with('#') {
            return parse_hex(s);
        }
        ansi_by_name(s).ok_or_else(|| {
            format!("\"{s}\" is not a color: expected \"#rrggbb\" or an ANSI name ({ANSI_NAMES})")
        })
    }
}

/// One `rr`/`gg`/`bb` pair of `digits` as a byte; `None` off the string's end,
/// on a non-hex digit, or on a non-ASCII boundary (`get` guards the slice).
fn hex_pair(digits: &str, at: usize) -> Option<u8> {
    u8::from_str_radix(digits.get(at..at + 2)?, 16).ok()
}

/// Parses a `#`-prefixed hex color; exactly six hex digits or rejection.
fn parse_hex(s: &str) -> Result<Color, String> {
    let digits = s.strip_prefix('#').unwrap_or(s);
    let rgb = (digits.len() == 6)
        .then(|| {
            Some((
                hex_pair(digits, 0)?,
                hex_pair(digits, 2)?,
                hex_pair(digits, 4)?,
            ))
        })
        .flatten();
    match rgb {
        Some((r, g, b)) => Ok(Color::Rgb(r, g, b)),
        None => Err(format!(
            "\"{s}\" is not a valid hex color: expected \"#rrggbb\""
        )),
    }
}

/// The ANSI-16 name table: snake_case, mirroring the terminal palette's
/// conventional names. Exact match - strictness keeps typos loud (ADR-0038).
fn ansi_by_name(name: &str) -> Option<Color> {
    match name {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" => Some(Color::Gray),
        "dark_gray" => Some(Color::DarkGray),
        "light_red" => Some(Color::LightRed),
        "light_green" => Some(Color::LightGreen),
        "light_yellow" => Some(Color::LightYellow),
        "light_blue" => Some(Color::LightBlue),
        "light_magenta" => Some(Color::LightMagenta),
        "light_cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

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
    ($($slot:ident),* $(,)?) => {
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
            $(pub $slot: Option<Color>,)*
        }

        /// A total Theme: every slot carries a color, `syntax` names a
        /// bundled syntect theme. What the presentation boundary draws from.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Theme {
            /// The bundled syntect theme code blocks highlight with.
            pub syntax: String,
            $(pub $slot: Color,)*
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
// mapping, ADR-0008). Grouped by the surface the slot colors; the `fg`/`bg`
// suffixes mark the two halves of a powerline segment pair.
theme_slots! {
    // The conversation plane: diff lines, dimmed machinery, the gutters.
    added,
    removed,
    context,
    muted,
    machinery,
    error,
    thinking,
    prompt_gutter,
    // Assistant markdown.
    heading,
    bullet,
    quote,
    link,
    code,
    code_block,
    code_bg,
    // Chrome.
    popup_border,
    // The powerline status bar. `segment_muted_bg` is the shared quiet
    // background of the low-emphasis segments (thinking/tools toggles, cost,
    // tokens at Ok pressure).
    bar_bg,
    segment_muted_bg,
    segment_idle_fg,
    segment_idle_bg,
    segment_running_fg,
    segment_running_bg,
    segment_model_fg,
    segment_model_bg,
    segment_toggle_fg,
    segment_cost_fg,
    pressure_ok_fg,
    pressure_elevated_fg,
    pressure_elevated_bg,
    pressure_critical_fg,
    pressure_critical_bg,
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

/// The bundled syntect theme names, loaded once. Only bundled themes are
/// nameable (ADR-0038 defers user `.tmTheme` loading).
fn syntax_theme_names() -> &'static BTreeSet<String> {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| ThemeSet::load_defaults().themes.into_keys().collect())
}

/// Rejects a `syntax` value naming no bundled syntect theme; the reason lists
/// what IS available, so the fix is in the message.
fn validate_syntax(name: &str) -> Result<(), ThemeError> {
    if syntax_theme_names().contains(name) {
        return Ok(());
    }
    let available = syntax_theme_names()
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    Err(ThemeError::Invalid(format!(
        "syntax: \"{name}\" is not a bundled syntax theme (available: {available})"
    )))
}

// ---------------------------------------------------------------------------
// Built-ins - embedded TOML, so the parser is dogfooded (ADR-0038).
// ---------------------------------------------------------------------------

/// The built-in theme names. Reserved: a user file with one of these stems is
/// refused ([`ThemeError::ShadowsBuiltIn`]), so a built-in is never shadowed.
pub const BUILT_INS: [&str; 2] = ["dark", "light"];

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
    BUILT_INS.contains(&name)
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
    match name {
        "dark" => Ok(dark().clone()),
        "light" => Ok(light().clone()),
        _ => load_user(name, dir),
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
mod tests {
    use super::*;

    // --- Color parsing ------------------------------------------------------

    #[test]
    fn every_ansi_16_name_parses_to_its_variant() {
        let names: [(&str, Color); 16] = [
            ("black", Color::Black),
            ("red", Color::Red),
            ("green", Color::Green),
            ("yellow", Color::Yellow),
            ("blue", Color::Blue),
            ("magenta", Color::Magenta),
            ("cyan", Color::Cyan),
            ("gray", Color::Gray),
            ("dark_gray", Color::DarkGray),
            ("light_red", Color::LightRed),
            ("light_green", Color::LightGreen),
            ("light_yellow", Color::LightYellow),
            ("light_blue", Color::LightBlue),
            ("light_magenta", Color::LightMagenta),
            ("light_cyan", Color::LightCyan),
            ("white", Color::White),
        ];
        for (name, expected) in names {
            assert_eq!(name.parse(), Ok(expected), "{name}");
        }
    }

    #[test]
    fn hex_parses_case_insensitively() {
        assert_eq!("#b9d7b4".parse(), Ok(Color::Rgb(185, 215, 180)));
        assert_eq!("#B9D7B4".parse(), Ok(Color::Rgb(185, 215, 180)));
        assert_eq!("#000000".parse(), Ok(Color::Rgb(0, 0, 0)));
        assert_eq!("#FFffFF".parse(), Ok(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn bad_hex_is_rejected_with_the_expected_shape() {
        for bad in ["#12345", "#1234567", "#12345g", "#", "#ééé"] {
            let err = bad.parse::<Color>().unwrap_err();
            assert_eq!(
                err,
                format!("\"{bad}\" is not a valid hex color: expected \"#rrggbb\"")
            );
        }
    }

    #[test]
    fn an_unknown_name_is_rejected_listing_the_vocabulary() {
        let err = "mauve".parse::<Color>().unwrap_err();
        assert!(err.contains("\"mauve\" is not a color"), "{err}");
        assert!(err.contains("#rrggbb"), "{err}");
        assert!(err.contains("dark_gray"), "the full name list is in: {err}");
        // Names are exact: no case-folding, no hyphens (strictness, ADR-0038).
        assert!("Cyan".parse::<Color>().is_err());
        assert!("dark-gray".parse::<Color>().is_err());
        assert!("".parse::<Color>().is_err());
    }

    // --- SparseTheme::parse (strict per-file, ADR-0038) ---------------------

    #[test]
    fn a_three_line_theme_is_valid_and_sparse() {
        let sparse = SparseTheme::parse("[colors]\nadded = \"#00ff00\"\nheading = \"magenta\"\n")
            .expect("sparse themes parse");
        assert_eq!(sparse.added, Some(Color::Rgb(0, 255, 0)));
        assert_eq!(sparse.heading, Some(Color::Magenta));
        assert_eq!(sparse.removed, None, "unstated slots stay unstated");
        assert_eq!(sparse.syntax, None);
    }

    #[test]
    fn an_empty_file_is_a_valid_all_default_theme() {
        assert_eq!(
            SparseTheme::parse("").expect("empty is valid"),
            SparseTheme::default()
        );
    }

    #[test]
    fn malformed_toml_rejects_the_file() {
        let err = SparseTheme::parse("[colors\nadded = ").unwrap_err();
        assert!(matches!(err, ThemeError::Invalid(_)), "{err}");
    }

    #[test]
    fn an_unknown_top_level_key_rejects_the_file() {
        let err = SparseTheme::parse("bold = true\n").unwrap_err();
        let ThemeError::Invalid(reason) = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(reason.contains("unknown field `bold`"), "{reason}");
    }

    #[test]
    fn an_unknown_color_slot_rejects_the_file() {
        let err = SparseTheme::parse("[colors]\nadedd = \"green\"\n").unwrap_err();
        let ThemeError::Invalid(reason) = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(reason.contains("unknown field `adedd`"), "{reason}");
    }

    #[test]
    fn an_unparsable_color_rejects_the_file_naming_its_slot() {
        let err = SparseTheme::parse("[colors]\nadded = \"greenish\"\n").unwrap_err();
        let ThemeError::Invalid(reason) = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(reason.starts_with("colors.added:"), "{reason}");
        assert!(reason.contains("\"greenish\" is not a color"), "{reason}");
    }

    #[test]
    fn a_non_string_color_value_rejects_the_file() {
        let err = SparseTheme::parse("[colors]\nadded = 3\n").unwrap_err();
        assert!(matches!(err, ThemeError::Invalid(_)), "{err}");
    }

    #[test]
    fn a_bundled_syntax_theme_name_is_accepted() {
        let sparse = SparseTheme::parse("syntax = \"Solarized (light)\"\n").expect("bundled name");
        assert_eq!(sparse.syntax.as_deref(), Some("Solarized (light)"));
    }

    #[test]
    fn an_unknown_syntax_theme_name_rejects_the_file_listing_what_exists() {
        let err = SparseTheme::parse("syntax = \"dracula\"\n").unwrap_err();
        let ThemeError::Invalid(reason) = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(
            reason.contains("\"dracula\" is not a bundled syntax theme"),
            "{reason}"
        );
        assert!(reason.contains("base16-ocean.dark"), "{reason}");
    }

    // --- resolution (sparse over the dark floor) ----------------------------

    #[test]
    fn an_empty_sparse_theme_over_dark_is_exactly_dark() {
        // Dark totality, from the other side: were any slot missing in
        // dark.toml, dark() itself would panic; this pins that resolution
        // adds nothing on top of a total floor.
        assert_eq!(SparseTheme::default().over(dark()), *dark());
    }

    #[test]
    fn a_stated_slot_wins_and_the_rest_falls_back() {
        let sparse = SparseTheme::parse("[colors]\nadded = \"#123456\"\n").expect("valid");
        let resolved = sparse.over(dark());
        assert_eq!(resolved.added, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(resolved.removed, dark().removed, "unstated → the floor");
        assert_eq!(resolved.syntax, dark().syntax);
    }

    #[test]
    fn a_stated_syntax_wins_over_the_floors() {
        let sparse = SparseTheme::parse("syntax = \"InspiredGitHub\"\n").expect("valid");
        assert_eq!(sparse.over(dark()).syntax, "InspiredGitHub");
    }

    // --- built-ins ----------------------------------------------------------

    #[test]
    fn dark_is_total_and_reproduces_todays_palette() {
        // Totality: the embedded file states every slot (total() would Err).
        let theme = SparseTheme::parse(DARK_TOML)
            .expect("dark parses")
            .total()
            .expect("dark states every slot");
        assert_eq!(theme, *dark());

        // The exact palette from ui::components - ANSI names where the code
        // used named colors, exact Rgb where it used Rgb (ADR-0038: the
        // default stays byte-identical and terminal-respecting).
        assert_eq!(theme.syntax, "base16-ocean.dark");
        assert_eq!(theme.added, Color::Green);
        assert_eq!(theme.removed, Color::Red);
        assert_eq!(theme.context, Color::DarkGray);
        assert_eq!(theme.muted, Color::DarkGray);
        assert_eq!(theme.machinery, Color::DarkGray);
        assert_eq!(theme.error, Color::Red);
        assert_eq!(theme.thinking, Color::DarkGray);
        assert_eq!(theme.prompt_gutter, Color::Cyan);
        assert_eq!(theme.heading, Color::Cyan);
        assert_eq!(theme.bullet, Color::Cyan);
        assert_eq!(theme.quote, Color::DarkGray);
        assert_eq!(theme.link, Color::Blue);
        assert_eq!(theme.code, Color::Yellow);
        assert_eq!(theme.code_block, Color::Rgb(185, 215, 180));
        assert_eq!(theme.code_bg, Color::Rgb(25, 25, 35));
        assert_eq!(theme.popup_border, Color::Cyan);
        assert_eq!(theme.bar_bg, Color::Rgb(30, 30, 40));
        assert_eq!(theme.segment_muted_bg, Color::Rgb(40, 44, 58));
        assert_eq!(theme.segment_idle_fg, Color::Black);
        assert_eq!(theme.segment_idle_bg, Color::Green);
        assert_eq!(theme.segment_running_fg, Color::Black);
        assert_eq!(theme.segment_running_bg, Color::Yellow);
        assert_eq!(theme.segment_model_fg, Color::Rgb(150, 160, 185));
        assert_eq!(theme.segment_model_bg, Color::Rgb(52, 58, 82));
        assert_eq!(theme.segment_toggle_fg, Color::DarkGray);
        assert_eq!(theme.segment_cost_fg, Color::Gray);
        assert_eq!(theme.pressure_ok_fg, Color::Gray);
        assert_eq!(theme.pressure_elevated_fg, Color::Black);
        assert_eq!(theme.pressure_elevated_bg, Color::Yellow);
        assert_eq!(theme.pressure_critical_fg, Color::Black);
        assert_eq!(theme.pressure_critical_bg, Color::Red);
    }

    #[test]
    fn light_is_total_valid_and_picks_a_light_syntax_theme() {
        // light() only needs to be a valid sparse file, but the shipped one
        // states every slot - a light theme leaning on dark fallbacks would
        // be unreadable, so totality is pinned here too.
        let theme = SparseTheme::parse(LIGHT_TOML)
            .expect("light parses")
            .total()
            .expect("light states every slot");
        assert_eq!(theme, *light());
        assert_eq!(theme.syntax, "base16-ocean.light");
        assert_ne!(theme.bar_bg, dark().bar_bg, "light is its own polarity");
    }

    #[test]
    fn a_missing_slot_fails_totality_naming_it() {
        let err = SparseTheme::parse("[colors]\nadded = \"green\"\n")
            .expect("valid sparse")
            .total()
            .unwrap_err();
        assert_eq!(err, ThemeError::Invalid("missing slot \"syntax\"".into()));
    }

    // --- discovery ----------------------------------------------------------

    fn write_theme(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("test file writes");
    }

    #[test]
    fn discovery_lists_toml_stems_sorted_with_per_file_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "zebra.toml", "[colors]\nadded = \"green\"\n");
        write_theme(
            dir.path(),
            "broken.toml",
            "[colors]\nadded = \"greenish\"\n",
        );
        write_theme(dir.path(), "notes.txt", "not a theme");

        let found = discover(dir.path());
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["broken", "zebra"], "sorted, non-toml ignored");
        let broken = found[0].1.as_ref().unwrap_err();
        assert!(
            broken.to_string().starts_with("colors.added:"),
            "a broken file carries its reason: {broken}"
        );
        assert_eq!(
            found[1].1.as_ref().expect("zebra is valid").added,
            Some(Color::Green)
        );
    }

    #[test]
    fn a_user_file_named_like_a_built_in_is_refused_as_a_shadow() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "dark.toml", "[colors]\nadded = \"blue\"\n");

        let found = discover(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0],
            (
                "dark".to_string(),
                Err(ThemeError::ShadowsBuiltIn("dark".into()))
            )
        );
    }

    #[test]
    fn a_missing_themes_directory_discovers_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover(&dir.path().join("no-such-dir")).is_empty());
    }

    // --- load (by configured name) ------------------------------------------

    #[test]
    fn built_ins_load_by_name_without_touching_the_directory() {
        let nowhere = Path::new("/no/such/dir");
        assert_eq!(load("dark", nowhere).expect("built-in"), *dark());
        assert_eq!(load("light", nowhere).expect("built-in"), *light());
    }

    #[test]
    fn a_built_in_name_wins_over_a_user_file_of_the_same_stem() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "dark.toml", "[colors]\nadded = \"blue\"\n");
        assert_eq!(
            load("dark", dir.path()).expect("the built-in loads"),
            *dark(),
            "the shadowing file is never read"
        );
    }

    #[test]
    fn a_user_theme_loads_resolved_over_the_dark_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "mine.toml", "[colors]\nadded = \"#101010\"\n");
        let theme = load("mine", dir.path()).expect("valid user theme");
        assert_eq!(theme.added, Color::Rgb(16, 16, 16));
        assert_eq!(theme.removed, dark().removed);
    }

    #[test]
    fn a_missing_theme_is_a_typed_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load("ghost", dir.path()).unwrap_err();
        assert_eq!(err, ThemeError::NotFound("ghost".into()));
        assert_eq!(err.to_string(), "no theme named \"ghost\"");
    }

    #[test]
    fn a_broken_theme_loads_as_invalid_with_its_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_theme(dir.path(), "typo.toml", "[colors]\nadedd = \"green\"\n");
        let err = load("typo", dir.path()).unwrap_err();
        let ThemeError::Invalid(reason) = &err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(reason.contains("unknown field `adedd`"), "{reason}");
    }

    // --- the error's display (shown verbatim by /theme) ---------------------

    #[test]
    fn shadow_errors_read_as_a_sentence() {
        assert_eq!(
            ThemeError::ShadowsBuiltIn("light".into()).to_string(),
            "shadows the built-in \"light\" theme"
        );
    }
}
