/// Matches `text` against `pattern`, returning `true` if and only if the
/// pattern matches the *entire* text (anchored matching, not substring search).
///
/// # Pattern language
///
/// - `*` matches any sequence of characters, including the empty sequence.
/// - `?` matches exactly one character.
/// - `[abc]` is a character class: it matches exactly one character that is
///   `a`, `b`, or `c`.
/// - Ranges are allowed inside classes: `[a-z]` matches one character between
///   `a` and `z` inclusive. Ranges and literals may be mixed, e.g. `[a-c9]`.
/// - `[!abc]` is a negated class: it matches exactly one character that is
///   NOT in the class.
/// - `\` escapes the next metacharacter, so `\*` matches a literal `*` and
///   `\[` matches a literal `[`.
/// - Every other character matches itself, literally. A `]` outside a class
///   is an ordinary literal character.
///
/// # Examples (per the spec above)
///
/// - `glob_match("src/*.rs", "src/lib.rs")` is `true`
/// - `glob_match("h?llo", "hello")` is `true`
/// - `glob_match("[!0-9]", "z")` is `true`
/// - `glob_match("hello", "hell")` is `false` (whole-text match required)
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // TODO: implement the matcher described in the doc comment above.
    let _ = (pattern, text);
    false
}
