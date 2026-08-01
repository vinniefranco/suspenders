//! The Theme [`Color`] model - this crate's own, ratatui-free (ADR-0019). A
//! leaf of `ui::theme`: the color type, its `#rrggbb`/ANSI-16 parse, and the
//! accepted vocabulary, with nothing of the theme schema, discovery, or
//! loading. `ui::components` translates a [`Color`] to a terminal style at the
//! presentation boundary; the theme schema stores these values and never
//! parses them itself.

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

/// The `#rrggbb` hex format requires exactly this many ASCII hex digit characters.
const HEX_DIGIT_COUNT: usize = 6;
/// Bit shift to extract the red byte from a packed `0xRRGGBB` value.
const RED_SHIFT: u32 = 16;
/// Bit shift to extract the green byte from a packed `0xRRGGBB` value.
const GREEN_SHIFT: u32 = 8;
/// Radix for hexadecimal parsing.
const HEX_RADIX: u32 = 16;

/// Parses a `#`-prefixed hex color; exactly six ASCII hex digits or rejection.
/// The digit check is explicit because `from_str_radix` alone accepts a
/// leading sign ("+12345" would sneak through a pairwise parse).
fn parse_hex(s: &str) -> Result<Color, String> {
    let digits = s.strip_prefix('#').unwrap_or(s);
    if digits.len() == HEX_DIGIT_COUNT && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        let rgb = u32::from_str_radix(digits, HEX_RADIX)
            .map_err(|e| format!("\"{s}\" is not a valid hex color: {e}"))?;
        return Ok(Color::Rgb(
            (rgb >> RED_SHIFT) as u8,
            (rgb >> GREEN_SHIFT) as u8,
            rgb as u8,
        ));
    }
    Err(format!(
        "\"{s}\" is not a valid hex color: expected \"#rrggbb\""
    ))
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

#[cfg(test)]
#[path = "../../../tests/ui/theme/color.rs"]
mod tests;
