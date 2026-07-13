//! Oracle tests for `glob_match`. Do not modify this file.
//!
//! Semantics under test are documented on `glob_match` in src/lib.rs:
//! anchored whole-text matching; `*` = any (possibly empty) sequence;
//! `?` = exactly one char; `[...]` classes with ranges; `[!...]` negation;
//! `\` escapes the next metacharacter.

use globber::glob_match;

#[test]
fn literal_match() {
    assert!(glob_match("hello", "hello"));
    assert!(glob_match("a", "a"));
}

#[test]
fn literal_mismatch() {
    assert!(!glob_match("hello", "world"));
    // Anchored: prefixes and extensions of the text must NOT match.
    assert!(!glob_match("hello", "hell"));
    assert!(!glob_match("hell", "hello"));
}

#[test]
fn star_at_start() {
    assert!(glob_match("*.rs", "main.rs"));
    assert!(glob_match("*.rs", ".rs")); // `*` may match the empty sequence
    assert!(!glob_match("*.rs", "main.rc"));
}

#[test]
fn star_in_middle() {
    assert!(glob_match("a*c", "abbbc"));
    assert!(glob_match("a*c", "ac"));
    assert!(!glob_match("a*c", "abbb"));
}

#[test]
fn star_at_end() {
    assert!(glob_match("src*", "src/lib.rs"));
    assert!(glob_match("src*", "src"));
    assert!(!glob_match("src*", "sr"));
}

#[test]
fn empty_pattern_and_empty_text() {
    assert!(glob_match("", ""));
    assert!(!glob_match("", "a"));
    assert!(glob_match("*", ""));
}

#[test]
fn question_mark_matches_exactly_one() {
    assert!(glob_match("h?llo", "hello"));
    assert!(glob_match("h?llo", "hallo"));
    assert!(!glob_match("h?llo", "hllo")); // `?` cannot match empty
    assert!(!glob_match("?", ""));
    assert!(!glob_match("??", "a"));
}

#[test]
fn multiple_stars() {
    assert!(glob_match("*a*b*", "xxayybzz"));
    assert!(glob_match("*a*b*", "ab"));
    assert!(glob_match("**", "anything"));
    assert!(!glob_match("*a*b*", "bbb")); // no `a` before a later `b`
}

#[test]
fn character_class_positive() {
    assert!(glob_match("[abc]at", "bat"));
    assert!(glob_match("[abc]at", "cat"));
    assert!(!glob_match("[abc]at", "rat"));
}

#[test]
fn character_class_range() {
    assert!(glob_match("[a-z]oo", "foo"));
    assert!(!glob_match("[a-z]oo", "Foo")); // 'F' is outside a-z
    assert!(glob_match("x[0-9]", "x7"));
    assert!(!glob_match("x[0-9]", "xq"));
}

#[test]
fn negated_class() {
    assert!(glob_match("[!abc]at", "rat"));
    assert!(!glob_match("[!abc]at", "bat"));
    assert!(glob_match("[!0-9]", "z"));
    assert!(!glob_match("[!0-9]", "5"));
}

#[test]
fn escaped_star_is_literal() {
    assert!(glob_match(r"\*", "*"));
    assert!(!glob_match(r"\*", "x")); // escaped `*` is not a wildcard
    assert!(glob_match(r"a\*b", "a*b"));
    assert!(!glob_match(r"a\*b", "aXb"));
}

#[test]
fn escaped_bracket_is_literal() {
    assert!(glob_match(r"\[abc]", "[abc]")); // `]` outside a class is literal
    assert!(!glob_match(r"\[abc]", "a"));
    assert!(!glob_match(r"\[abc]", "b"));
}

#[test]
fn path_style_combination() {
    assert!(glob_match("src/*.rs", "src/lib.rs"));
    assert!(glob_match("src/*.rs", "src/main.rs"));
    assert!(!glob_match("src/*.rs", "src/lib.rs.bak"));
    assert!(glob_match("src/ma?n.rs", "src/main.rs"));
}

#[test]
fn backtracking_across_stars() {
    // The first `*` must settle on "X" (not "XbX") so a `b` remains for the
    // literal `b`, and the second `*` must then absorb "Xb" to reach the `c`.
    assert!(glob_match("a*b*c", "aXbXbc"));
    assert!(!glob_match("a*b*c", "aXbXb"));
}

#[test]
fn naive_greedy_matching_fails_here() {
    // A greedy `*` that swallows the rest of the text and never backtracks
    // gets these wrong: the star must give characters back.
    assert!(glob_match("*aa", "aaa"));
    assert!(glob_match("a*ba", "aba"));
    assert!(!glob_match("*aa", "a"));
}
