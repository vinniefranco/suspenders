// Oracle tests for the run-length + dictionary decoder.
// DO NOT MODIFY THIS FILE. The implementation in src/lib.rs must make
// every test here pass.

use rle_dict_decoder::{decode, DecodeError};

#[test]
fn t01_empty_input_gives_empty_output() {
    assert_eq!(decode(""), Ok(String::new()));
}

#[test]
fn t02_literals_pass_through() {
    assert_eq!(decode("hello, world"), Ok("hello, world".to_string()));
    assert_eq!(decode("A-Z_ = ok?"), Ok("A-Z_ = ok?".to_string()));
}

#[test]
fn t03_simple_repeat() {
    assert_eq!(decode("3(ab)"), Ok("ababab".to_string()));
    assert_eq!(decode("x1(y)z"), Ok("xyz".to_string()));
}

#[test]
fn t04_multi_digit_repeat() {
    assert_eq!(decode("12(a)"), Ok("aaaaaaaaaaaa".to_string()));
}

#[test]
fn t05_nested_repeat() {
    assert_eq!(decode("2(a3(b))"), Ok("abbbabbb".to_string()));
    assert_eq!(decode("2(2(2(x)))"), Ok("xxxxxxxx".to_string()));
}

#[test]
fn t06_digit_not_followed_by_paren_is_literal() {
    assert_eq!(decode("a2b"), Ok("a2b".to_string()));
    assert_eq!(decode("12x"), Ok("12x".to_string()));
    // The escaped paren does not open a group, so the 2 is a literal.
    assert_eq!(decode("2\\(a\\)"), Ok("2(a)".to_string()));
}

#[test]
fn t07_definition_and_substitution() {
    assert_eq!(decode("!g=abc;x&g;y&g;"), Ok("xabcyabc".to_string()));
}

#[test]
fn t08_definition_uses_earlier_definition() {
    assert_eq!(decode("!a=xy;!b=2(&a;)z;&b;"), Ok("xyxyz".to_string()));
}

#[test]
fn t09_definition_inside_repeat() {
    // Bindings are global and survive the group; a 1(...) group runs once.
    assert_eq!(decode("1(!k=hi;&k;)&k;"), Ok("hihi".to_string()));
    // A repeated group body executes per repetition, so the definition
    // runs twice: DuplicateKey.
    assert_eq!(decode("2(!d=a;)"), Err(DecodeError::DuplicateKey));
}

#[test]
fn t10_redefinition_is_duplicate_key() {
    assert_eq!(decode("!k=a;!k=b;"), Err(DecodeError::DuplicateKey));
}

#[test]
fn t11_unknown_key() {
    assert_eq!(decode("&z;"), Err(DecodeError::UnknownKey));
    // Keys bound later are not visible earlier.
    assert_eq!(decode("!a=&b;;!b=x;"), Err(DecodeError::UnknownKey));
}

#[test]
fn t12_bad_repeat_count() {
    assert_eq!(decode("0(x)"), Err(DecodeError::BadRepeatCount));
    assert_eq!(decode("00(x)"), Err(DecodeError::BadRepeatCount));
    // '(' with no digits immediately before it.
    assert_eq!(decode("(ab)"), Err(DecodeError::BadRepeatCount));
}

#[test]
fn t13_unbalanced_parens() {
    assert_eq!(decode("2(ab"), Err(DecodeError::UnbalancedParens));
    assert_eq!(decode("ab)"), Err(DecodeError::UnbalancedParens));
    assert_eq!(decode("3(2(a)"), Err(DecodeError::UnbalancedParens));
}

#[test]
fn t14_unterminated_definition() {
    assert_eq!(decode("!k=abc"), Err(DecodeError::UnterminatedDefinition));
    assert_eq!(decode("!k"), Err(DecodeError::UnterminatedDefinition));
    // Key must be a single lowercase ASCII letter, followed by '='.
    assert_eq!(decode("!5=a;"), Err(DecodeError::UnterminatedDefinition));
    assert_eq!(decode("!kx=a;"), Err(DecodeError::UnterminatedDefinition));
}

#[test]
fn t15_unterminated_substitution() {
    assert_eq!(decode("&k"), Err(DecodeError::UnterminatedSubstitution));
    assert_eq!(decode("&;"), Err(DecodeError::UnterminatedSubstitution));
    assert_eq!(decode("&kk;"), Err(DecodeError::UnterminatedSubstitution));
}

#[test]
fn t16_escapes_produce_literals() {
    assert_eq!(decode("\\(\\)\\&\\!\\;\\\\"), Ok("()&!;\\".to_string()));
    // Escaped ')' inside a group is a literal, not the group close.
    assert_eq!(decode("3(\\))"), Ok(")))".to_string()));
}

#[test]
fn t17_bad_escape() {
    assert_eq!(decode("\\n"), Err(DecodeError::BadEscape));
    assert_eq!(decode("\\a"), Err(DecodeError::BadEscape));
    // Trailing backslash at end of input.
    assert_eq!(decode("\\"), Err(DecodeError::BadEscape));
}

#[test]
fn t18_integration_combined() {
    assert_eq!(
        decode("!s=2(ab);3(x&s;)\\!done\\;"),
        Ok("xababxababxabab!done;".to_string())
    );
}
