use super::*;

// ---- edit_strings (qwen normalizeEditStrings) ----

#[test]
fn a_literal_match_returns_the_strings_unchanged() {
    let n = edit_strings("foo bar baz", "bar", "qux");
    assert_eq!(n.old_string, "bar");
    assert_eq!(n.new_string, "qux");
}

#[test]
fn an_empty_old_string_is_the_new_file_sentinel_and_passes_through() {
    let n = edit_strings("existing content", "", "new body");
    assert_eq!(n.old_string, "");
    assert_eq!(n.new_string, "new body");
}

#[test]
fn no_match_at_all_returns_the_strings_unchanged() {
    let n = edit_strings("actual content", "imaginary", "x");
    assert_eq!(n.old_string, "imaginary");
    assert_eq!(n.new_string, "x");
}

#[test]
fn a_curly_quote_canonicalizes_to_the_on_disk_straight_quote_slice() {
    // U+2019 RIGHT SINGLE QUOTATION MARK in old_string; the file holds a
    // straight apostrophe. The character-equivalence pass substitutes the
    // CANONICAL on-disk slice so the caller replaces real bytes.
    let n = edit_strings("it's here", "it\u{2019}s here", "it is here");
    assert_eq!(n.old_string, "it's here");
    assert_eq!(n.new_string, "it is here");
}

#[test]
fn an_em_dash_canonicalizes_to_the_on_disk_hyphen_slice() {
    // U+2014 EM DASH maps to ASCII hyphen-minus.
    let n = edit_strings("a - b", "a \u{2014} b", "a plus b");
    assert_eq!(n.old_string, "a - b");
}

#[test]
fn an_exotic_space_canonicalizes_to_the_on_disk_normal_space_slice() {
    // U+00A0 NO-BREAK SPACE maps to a normal space.
    let n = edit_strings("a b", "a\u{00A0}b", "ab");
    assert_eq!(n.old_string, "a b");
}

#[test]
fn the_character_pass_cuts_the_original_haystack_not_the_normalized_one() {
    // The HAYSTACK carries the exotic characters; the needle is plain. The
    // returned slice is the original (curly-quoted) on-disk bytes.
    let n = edit_strings("say \u{201C}hi\u{201D} now", "say \"hi\" now", "said");
    assert_eq!(n.old_string, "say \u{201C}hi\u{201D} now");
}

#[test]
fn line_matching_tolerates_trailing_whitespace_on_disk() {
    // The file line has trailing spaces the needle did not carry; the trimEnd
    // line pass matches and returns the canonical trailing-whitespace-bearing
    // slice.
    let n = edit_strings("x = 1   \ny = 2\n", "x = 1\ny = 2", "x = 3\ny = 2");
    assert_eq!(n.old_string, "x = 1   \ny = 2");
    assert_eq!(n.new_string, "x = 3\ny = 2");
}

#[test]
fn a_full_match_including_the_trailing_newline_keeps_it_in_the_slice() {
    // The needle ends with '\n' and its empty final split line matches the
    // file's own final empty line: the slice includes the newline after its
    // last content line, and new_string is untouched.
    let n = edit_strings("alpha  \nbeta\n", "alpha\nbeta\n", "gamma\n");
    assert_eq!(n.old_string, "alpha  \nbeta\n");
    assert_eq!(n.new_string, "gamma\n");
}

#[test]
fn dropping_the_needles_trailing_empty_line_trims_new_strings_newline() {
    // The needle's trailing '\n' (an empty final split line) finds no match;
    // the retry without it matches, and new_string loses its own trailing
    // newline to compensate (removedTrailingFinalEmptyLine).
    let n = edit_strings("only line", "only line\n", "new line\n");
    assert_eq!(n.old_string, "only line");
    assert_eq!(n.new_string, "new line");
}

// ---- augment_old_string_for_deletion (qwen maybeAugmentOldStringForDeletion) ----

#[test]
fn a_pure_deletion_grows_old_string_by_the_files_trailing_newline() {
    assert_eq!(
        augment_old_string_for_deletion("keep\nDROP\nkeep\n", "DROP", ""),
        "DROP\n"
    );
}

#[test]
fn a_replacement_is_never_augmented() {
    assert_eq!(
        augment_old_string_for_deletion("keep\nDROP\nkeep\n", "DROP", "SWAP"),
        "DROP"
    );
}

#[test]
fn an_old_string_already_ending_in_newline_is_not_augmented() {
    assert_eq!(
        augment_old_string_for_deletion("keep\nDROP\nkeep\n", "DROP\n", ""),
        "DROP\n"
    );
}

#[test]
fn no_augmentation_when_the_file_lacks_the_newline_bearing_candidate() {
    assert_eq!(
        augment_old_string_for_deletion("keep DROP keep", "DROP", ""),
        "DROP"
    );
}

#[test]
fn an_empty_old_string_is_not_augmented() {
    assert_eq!(augment_old_string_for_deletion("body\n", "", ""), "");
}

// ---- count_occurrences (qwen countOccurrences) ----

#[test]
fn counts_non_overlapping_occurrences() {
    assert_eq!(count_occurrences("foo bar foo", "foo"), 2);
    assert_eq!(count_occurrences("foo bar foo", "bar"), 1);
    assert_eq!(count_occurrences("foo bar foo", "missing"), 0);
}

#[test]
fn an_empty_old_string_counts_zero() {
    assert_eq!(count_occurrences("anything", ""), 0);
}

#[test]
fn counting_is_exact_not_normalized() {
    // The counter runs AFTER canonicalization: a curly-quoted needle does not
    // count against straight-quoted content.
    assert_eq!(count_occurrences("it's here", "it\u{2019}s here"), 0);
}
