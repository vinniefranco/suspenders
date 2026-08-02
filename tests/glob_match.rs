use super::*;

#[test]
fn a_slash_free_glob_matches_the_basename_at_any_depth() {
    let re = compile("*.rs").unwrap();
    assert!(re.is_match("a.rs"));
    assert!(re.is_match("src/a.rs"));
    assert!(re.is_match("src/nested/a.rs"));
    assert!(!re.is_match("a.txt"));
}

#[test]
fn a_glob_with_a_slash_is_anchored_to_the_root() {
    let re = compile("src/*.rs").unwrap();
    assert!(re.is_match("src/a.rs"));
    assert!(!re.is_match("src/inner/a.rs"));
    assert!(!re.is_match("other/a.rs"));
}

#[test]
fn double_star_crosses_directories() {
    let re = compile("src/**/*.rs").unwrap();
    assert!(re.is_match("src/a.rs"));
    assert!(re.is_match("src/net/client.rs"));
}

#[test]
fn question_mark_matches_one_non_slash_character() {
    let re = compile("a?.txt").unwrap();
    assert!(re.is_match("a1.txt"));
    assert!(!re.is_match("a10.txt"));
}

#[test]
fn a_character_class_matches_the_enumerated_characters() {
    let re = compile("[ab].txt").unwrap();
    assert!(re.is_match("a.txt"));
    assert!(!re.is_match("c.txt"));
}

#[test]
fn matching_is_case_insensitive() {
    let re = compile("*.rs").unwrap();
    assert!(re.is_match("Main.RS"));
}

#[test]
fn an_unclosed_class_is_an_error_not_a_panic() {
    assert!(compile("[").is_err());
}
