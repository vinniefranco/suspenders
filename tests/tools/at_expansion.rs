use super::*;
use crate::content::{ContentBlock, Modalities};
use tempfile::TempDir;

// ---- parse_at_commands (PURE segmenter) --------------------------------------

#[test]
fn parses_plain_text_as_one_text_part() {
    assert_eq!(
        parse_at_commands("just some text"),
        vec![Part::Text("just some text".into())]
    );
}

#[test]
fn parses_a_lone_at_path() {
    assert_eq!(
        parse_at_commands("@src/main.rs"),
        vec![Part::AtPath("@src/main.rs".into())]
    );
}

#[test]
fn parses_text_then_at_path_then_text() {
    assert_eq!(
        parse_at_commands("look at @shot.png please"),
        vec![
            Part::Text("look at ".into()),
            Part::AtPath("@shot.png".into()),
            Part::Text(" please".into()),
        ]
    );
}

#[test]
fn parses_multiple_at_paths() {
    assert_eq!(
        parse_at_commands("@a.txt and @b.txt"),
        vec![
            Part::AtPath("@a.txt".into()),
            Part::Text(" and ".into()),
            Part::AtPath("@b.txt".into()),
        ]
    );
}

#[test]
fn a_lone_at_is_an_at_path_part_treated_as_text() {
    // qwen: a bare '@' parses as an atPath part whose content is "@"; it is later
    // treated as text (put back verbatim).
    assert_eq!(parse_at_commands("@"), vec![Part::AtPath("@".into())]);
}

#[test]
fn an_escaped_at_is_text_not_a_mention() {
    // A backslash-escaped '@' is not an unescaped '@', so the whole thing is text.
    let parts = parse_at_commands("email\\@example.com");
    assert!(parts.iter().all(|p| matches!(p, Part::Text(_))));
}

#[test]
fn a_path_terminates_at_punctuation() {
    assert_eq!(
        parse_at_commands("see @file.rs, then stop"),
        vec![
            Part::Text("see ".into()),
            Part::AtPath("@file.rs".into()),
            Part::Text(", then stop".into()),
        ]
    );
}

#[test]
fn a_sentence_ending_dot_is_not_part_of_the_path() {
    // "file.txt." at a sentence end: the trailing dot (followed by space) drops.
    assert_eq!(
        parse_at_commands("open @file.txt. done"),
        vec![
            Part::Text("open ".into()),
            Part::AtPath("@file.txt".into()),
            Part::Text(". done".into()),
        ]
    );
}

#[test]
fn an_escaped_space_stays_in_the_path() {
    // "@my\ file.txt" is one path "my file.txt" (unescapePath strips the escape).
    assert_eq!(
        parse_at_commands("@my\\ file.txt"),
        vec![Part::AtPath("@my file.txt".into())]
    );
}

#[test]
fn a_second_at_does_not_terminate_a_path() {
    // '@' is not a path terminator (qwen breaks only on whitespace/punctuation/.),
    // so "@a@b" is ONE mention "@a@b", not two.
    assert_eq!(parse_at_commands("@a@b"), vec![Part::AtPath("@a@b".into())]);
}

#[test]
fn space_separated_mentions_drop_the_whitespace_text_between() {
    // The " " between two mentions is a whitespace-only text part, which qwen
    // filters out, leaving just the two mentions.
    assert_eq!(
        parse_at_commands("@a.txt @b.txt"),
        vec![Part::AtPath("@a.txt".into()), Part::AtPath("@b.txt".into())]
    );
}

// ---- has_at_mention (fast-path gate) -----------------------------------------

#[test]
fn has_at_mention_true_for_a_mention() {
    assert!(has_at_mention("look at @shot.png"));
}

#[test]
fn has_at_mention_true_for_a_lone_at() {
    assert!(has_at_mention("just @"));
}

#[test]
fn has_at_mention_false_for_plain_text() {
    assert!(!has_at_mention("no mention here"));
}

#[test]
fn has_at_mention_false_for_an_escaped_at() {
    assert!(!has_at_mention("email\\@example.com"));
}

