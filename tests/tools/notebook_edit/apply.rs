use super::*;

// A minimal nbformat-4.5 notebook fixture with two cells carrying stable ids
// and array-form source, so inserts generate ids and preserve the array
// style.
const FIXTURE: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "intro",
   "metadata": {},
   "source": [
    "# Title\n"
   ]
  },
  {
   "cell_type": "code",
   "id": "run",
   "execution_count": 3,
   "metadata": {},
   "outputs": [
    {
     "output_type": "stream",
     "text": "hi\n"
    }
   ],
   "source": [
    "print('hi')\n"
   ]
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

fn params(mode: EditMode) -> NotebookEditParams {
    NotebookEditParams {
        edit_mode: mode,
        ..Default::default()
    }
}

/// A replace-mode edit of `cell_id` to `new_source` - the most common test
/// shape (swap a cell's source, keep everything else).
fn replace(cell_id: &str, new_source: &str) -> NotebookEditParams {
    NotebookEditParams {
        cell_id: Some(cell_id.into()),
        new_source: Some(new_source.into()),
        ..params(EditMode::Replace)
    }
}

#[test]
fn default_edit_mode_is_replace() {
    assert_eq!(EditMode::default(), EditMode::Replace);
    // A params with no edit_mode set defaults to replace.
    assert_eq!(NotebookEditParams::default().edit_mode, EditMode::Replace);
}

#[test]
fn replace_swaps_source_and_keeps_the_cell_id() {
    let p = NotebookEditParams {
        cell_id: Some("run".into()),
        new_source: Some("print('bye')\n".into()),
        ..params(EditMode::Replace)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    assert_eq!(result.edited_cell_id, "run");
    let nb = Notebook::parse(&result.updated_content).unwrap();
    assert_eq!(nb.cells[1].source.normalize(), "print('bye')\n");
    // A code cell replace clears outputs and null-s the execution count.
    assert_eq!(nb.cells[1].outputs, Some(Vec::new()));
    assert_eq!(nb.cells[1].execution_count, Some(serde_json::Value::Null));
}

#[test]
fn replace_preserves_the_array_source_style() {
    let p = NotebookEditParams {
        cell_id: Some("intro".into()),
        new_source: Some("# New\nsecond\n".into()),
        ..params(EditMode::Replace)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    // The original cell used array-form source, so the edit keeps it.
    assert!(result.updated_content.contains("\"# New\\n\""));
    assert!(result.updated_content.contains("\"second\\n\""));
}

#[test]
fn cell_type_conversion_on_replace_clears_outputs() {
    // Convert the code cell to markdown: outputs + execution_count vanish.
    let p = NotebookEditParams {
        cell_id: Some("run".into()),
        new_source: Some("now prose\n".into()),
        cell_type: Some(CellType::Markdown),
        ..params(EditMode::Replace)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    let nb = Notebook::parse(&result.updated_content).unwrap();
    assert_eq!(nb.cells[1].cell_type, "markdown");
    assert_eq!(nb.cells[1].outputs, None);
    assert_eq!(nb.cells[1].execution_count, None);
}

#[test]
fn insert_after_a_cell_adds_a_code_cell_by_default() {
    let p = NotebookEditParams {
        cell_id: Some("intro".into()),
        new_source: Some("x = 1\n".into()),
        ..params(EditMode::Insert)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    let nb = Notebook::parse(&result.updated_content).unwrap();
    assert_eq!(nb.cells.len(), 3);
    // Inserted AFTER intro (index 0), so it lands at index 1.
    assert_eq!(nb.cells[1].source.normalize(), "x = 1\n");
    assert_eq!(nb.cells[1].cell_type, "code");
    // nbformat 4.5 -> a generated stable id.
    assert_eq!(nb.cells[1].id.as_deref(), Some("qwen-cell-1"));
}

#[test]
fn insert_with_no_cell_id_inserts_at_the_beginning() {
    let p = NotebookEditParams {
        new_source: Some("first\n".into()),
        cell_type: Some(CellType::Markdown),
        ..params(EditMode::Insert)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    let nb = Notebook::parse(&result.updated_content).unwrap();
    assert_eq!(nb.cells.len(), 3);
    assert_eq!(nb.cells[0].source.normalize(), "first\n");
    assert_eq!(nb.cells[0].cell_type, "markdown");
}

#[test]
fn delete_removes_the_targeted_cell() {
    let p = NotebookEditParams {
        cell_id: Some("intro".into()),
        ..params(EditMode::Delete)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    assert_eq!(result.edited_cell_id, "intro");
    let nb = Notebook::parse(&result.updated_content).unwrap();
    assert_eq!(nb.cells.len(), 1);
    assert_eq!(nb.cells[0].id.as_deref(), Some("run"));
}

#[test]
fn an_ambiguous_cell_id_is_the_verbatim_error() {
    // Two cells whose display id both resolve to "cell-0": a real id
    // "cell-0" on the second cell collides with the first's fallback.
    let raw = r#"{"cells":[{"cell_type":"code","source":"a"},
                              {"cell_type":"code","source":"b","id":"cell-0"}]}"#;
    let p = NotebookEditParams {
        cell_id: Some("cell-0".into()),
        new_source: Some("x".into()),
        ..params(EditMode::Replace)
    };
    let err = apply_notebook_edit(raw, &p).unwrap_err();
    assert_eq!(
        err,
        "Cell ID \"cell-0\" is ambiguous in the rendered notebook. Re-read the notebook \
and target a stable real cell ID before editing."
    );
}

#[test]
fn a_missing_cell_id_on_replace_is_the_verbatim_error() {
    let p = NotebookEditParams {
        new_source: Some("x".into()),
        ..params(EditMode::Replace)
    };
    let err = apply_notebook_edit(FIXTURE, &p).unwrap_err();
    assert_eq!(
        err,
        "cell_id is required for replace and delete operations."
    );
}

#[test]
fn a_missing_new_source_on_replace_is_the_verbatim_error() {
    let p = NotebookEditParams {
        cell_id: Some("run".into()),
        ..params(EditMode::Replace)
    };
    let err = apply_notebook_edit(FIXTURE, &p).unwrap_err();
    assert_eq!(err, "new_source is required when edit_mode is \"replace\".");
}

#[test]
fn an_unresolved_cell_id_is_the_verbatim_error() {
    let p = NotebookEditParams {
        cell_id: Some("nope".into()),
        new_source: Some("x".into()),
        ..params(EditMode::Replace)
    };
    let err = apply_notebook_edit(FIXTURE, &p).unwrap_err();
    assert_eq!(err, "Cell with ID \"nope\" not found in notebook.");
}

#[test]
fn invalid_json_surfaces_the_parse_error() {
    let p = NotebookEditParams {
        cell_id: Some("x".into()),
        new_source: Some("y".into()),
        ..params(EditMode::Replace)
    };
    assert!(apply_notebook_edit("not json", &p).is_err());
    assert!(
        apply_notebook_edit("{}", &p)
            .unwrap_err()
            .contains("missing cells array")
    );
}

#[test]
fn the_json_format_indent_and_trailing_newline_are_preserved() {
    // FIXTURE is 1-space-indented with a trailing newline.
    let p = NotebookEditParams {
        cell_id: Some("run".into()),
        new_source: Some("print('bye')\n".into()),
        ..params(EditMode::Replace)
    };
    let result = apply_notebook_edit(FIXTURE, &p).unwrap();
    assert!(result.updated_content.ends_with("}\n"));
    // A 1-space indent means the top-level keys start with exactly one space.
    assert!(result.updated_content.contains("\n \"cells\": ["));
}

#[test]
fn a_populated_map_keeps_insertion_key_order_across_an_edit() {
    // serde_json's `preserve_order` feature makes serde_json::Map an
    // IndexMap, so a parse -> edit -> serialize round-trip keeps object
    // keys in on-disk INSERTION order. Without it, every nested map would be
    // re-sorted alphabetically on write, corrupting notebook fidelity. The
    // metadata map here is DELIBERATELY out of alphabetical order (`zebra`
    // before `alpha`) so an alphabetizing serialize would flip them.
    let raw = r##"{
 "cells": [
  {
   "cell_type": "code",
   "id": "run",
   "execution_count": 1,
   "metadata": {"zebra": 1, "alpha": 2, "middle": 3},
   "outputs": [],
   "source": ["print('hi')\n"]
  }
 ],
 "metadata": {"zulu": 1, "bravo": 2},
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;
    // Edit the OTHER concern (source) so the populated maps ride through
    // untouched, then assert their key order is the on-disk order.
    let p = replace("run", "print('bye')\n");
    let out = apply_notebook_edit(raw, &p).unwrap().updated_content;
    // The cell metadata keys stay in insertion order (zebra, alpha, middle),
    // not alphabetical (alpha, middle, zebra).
    let cell_meta = out.find("\"zebra\"").unwrap();
    assert!(cell_meta < out.find("\"alpha\"").unwrap());
    assert!(out.find("\"alpha\"").unwrap() < out.find("\"middle\"").unwrap());
    // The notebook-level metadata keys likewise stay insertion-ordered.
    assert!(out.find("\"zulu\"").unwrap() < out.find("\"bravo\"").unwrap());
}

#[test]
fn an_edit_preserves_unmodeled_metadata_and_language_info_version() {
    // A real notebook's metadata (a custom key + language_info.version) must
    // survive a notebook_edit write - the `#[serde(flatten)] extra` fields
    // carry the keys this leaf does not model.
    let raw = r##"{
 "cells": [
  {"cell_type": "code", "id": "run", "execution_count": 1, "metadata": {}, "outputs": [], "source": ["x\n"]}
 ],
 "metadata": {
  "authors": [{"name": "Ada"}],
  "language_info": {"name": "python", "version": "3.11.4"},
  "kernelspec": {"language": "python", "name": "python3"}
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;
    let p = replace("run", "y\n");
    let out = apply_notebook_edit(raw, &p).unwrap().updated_content;
    let round: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(round["metadata"]["authors"][0]["name"], "Ada");
    assert_eq!(round["metadata"]["language_info"]["version"], "3.11.4");
    assert_eq!(round["metadata"]["kernelspec"]["name"], "python3");
}

#[test]
fn a_notebook_without_a_trailing_newline_serializes_without_one() {
    let raw =
        r#"{"cells":[{"cell_type":"code","id":"c","source":"x"}],"nbformat":4,"nbformat_minor":5}"#;
    let p = NotebookEditParams {
        cell_id: Some("c".into()),
        new_source: Some("y".into()),
        ..params(EditMode::Replace)
    };
    let result = apply_notebook_edit(raw, &p).unwrap();
    assert!(!result.updated_content.ends_with('\n'));
}
