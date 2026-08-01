
use super::*;

fn file(lines: &[&str]) -> String {
    lines.join("\n")
}

// ---- compute/2 ----

#[test]
fn identical_contents_yield_no_hunks() {
    let content = file(&["a", "b", "c"]);
    assert_eq!(compute(&content, &content), Vec::new());
}

#[test]
fn one_changed_line_gets_3_context_lines_each_side_with_line_numbers() {
    let before = file(&["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10"]);
    let changed = file(&["l1", "l2", "l3", "l4", "L5", "l6", "l7", "l8", "l9", "l10"]);

    let hunks = compute(&before, &changed);
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    assert_eq!(hunk.old_start, 2);
    assert_eq!(hunk.old_count, 7);
    assert_eq!(hunk.new_start, 2);
    assert_eq!(hunk.new_count, 7);

    assert_eq!(
        hunk.lines,
        vec![
            Line::new(Tag::Context, Some(2), Some(2), "l2"),
            Line::new(Tag::Context, Some(3), Some(3), "l3"),
            Line::new(Tag::Context, Some(4), Some(4), "l4"),
            Line::new(Tag::Removed, Some(5), None, "l5"),
            Line::new(Tag::Added, None, Some(5), "L5"),
            Line::new(Tag::Context, Some(6), Some(6), "l6"),
            Line::new(Tag::Context, Some(7), Some(7), "l7"),
            Line::new(Tag::Context, Some(8), Some(8), "l8"),
        ]
    );
}

#[test]
fn a_change_at_the_top_has_no_leading_context_to_invent() {
    let before = file(&["l1", "l2", "l3", "l4", "l5"]);
    let changed = file(&["L1", "l2", "l3", "l4", "l5"]);

    let hunks = compute(&before, &changed);
    assert_eq!(hunks.len(), 1);
    let lines = &hunks[0].lines;
    assert_eq!(lines[0], Line::new(Tag::Removed, Some(1), None, "l1"));
    assert_eq!(lines[1], Line::new(Tag::Added, None, Some(1), "L1"));
    let context = &lines[2..];
    assert_eq!(context.len(), 3);
}

#[test]
fn far_apart_changes_split_into_separate_hunks() {
    let before_lines: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
    let mut changed_lines = before_lines.clone();
    changed_lines[1] = "CHANGED2".to_string();
    changed_lines[17] = "CHANGED18".to_string();

    let before = before_lines.join("\n");
    let changed = changed_lines.join("\n");
    let hunks = compute(&before, &changed);
    assert_eq!(hunks.len(), 2);
    assert!(
        hunks[0]
            .lines
            .iter()
            .any(|l| *l == Line::new(Tag::Added, None, Some(2), "CHANGED2"))
    );
    assert!(
        hunks[1]
            .lines
            .iter()
            .any(|l| *l == Line::new(Tag::Added, None, Some(18), "CHANGED18"))
    );
}

#[test]
fn nearby_changes_merge_into_one_hunk() {
    let before_lines: Vec<String> = (1..=12).map(|i| format!("line{i}")).collect();
    let mut changed_lines = before_lines.clone();
    changed_lines[3] = "CHANGED4".to_string();
    changed_lines[7] = "CHANGED8".to_string();

    let hunks = compute(&before_lines.join("\n"), &changed_lines.join("\n"));
    assert_eq!(hunks.len(), 1);
    let lines = &hunks[0].lines;
    assert!(
        lines
            .iter()
            .any(|l| *l == Line::new(Tag::Added, None, Some(4), "CHANGED4"))
    );
    assert!(
        lines
            .iter()
            .any(|l| *l == Line::new(Tag::Added, None, Some(8), "CHANGED8"))
    );
}

#[test]
fn pure_insertion_carries_only_added_and_context_lines() {
    let before = file(&["a", "b"]);
    let changed = file(&["a", "new", "b"]);

    let hunks = compute(&before, &changed);
    assert_eq!(hunks.len(), 1);
    let lines = &hunks[0].lines;
    assert!(
        lines
            .iter()
            .any(|l| *l == Line::new(Tag::Added, None, Some(2), "new"))
    );
    assert!(!lines.iter().any(|l| l.tag == Tag::Removed));
}

// ---- all_added/1 ----

#[test]
fn a_created_file_is_one_all_added_hunk_no_phantom_removed_line() {
    let hunks = all_added("a\nb\n");
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    assert_eq!(hunk.old_count, 0);
    assert_eq!(hunk.new_start, 1);
    assert_eq!(hunk.new_count, 2);
    assert_eq!(
        hunk.lines,
        vec![
            Line::new(Tag::Added, None, Some(1), "a"),
            Line::new(Tag::Added, None, Some(2), "b"),
        ]
    );
}

// ---- stats/1 ----

#[test]
fn counts_every_added_and_removed_line_across_hunks() {
    let before_lines: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
    let mut changed_lines = before_lines.clone();
    changed_lines[1] = "CHANGED2".to_string();
    changed_lines.remove(17);

    let hunks = compute(&before_lines.join("\n"), &changed_lines.join("\n"));
    let stats = stats(&hunks);
    assert_eq!(
        stats,
        Stats {
            added: 1,
            removed: 2
        }
    );
}

// ---- to_unified/2 ----

#[test]
fn renders_headers_and_prefixed_lines() {
    let before = file(&["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10"]);
    let changed = file(&["l1", "l2", "l3", "l4", "L5", "l6", "l7", "l8", "l9", "l10"]);

    let unified = to_unified(&compute(&before, &changed), 100);

    assert_eq!(
        unified,
        "@@ -2,7 +2,7 @@\n l2\n l3\n l4\n-l5\n+L5\n l6\n l7\n l8"
    );
}

#[test]
fn caps_at_max_lines_with_an_elision_note() {
    let before: String = (1..=30)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let changed: String = (1..=30)
        .map(|i| format!("LINE{i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let unified = to_unified(&compute(&before, &changed), 5);

    assert_eq!(unified.split('\n').count(), 6);
    assert!(unified.contains("more diff lines)"));
}
