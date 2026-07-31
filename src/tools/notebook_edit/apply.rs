//! The pure cell-level edit over the shared [`crate::notebook`] model - a
//! VERBATIM port of qwen v0.16.0 `packages/core/src/tools/notebook-edit.ts`
//! (`applyNotebookEdit`) plus the format helpers it leans on from `notebook.ts`
//! (`inferNotebookJsonFormat`, `serializeNotebook`, `normalizeEditedCell`).
//!
//! [`apply_notebook_edit`] is filesystem-free: it takes the notebook's raw JSON
//! text and the edit params, resolves the target cell by display id, mutates the
//! parsed model (replace / insert / delete, with cell-type conversion clearing
//! outputs), and serializes back preserving the on-disk JSON format (indent +
//! trailing newline). The tool wrapper in [`super`] owns the I/O, the read-cache
//! enforcement, and the atomic write.

use crate::notebook::{Cell, Notebook, Source};
use serde::{Deserialize, Serialize};

/// The cell type an edit may set (qwen `EditableNotebookCellType`): a notebook
/// cell may also be `raw`, but an edit only ever sets `code` or `markdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellType {
    Code,
    Markdown,
}

impl CellType {
    fn as_str(self) -> &'static str {
        match self {
            CellType::Code => "code",
            CellType::Markdown => "markdown",
        }
    }
}

/// The edit operation (qwen `NotebookEditMode`), defaulting to `replace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Replace,
    Insert,
    Delete,
}

impl EditMode {
    /// The mode's lowercase wire word (qwen's mode string) - the single home for
    /// this mapping, shared by `apply`'s "0 occurrences" error and the tool
    /// wrapper's result summary.
    pub fn as_str(self) -> &'static str {
        match self {
            EditMode::Replace => "replace",
            EditMode::Insert => "insert",
            EditMode::Delete => "delete",
        }
    }
}

/// The cell-level edit params (qwen `NotebookEditToolParams`, minus
/// `notebook_path` which the tool wrapper owns). `cell_id` is the display id
/// from read_file output; `new_source` is required for replace/insert;
/// `cell_type` sets an inserted cell's type or converts a replaced cell.
#[derive(Debug, Clone, Default)]
pub struct NotebookEditParams {
    pub cell_id: Option<String>,
    pub new_source: Option<String>,
    pub cell_type: Option<CellType>,
    pub edit_mode: EditMode,
}

/// The result of a successful edit (qwen `NotebookEditResult`, narrowed to what
/// the tool wrapper reports): the serialized notebook plus the display id and
/// mode the wrapper echoes to the model.
#[derive(Debug)]
pub struct NotebookEditResult {
    pub updated_content: String,
    pub edited_cell_id: String,
    pub mode: EditMode,
    /// Whether this edit lost stable cell ids and so the read cache must be
    /// INVALIDATED (a re-read forced) instead of recorded as a fresh write (qwen
    /// `requiresReadAfterWrite`): a structural edit (insert/delete) where the
    /// notebook did NOT carry stable ids both before and after. Without stable
    /// ids the `cell-N` fallback display ids renumber, so a second cell-level
    /// edit against an id the model has not re-verified would target the wrong
    /// cell. Forcing a re-read makes the model requote from fresh output.
    pub requires_read_after_write: bool,
}

/// The JSON format inferred from the on-disk text (qwen `NotebookJsonFormat`):
/// the indent width (spaces) and whether the file ended with a newline. Both are
/// preserved on write so an edit does not reflow the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonFormat {
    indent: usize,
    trailing_newline: bool,
}

