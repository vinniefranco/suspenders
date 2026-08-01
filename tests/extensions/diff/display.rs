
use super::*;
use crate::extensions::diff::hunks;

// ---- title/2 ----

#[test]
fn title_existing_file() {
    let diff = Diff {
        path: "lib/x.ex".to_string(),
        hunks: vec![],
        added: 3,
        removed: 1,
        created: false,
    };
    assert_eq!(title("edit", &diff), "edit lib/x.ex (+3 -1)");
}

#[test]
fn title_new_file() {
    let diff = Diff {
        path: "new.ex".to_string(),
        hunks: vec![],
        added: 5,
        removed: 0,
        created: true,
    };
    assert_eq!(
        title("write_file", &diff),
        "write_file new.ex (new file, +5)"
    );
}

// ---- lang/1 ----

#[test]
fn lang_is_the_file_extension() {
    assert_eq!(lang("src/main.rs").as_deref(), Some("rs"));
    assert_eq!(lang("app/foo.js").as_deref(), Some("js"));
    assert_eq!(lang("data.json").as_deref(), Some("json"));
}

#[test]
fn lang_is_none_without_an_extension() {
    assert_eq!(lang("Makefile"), None);
    assert_eq!(lang(""), None);
}

// ---- hunks/2 ----

#[test]
fn existing_file_carries_a_hunk_header_and_raw_marker_free_lines() {
    let computed = hunks::compute("a\nb\nc", "a\nB\nc");
    let diff = Diff {
        path: String::new(),
        hunks: computed,
        added: 1,
        removed: 1,
        created: false,
    };
    let (hunks, elided) = hunks(&diff, DISPLAY_LINES);
    assert_eq!(elided, 0);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].header.as_deref(), Some("@@ -1,3 +1,3 @@"));
    // The lines are RAW code with no +/-/context marker - the adapter adds it.
    assert!(
        hunks[0]
            .lines
            .contains(&DiffLine::new(DiffSide::Context, "a"))
    );
    assert!(
        hunks[0]
            .lines
            .contains(&DiffLine::new(DiffSide::Removed, "b"))
    );
    assert!(
        hunks[0]
            .lines
            .contains(&DiffLine::new(DiffSide::Added, "B"))
    );
}

#[test]
fn created_file_skips_the_hunk_header() {
    let computed = hunks::all_added("a\n");
    let diff = Diff {
        path: String::new(),
        hunks: computed,
        added: 1,
        removed: 0,
        created: true,
    };
    let (hunks, elided) = hunks(&diff, DISPLAY_LINES);
    assert_eq!(elided, 0);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].header, None);
    assert_eq!(hunks[0].lines, vec![DiffLine::new(DiffSide::Added, "a")]);
}

#[test]
fn long_diffs_cap_and_report_the_elided_count() {
    let content = (1..=100)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let computed = hunks::all_added(&content);
    let diff = Diff {
        path: String::new(),
        hunks: computed,
        added: 100,
        removed: 0,
        created: true,
    };
    let (hunks, elided) = hunks(&diff, DISPLAY_LINES);
    let shown: usize = hunks.iter().map(|h| h.lines.len()).sum();
    assert_eq!(shown, DISPLAY_LINES);
    assert_eq!(elided, 40);
}