// ---- expand: resolution + confinement (IMPURE) -------------------------------

async fn expand_in(query: &str, root: &std::path::Path) -> (UserPrompt, ReadDisplay) {
    expand(
        query,
        root,
        None,
        None,
        Modalities {
            image: true,
            pdf: true,
        },
    )
    .await
}

/// [`expand`] with the P5 global clipboard temp dir supplied, for exercising the
/// confinement exception (a `@<abs-temp-path>` under `temp_dir` resolves even
/// though it is outside `root`).
async fn expand_in_with_temp(
    query: &str,
    root: &std::path::Path,
    temp_dir: &std::path::Path,
) -> (UserPrompt, ReadDisplay) {
    expand(
        query,
        root,
        None,
        Some(temp_dir),
        Modalities {
            image: true,
            pdf: true,
        },
    )
    .await
}

// A 1x1 transparent PNG so an image read produces a real base64 payload.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

#[tokio::test]
async fn no_mention_yields_plain_text_prompt() {
    let tmp = TempDir::new().unwrap();
    let (prompt, display) = expand_in("no mention here", tmp.path()).await;
    assert!(prompt.is_plain_text());
    assert_eq!(prompt.text(), "no mention here");
    assert!(display.is_empty());
}

#[tokio::test]
async fn end_to_end_look_at_shot_png_yields_text_plus_image() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("shot.png"), PNG_1X1).unwrap();

    let (prompt, display) = expand_in("look at @shot.png", tmp.path()).await;
    let blocks = prompt.blocks();

    // Residual text block FIRST, then the image block (qwen's ordering).
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "look at @shot.png"));
    assert!(matches!(
        &blocks[1],
        ContentBlock::Image { mime, data } if mime == "image/png" && !data.is_empty()
    ));
    assert_eq!(blocks.len(), 2);

    // The read display names the file.
    assert_eq!(display.read, vec![("shot.png".to_string(), false)]);
}

#[tokio::test]
async fn a_text_file_inlines_as_a_text_block() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "hello world\nsecond line").unwrap();

    let (prompt, _) = expand_in("read @notes.txt", tmp.path()).await;
    let blocks = prompt.blocks();
    assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "read @notes.txt"));
    assert!(
        matches!(&blocks[1], ContentBlock::Text { text } if text.contains("hello world") && text.contains("second line"))
    );
}

#[tokio::test]
async fn a_relative_path_climbing_out_of_the_root_is_skipped() {
    let tmp = TempDir::new().unwrap();
    // BUG 1: a RELATIVE `@path` is still root-confined; one that climbs out with
    // `..` is refused by the confinement predicate.
    let (prompt, display) = expand_in("look @../../../etc/hostname", tmp.path()).await;
    assert!(prompt.is_plain_text());
    assert!(
        display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::OutsideWorkspace))
    );
}

#[tokio::test]
async fn a_user_absolute_path_outside_the_root_is_honored() {
    // BUG 1 (the reported symptom): an At Mention is USER input, not a model tool
    // call, so a user's OWN absolute file OUTSIDE the Project Root must be honored,
    // not skipped as "outside project root". Build a real file in a SIBLING
    // tempdir (outside `root`) and mention it by absolute path.
    let root = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let pic = elsewhere.path().join("background-meme1.txt");
    std::fs::write(&pic, "meme contents").unwrap();

    let query = format!("look at @{}", pic.display());
    let (prompt, display) = expand_in(&query, root.path()).await;

    // It resolved and was READ, not skipped: a text block plus the file content.
    assert!(!prompt.is_plain_text(), "absolute user path must resolve");
    let joined: String = prompt
        .blocks()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(joined.contains("meme contents"), "the file must be inlined");
    assert!(
        !display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::OutsideWorkspace)),
        "a user's own absolute file must NOT be skipped as outside-workspace"
    );
}