/// Apply a cell-level edit to a notebook's raw JSON text (qwen
/// `applyNotebookEdit`). Returns the serialized updated notebook, or a VERBATIM
/// qwen error string on invalid JSON / bad params / an unresolved or ambiguous
/// cell id.
// qual:allow(complexity, error_handling) reason: "the two `.expect()`s guard a
// real invariant `resolve_target_index` already proved - delete/replace always
// resolve to Some(index) (an insert with no cell_id is the only None arm), so
// the panic is unreachable; a `?` would need a fabricated error string for an
// impossible case."
pub fn apply_notebook_edit(
    raw: &str,
    params: &NotebookEditParams,
) -> Result<NotebookEditResult, String> {
    let mut notebook = Notebook::parse(raw)?;
    let mode = params.edit_mode;
    let source = require_source(params.new_source.as_deref(), mode)?;
    let target_index = resolve_target_index(&notebook, params.cell_id.as_deref(), mode)?;
    let format = infer_json_format(raw);
    // Snapshot before the mutation: a structural edit's read-after-write need
    // (below) turns on whether the notebook carried stable ids BOTH before and
    // after (qwen `originalHasStableCellIds`).
    let original_has_stable_cell_ids = notebook.has_stable_cell_ids();

    let edited_cell_id = match mode {
        EditMode::Insert => {
            let cell_type = params.cell_type.unwrap_or(CellType::Code);
            let insert_at = match target_index {
                None => 0,
                Some(i) => i + 1,
            };
            let prefer_array = notebook.inserted_source_array_style(insert_at);
            let new_cell = make_cell(&notebook, cell_type, source, prefer_array);
            let id = new_cell.display_id(insert_at);
            notebook.cells.insert(insert_at, new_cell);
            id
        }
        EditMode::Delete => {
            // resolve_target_index guarantees Some for delete.
            let index = target_index.expect("delete requires a resolved cell_id");
            let removed = notebook.cells.remove(index);
            removed.display_id(index)
        }
        EditMode::Replace => {
            let index = target_index.expect("replace requires a resolved cell_id");
            // The final type is the requested cell_type, else the target's own.
            let final_type = params.cell_type.map(CellType::as_str).unwrap_or_else(|| {
                // `code`/`markdown`/`raw` - `resolve_target_index` proved the
                // index is in range.
                cell_type_str(&notebook.cells[index])
            });
            let prefer_array = notebook.cells[index].source_is_array();
            let target = &mut notebook.cells[index];
            target.source = Source::from_str_preferring(source, prefer_array);
            normalize_edited_cell(target, final_type);
            target.display_id(index)
        }
    };

    // qwen `requiresReadAfterWrite`: a structural edit (insert/delete) forces a
    // re-read UNLESS the notebook carried stable ids both before AND after. A
    // replace never renumbers the `cell-N` fallbacks, so it never forces one.
    let structural_edit = matches!(mode, EditMode::Insert | EditMode::Delete);
    let requires_read_after_write =
        structural_edit && !(original_has_stable_cell_ids && notebook.has_stable_cell_ids());

    Ok(NotebookEditResult {
        updated_content: serialize_notebook(&notebook, format),
        edited_cell_id,
        mode,
        requires_read_after_write,
    })
}

/// `new_source` is required for replace/insert, empty for delete (qwen
/// `requireNotebookSource`). The VERBATIM missing-source error names the mode.
fn require_source(source: Option<&str>, mode: EditMode) -> Result<&str, String> {
    if mode == EditMode::Delete {
        return Ok("");
    }
    source.ok_or_else(|| {
        let m = mode.as_str();
        format!("new_source is required when edit_mode is \"{m}\".")
    })
}

/// Resolve the target cell index for the edit (qwen `resolveTargetIndex`):
/// `None` for an insert with no `cell_id` (insert at the beginning); the single
/// index a `cell_id` resolves to otherwise. VERBATIM errors for a missing
/// `cell_id` on replace/delete, an ambiguous id, and an id that matches no cell.
fn resolve_target_index(
    notebook: &Notebook,
    cell_id: Option<&str>,
    mode: EditMode,
) -> Result<Option<usize>, String> {
    let cell_id = match cell_id {
        None => {
            if mode == EditMode::Insert {
                return Ok(None);
            }
            return Err("cell_id is required for replace and delete operations.".to_string());
        }
        Some(id) => id,
    };

    if notebook.is_ambiguous_cell_id(cell_id) {
        return Err(format!(
            "Cell ID \"{cell_id}\" is ambiguous in the rendered notebook. Re-read the notebook \
and target a stable real cell ID before editing."
        ));
    }

    match notebook.find_cell_index(cell_id) {
        Some(index) => Ok(Some(index)),
        None => Err(format!("Cell with ID \"{cell_id}\" not found in notebook.")),
    }
}

