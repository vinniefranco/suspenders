
/// Errors produced by [`decode`]. Tests match on these variants exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// A `)` with no matching open group, or a group that is never closed.
    UnbalancedParens,
    /// A repeat group whose count is zero (`0(x)`), a `(` with no digits
    /// immediately before it, or a count too large to represent.
    BadRepeatCount,
    /// A substitution `&k;` where `k` has not been bound by an earlier
    /// definition.
    UnknownKey,
    /// A definition `!k=...;` for a key that is already bound. Note this
    /// also fires when a definition sits inside a repeated group with
    /// count >= 2, because the group body executes once per repetition.
    DuplicateKey,
    /// A `\` followed by anything other than `(`, `)`, `&`, `!`, `;`, `\`,
    /// or a `\` at end of input.
    BadEscape,
    /// A definition `!k=VALUE;` that is malformed (key is not a single
    /// lowercase ASCII letter, missing `=`) or whose terminating `;` is
    /// never reached before the enclosing group closes or input ends.
    UnterminatedDefinition,
    /// A substitution `&k;` that is malformed (key is not a single
    /// lowercase ASCII letter) or missing its terminating `;`.
    UnterminatedSubstitution,
}

/// Decode a string written in a small run-length + dictionary mini-language.
///
/// # Language
///
/// Input is processed left to right. Ordinary characters pass through to
/// the output unchanged.
///
/// ## Repeat groups: `N(...)`
///
/// A run of decimal digits immediately followed by `(` opens a repeat
/// group. The group body extends to the matching `)` and is *executed*
/// `N` times, with each execution's output appended. Counts may be
/// multi-digit: `12(a)` yields twelve `a`s. Groups nest:
/// `2(a3(b))` yields `abbbabbb`.
///
/// * `N` must be a positive integer: `0(x)` (and `00(x)`) is
///   [`DecodeError::BadRepeatCount`].
/// * A `(` *not* immediately preceded by digits is also
///   [`DecodeError::BadRepeatCount`].
/// * Digits **not** immediately followed by `(` are ordinary literal
///   characters: `a2b` decodes to `a2b`, `12x` to `12x`, and `2\(a\)`
///   to `2(a)` (the escaped paren does not open a group).
/// * A stray `)` or a group that is never closed is
///   [`DecodeError::UnbalancedParens`].
///
/// ## Definitions: `!k=VALUE;` and substitutions: `&k;`
///
/// `!k=VALUE;` binds the single lowercase ASCII letter `k` to the result
/// of decoding `VALUE`, and produces **no output** where it appears.
/// `VALUE` is itself decoded with the full language, so it may use repeat
/// groups and substitutions of keys bound *earlier* (a key bound later is
/// [`DecodeError::UnknownKey`] at the point of use). `&k;` appends the
/// string bound to `k`.
///
/// * Bindings are global: a key bound inside a group stays bound after
///   the group closes.
/// * Definitions may appear anywhere: at top level or inside a group.
///   Because a repeated group body executes once per repetition, a
///   definition inside a group with count >= 2 executes twice and is
///   [`DecodeError::DuplicateKey`]. Inside a `1(...)` group it is fine.
/// * Rebinding an already-bound key is [`DecodeError::DuplicateKey`].
/// * Using `&k;` with an unbound `k` is [`DecodeError::UnknownKey`].
/// * In a definition, a missing `=`, a key that is not a single lowercase
///   ASCII letter, or a terminating `;` that is never reached (end of
///   input, or the enclosing group closes first) is
///   [`DecodeError::UnterminatedDefinition`].
/// * In a substitution, a key that is not a single lowercase ASCII letter
///   or a missing `;` is [`DecodeError::UnterminatedSubstitution`].
/// * A bare `;` terminates the innermost enclosing definition VALUE;
///   anywhere else it is an ordinary literal character. A bare `=` is
///   always an ordinary literal.
///
/// ## Escapes
///
/// `\(`, `\)`, `\&`, `\!`, `\;`, and `\\` produce the literal character
/// after the backslash, stripped of any special meaning. A `\` followed
/// by any other character, or a `\` at end of input, is
/// [`DecodeError::BadEscape`].
///
/// ## Misc
///
/// Empty input decodes to the empty string.
pub fn decode(input: &str) -> Result<String, DecodeError> {
    // TODO: implement the decoder described in the doc comment above.
    let _ = input;
    Ok(String::new())
}