#[tokio::test]
async fn a_user_absolute_image_outside_the_root_is_read_as_an_image_block() {
    // The reported symptom concretely: `@/abs/path/background-meme1.jpg`. An
    // absolute image outside the root is honored and read as an Image block.
    let root = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let img = elsewhere.path().join("background-meme1.png");
    std::fs::write(&img, PNG_1X1).unwrap();

    let query = format!("look at @{}", img.display());
    let (prompt, display) = expand_in(&query, root.path()).await;

    assert!(
        prompt
            .blocks()
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "an absolute user image outside the root should resolve to an Image block"
    );
    assert!(display.skipped.is_empty(), "nothing should be skipped");
}

#[tokio::test]
async fn a_nonexistent_absolute_path_is_still_skipped_as_not_found() {
    // BUG 1 keeps the not-found skip: an absolute path that does not exist is
    // honored by `confine` but caught by the downstream `stat` and reported
    // NotFound (never read, never OutsideWorkspace).
    let root = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let missing = elsewhere.path().join("does-not-exist.png");

    let query = format!("look @{}", missing.display());
    let (prompt, display) = expand_in(&query, root.path()).await;

    assert!(prompt.is_plain_text());
    assert!(
        display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::NotFound)),
        "a non-existent absolute path is skipped as not-found, not honored"
    );
    assert!(
        !display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::OutsideWorkspace)),
    );
}

#[tokio::test]
async fn a_temp_dir_path_resolves_via_the_confinement_exception() {
    // ADR-0068 P5: a staged clipboard image lives OUTSIDE the Project Root, under
    // the global clipboard temp dir. Its ABSOLUTE `@path` must resolve via the
    // temp-dir confinement exception even though it escapes the root.
    let root = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();
    let img = temp.path().join("clipboard-123-abc.png");
    std::fs::write(&img, PNG_1X1).unwrap();

    let query = format!("look @{}", img.display());
    let (prompt, display) = expand_in_with_temp(&query, root.path(), temp.path()).await;

    // The temp-dir image resolved: a text block plus the image block, no skip.
    assert!(!prompt.is_plain_text());
    assert!(
        prompt
            .blocks()
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "the temp-dir clipboard image should resolve to an Image block"
    );
    assert!(
        !display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::OutsideWorkspace)),
        "a temp-dir path must NOT be skipped as outside-workspace"
    );
}

#[tokio::test]
async fn a_relative_outside_root_path_is_still_skipped_when_a_temp_dir_is_set() {
    // The exception is NARROW: with a temp dir configured, a RELATIVE path that
    // climbs out of the root and is NOT under the temp dir is still skipped.
    // (Absolute user paths are honored regardless per BUG 1; only the relative
    // completion form is confined, so the narrowness is exercised relatively.)
    let root = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();

    let (prompt, display) =
        expand_in_with_temp("look @../../../etc/hostname", root.path(), temp.path()).await;

    assert!(prompt.is_plain_text());
    assert!(
        display
            .skipped
            .iter()
            .any(|(_, s)| matches!(s, Skip::OutsideWorkspace)),
        "a relative outside-root path must still be skipped"
    );
}

#[tokio::test]
async fn a_staged_clipboard_file_at_one_level_passes_confinement() {
    // BUG 3 + confinement: a staged clipboard image lands at exactly
    // `<global>/clipboard/clipboard-*.png` (one `clipboard/` level). At Expansion's
    // confinement `temp_dir` is that same landing dir (`<global>/clipboard`), so the
    // staged file's absolute `@path` must resolve. Mirror the real landing layout.
    let root = TempDir::new().unwrap();
    let global = TempDir::new().unwrap();
    let landing = global.path().join("clipboard");
    std::fs::create_dir_all(&landing).unwrap();
    let staged = landing.join("clipboard-123-abc.png");
    std::fs::write(&staged, PNG_1X1).unwrap();

    // The confinement dir the adapter passes is the landing dir itself.
    let query = format!("@{}", staged.display());
    let (prompt, display) = expand_in_with_temp(&query, root.path(), &landing).await;

    assert!(
        prompt
            .blocks()
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "the staged clipboard image must resolve to an Image block"
    );
    assert!(
        display.skipped.is_empty(),
        "the staged file must not be skipped"
    );
}