/// Build a new cell for an insert (qwen `createNotebookCell`): the source in the
/// inferred array style, an empty metadata map, a generated id when the format
/// supports one, then normalized (code cells get `execution_count: null` +
/// `outputs: []`).
fn make_cell(notebook: &Notebook, cell_type: CellType, source: &str, prefer_array: bool) -> Cell {
    let mut cell = Cell {
        cell_type: cell_type.as_str().to_string(),
        source: Source::from_str_preferring(source, prefer_array),
        metadata: Some(serde_json::Map::new()),
        outputs: None,
        execution_count: None,
        id: notebook.make_cell_id(),
        ..Cell::default()
    };
    normalize_edited_cell(&mut cell, cell_type.as_str());
    cell
}

/// Normalize an edited cell to its final type (qwen `normalizeEditedCell`): set
/// the type, ensure a metadata map, and for a `code` cell clear outputs
/// (`execution_count: null`, `outputs: []`); for any other type DROP the
/// code-only fields. A type conversion therefore clears outputs, faithful to
/// qwen.
fn normalize_edited_cell(cell: &mut Cell, final_type: &str) {
    cell.cell_type = final_type.to_string();
    if cell.metadata.is_none() {
        cell.metadata = Some(serde_json::Map::new());
    }
    if final_type == "code" {
        cell.execution_count = Some(serde_json::Value::Null);
        cell.outputs = Some(Vec::new());
    } else {
        cell.execution_count = None;
        cell.outputs = None;
    }
}

/// The cell's type as a `&str` (for the replace-mode "keep the current type"
/// path).
fn cell_type_str(cell: &Cell) -> &'static str {
    match cell.cell_type.as_str() {
        "code" => "code",
        "markdown" => "markdown",
        "raw" => "raw",
        // An unknown type on disk is preserved as code-shaped output would be
        // wrong; fall back to code (qwen keeps `target.cell_type` verbatim - a
        // string - here we only reach this arm for a well-formed notebook).
        _ => "code",
    }
}

/// Infer the on-disk JSON format (qwen `inferNotebookJsonFormat`): the indent is
/// the run of spaces before the first `"` that follows a newline; the trailing
/// newline is whether the text ends in `\n`. Defaults to qwen's fallback
/// (`indent: 1, trailingNewline: true`) when no indented line is found.
fn infer_json_format(content: &str) -> JsonFormat {
    let indent = first_indented_line_width(content).unwrap_or(1);
    JsonFormat {
        indent,
        trailing_newline: content.ends_with('\n'),
    }
}

/// The count of spaces in the first `\n<spaces>"` run (qwen's `/\n( +)"/`). A
/// notebook with no such line (single-line JSON) yields `None`.
fn first_indented_line_width(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b'"' {
                return Some(j - (i + 1));
            }
        }
        i += 1;
    }
    None
}

/// Serialize the notebook back to text (qwen `serializeNotebook`): pretty JSON at
/// the inferred indent width, with a trailing newline when the original had one.
/// Matches `JSON.stringify(notebook, null, indent)` - a space-only indent, keys
/// in insertion order (serde preserves object order via `serde_json`'s
/// preserve-order feature the crate already enables).
// qual:allow(complexity, error_handling) reason: "both `.expect()`s hold a real
// invariant: a notebook that already PARSED re-serializes (serde only errors on
// a custom Serialize, which the derived impls are not), and serde_json emits
// valid UTF-8 by construction - a `?` here would fabricate an error for an
// impossible case."
fn serialize_notebook(notebook: &Notebook, format: JsonFormat) -> String {
    let indent = " ".repeat(format.indent);
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    notebook
        .serialize(&mut ser)
        .expect("a parsed notebook re-serializes");
    let mut out = String::from_utf8(buf).expect("serde_json emits valid UTF-8");
    if format.trailing_newline {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
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
        let raw = r#"{"cells":[{"cell_type":"code","id":"c","source":"x"}],"nbformat":4,"nbformat_minor":5}"#;
        let p = NotebookEditParams {
            cell_id: Some("c".into()),
            new_source: Some("y".into()),
            ..params(EditMode::Replace)
        };
        let result = apply_notebook_edit(raw, &p).unwrap();
        assert!(!result.updated_content.ends_with('\n'));
    }
}
