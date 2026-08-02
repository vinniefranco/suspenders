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
#[path = "../../../tests/tools/notebook_edit/apply.rs"]
mod tests;