#[tokio::test]
async fn an_absolute_path_near_the_temp_dir_is_honored_as_user_input() {
    // BUG 1 supersedes the old temp-dir sibling-prefix REFUSAL: an At Mention is
    // USER input, so ANY absolute path the user types is honored regardless of the
    // Project Root or the temp-dir boundary (they could paste its contents
    // themselves). A `<temp_dir>-evil` sibling that once had to be refused is now
    // just another user-named absolute file - honored, read, never skipped.
    let root = TempDir::new().unwrap();
    let temp = TempDir::new().unwrap();
    let sibling = format!("{}-evil", temp.path().display());
    std::fs::create_dir_all(&sibling).unwrap();
    let img = std::path::Path::new(&sibling).join("clipboard-x.png");
    std::fs::write(&img, PNG_1X1).unwrap();

    let query = format!("look @{}", img.display());
    let (prompt, display) = expand_in_with_temp(&query, root.path(), temp.path()).await;

    assert!(
        prompt
            .blocks()
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "an absolute user path is honored regardless of the temp-dir boundary"
    );
    assert!(display.skipped.is_empty(), "nothing should be skipped");
}

#[tokio::test]
async fn a_not_found_path_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let (prompt, display) = expand_in("read @missing.txt", tmp.path()).await;
    assert!(prompt.is_plain_text());
    assert_eq!(prompt.text(), "read @missing.txt");
    assert!(
        display
            .skipped
            .iter()
            .any(|(label, s)| label == "missing.txt" && matches!(s, Skip::NotFound))
    );
}

#[tokio::test]
async fn a_gitignored_path_is_skipped_and_reported() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "shh").unwrap();

    let (prompt, display) = expand_in("read @secret.txt", tmp.path()).await;
    assert!(prompt.is_plain_text());
    assert!(
        display.skipped.iter().any(
            |(label, s)| label == "secret.txt" && matches!(s, Skip::Ignored(IgnoreReason::Git))
        )
    );
}

#[tokio::test]
async fn a_directory_mention_glob_expands_its_files() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("dir")).unwrap();
    std::fs::write(tmp.path().join("dir/a.txt"), "aaa").unwrap();
    std::fs::write(tmp.path().join("dir/b.txt"), "bbb").unwrap();

    let (prompt, display) = expand_in("scan @dir", tmp.path()).await;
    let blocks = prompt.blocks();
    // Text block first, then both files' content (walk order: a before b).
    let joined: String = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(joined.contains("aaa"));
    assert!(joined.contains("bbb"));
    // The read display marks it a directory.
    assert_eq!(display.read, vec![("dir".to_string(), true)]);
}

// ---- residual query rebuild (PURE, exercised via expand) ---------------------

#[tokio::test]
async fn rebuild_reemits_resolved_mention_with_spacing() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
    // No space around the mention in the draft; the rebuild inserts qwen's spacing.
    let (prompt, _) = expand_in("see@a.txt", tmp.path()).await;
    if let ContentBlock::Text { text } = &prompt.blocks()[0] {
        assert_eq!(text, "see @a.txt");
    } else {
        panic!("first block should be text");
    }
}

#[tokio::test]
async fn rebuild_puts_an_unresolved_mention_back_verbatim() {
    let tmp = TempDir::new().unwrap();
    let (prompt, _) = expand_in("look @missing.txt here", tmp.path()).await;
    assert_eq!(prompt.text(), "look @missing.txt here");
}

// ---- rewrite_paste (bracketed-paste path detection, ADR-0068 P4) -------------

// The real `isValidPath` predicate qwen injects: exists AND is a regular file.
fn is_file(p: &std::path::Path) -> bool {
    p.is_file()
}

#[test]
fn paste_of_an_existing_file_path_rewrites_to_an_at_mention() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("shot.png"), b"x").unwrap();
    let abs = tmp.path().join("shot.png");
    // A dragged image drops its absolute path; inside the root it comes back
    // root-relative (the form At Expansion resolves), with qwen's trailing space.
    let out = rewrite_paste(abs.to_str().unwrap(), tmp.path(), is_file);
    assert_eq!(out, Some("@shot.png ".to_string()));
}

