use super::*;

// `line_col` and `cursor_at` are exact inverses on every clamped cursor.
#[test]
fn line_col_and_cursor_at_round_trip() {
    for value in ["", "abcd", "ab\ncd\nef", "a\n\nb", "x\n", "\n\n\n"] {
        let chars = value.chars().count();
        for cursor in 0..=chars {
            let (line, col) = line_col(value, cursor);
            assert_eq!(
                cursor_at(value, line, col),
                cursor,
                "round trip failed for value={value:?} cursor={cursor}"
            );
        }
    }
}

#[test]
fn a_cursor_on_a_newline_is_the_end_of_the_line_before() {
    // "ab\ncd": index 2 sits on the '\n'; it is column 2 of line 0.
    assert_eq!(line_col("ab\ncd", 2), (0, 2));
    assert_eq!(line_col("ab\ncd", 3), (1, 0));
}

#[test]
fn line_lengths_never_empty_and_counts_blank_lines() {
    assert_eq!(line_lengths(""), vec![0]);
    assert_eq!(line_lengths("a\n\nb"), vec![1, 0, 1]);
    assert_eq!(line_lengths("x\n"), vec![1, 0]);
}

#[test]
fn byte_of_translates_char_index_past_multibyte() {
    // 'é' is two bytes; char index 2 is the byte after "hé".
    assert_eq!(byte_of("héllo", 0), 0);
    assert_eq!(byte_of("héllo", 2), 3);
    assert_eq!(byte_of("héllo", 99), "héllo".len());
}