#[test]
fn paste_of_an_absolute_path_outside_the_root_stays_verbatim() {
    let tmp = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    std::fs::write(other.path().join("outside.txt"), b"x").unwrap();
    let abs = other.path().join("outside.txt");
    // Outside the Project Root: qwen inserts the dropped path verbatim (At
    // Expansion later skips an out-of-root mention); still an `@<path> `.
    let out = rewrite_paste(abs.to_str().unwrap(), tmp.path(), is_file).unwrap();
    assert!(out.starts_with('@') && out.ends_with(' '));
    assert!(out.contains("outside.txt"));
}

#[test]
fn paste_of_a_quoted_dragged_path_is_unquoted() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"x").unwrap();
    let abs = tmp.path().join("a.txt");
    // Terminals often quote a drag/drop; the quotes are stripped before the
    // existence check.
    let quoted = format!("'{}'", abs.to_str().unwrap());
    let out = rewrite_paste(&quoted, tmp.path(), is_file);
    assert_eq!(out, Some("@a.txt ".to_string()));
}

#[test]
fn paste_of_a_space_containing_path_is_re_escaped() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("my shot.png"), b"x").unwrap();
    let abs = tmp.path().join("my shot.png");
    // A drag/drop escapes the space (`my\ shot.png`); after unescape+existence the
    // mention re-escapes it so the composer's AT scan round-trips.
    let escaped = abs.to_str().unwrap().replace(' ', "\\ ");
    let out = rewrite_paste(&escaped, tmp.path(), is_file);
    assert_eq!(out, Some("@my\\ shot.png ".to_string()));
}

#[test]
fn paste_of_a_nonexistent_path_inserts_literally() {
    let tmp = TempDir::new().unwrap();
    let abs = tmp.path().join("nope.png");
    // Not on disk: qwen's `isValidPath` is false, so it is literal text (None).
    assert_eq!(
        rewrite_paste(abs.to_str().unwrap(), tmp.path(), is_file),
        None
    );
}

#[test]
fn paste_of_a_directory_inserts_literally() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("subdir")).unwrap();
    let abs = tmp.path().join("subdir");
    // A directory exists but is not a FILE: qwen's `isValidPath` is false.
    assert_eq!(
        rewrite_paste(abs.to_str().unwrap(), tmp.path(), is_file),
        None
    );
}

#[test]
fn paste_of_ordinary_multiline_text_inserts_literally() {
    let tmp = TempDir::new().unwrap();
    // Two non-blank lines: not a single dropped path, so literal (None) even if a
    // line happened to name a real file.
    std::fs::write(tmp.path().join("a.txt"), b"x").unwrap();
    let text = format!("some notes\n{}", tmp.path().join("a.txt").display());
    assert_eq!(rewrite_paste(&text, tmp.path(), is_file), None);
}

#[test]
fn paste_of_ordinary_prose_inserts_literally() {
    let tmp = TempDir::new().unwrap();
    // A sentence is not a path: `isValidPath` is false → None (literal).
    assert_eq!(
        rewrite_paste("just some pasted prose here", tmp.path(), is_file),
        None
    );
}

#[test]
fn paste_shorter_than_the_drag_drop_floor_inserts_literally() {
    let tmp = TempDir::new().unwrap();
    // qwen never infers a < 3-char paste as a drop, even if it exists.
    std::fs::write(tmp.path().join("a"), b"x").unwrap();
    let abs = tmp.path().join("a");
    // The absolute path is long, so force the short case with a bare short name
    // that the predicate rejects on length before any fs touch.
    assert_eq!(rewrite_paste("ab", tmp.path(), is_file), None);
    // Sanity: the long absolute form of the same file DOES rewrite.
    assert!(rewrite_paste(abs.to_str().unwrap(), tmp.path(), is_file).is_some());
}
