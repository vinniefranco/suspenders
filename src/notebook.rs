//! The shared Jupyter Notebook (`.ipynb`) model - a std+serde-only leaf so both
//! read_file (P3 3b, this phase) and a future notebook_edit (P3 3c) import the
//! same `Notebook`/`Cell` shapes from the crate root with no tool-graph cycle.
//!
//! Ported from qwen v0.16.0 `packages/core/src/utils/notebook.ts` (the
//! `NotebookContent`/`NotebookCell`/`NotebookCellOutput` interfaces and the
//! cell-id helpers). `source` is `string | string[]` on the wire, so it rides an
//! untagged enum here. Formatting a notebook into the read_file text projection
//! lives in `tools::read_file::notebook`; this leaf is just the data model plus
//! the id/language helpers both phases share.

use serde::{Deserialize, Serialize};

/// The nbformat major version at which Jupyter began generating stable cell ids
/// (nbformat 4). See [`Notebook::should_generate_cell_ids`].
const NBFORMAT_STABLE_IDS_MAJOR: i64 = 4;
/// The nbformat 4.x minor version at which stable cell ids became the default
/// (nbformat 4.5). See [`Notebook::should_generate_cell_ids`].
const NBFORMAT_STABLE_IDS_MINOR: i64 = 5;

/// Deserialize a present `execution_count` field, keeping the JSON literal
/// `null` as `Some(Value::Null)` (not `None`). serde's default `Option` handling
/// folds a present `null` onto `None`, which would make an absent field and a
/// present-null field indistinguishable; a code cell qwen normalized carries
/// `"execution_count": null`, so that distinction must survive a round-trip.
/// An ABSENT field never calls this (it takes the `default`, `None`).
fn deserialize_present_null<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(Some(value))
}

/// A cell's source: the JSON is either one string or an array of line-strings
/// (Jupyter stores multi-line source as a `string[]`). `#[serde(untagged)]`
/// accepts both; [`Source::normalize`] joins the array form (qwen's
/// `normalizeSource`, a plain `join('')` - the array elements already carry
/// their own trailing newlines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Text(String),
    Lines(Vec<String>),
}

impl Source {
    /// The source as one string (qwen `normalizeSource`): the array form joined
    /// with no separator, since each element already ends in `\n`.
    pub fn normalize(&self) -> String {
        match self {
            Source::Text(s) => s.clone(),
            Source::Lines(lines) => lines.concat(),
        }
    }
}

impl Default for Source {
    fn default() -> Self {
        Source::Text(String::new())
    }
}

/// A single output on a code cell (qwen `NotebookCellOutput`). Every field is
/// optional so partial/odd notebooks deserialize without loss; the formatter
/// reads only what a given `output_type` carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CellOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evalue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceback: Option<Vec<String>>,
    /// Every other on-disk key (e.g. an output's `metadata`, or a `png` field on
    /// an nbformat-3 output). qwen round-trips the whole output object verbatim,
    /// so an unmodeled key must survive a parse -> edit -> serialize pass rather
    /// than vanish on the next notebook_edit write.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A notebook cell (qwen `NotebookCell`). `cell_type` is `code | markdown | raw`
/// on well-formed notebooks; kept a `String` so an unknown type still parses and
/// formats through the default arm.
// qual:allow(srp, god_struct) reason: "serde ipynb wire DTO - fields are independent JSON keys preserved verbatim for notebook round-trip; splitting breaks fidelity"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cell {
    pub cell_type: String,
    #[serde(default)]
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<CellOutput>>,
    /// The code cell's execution count. A `serde_json::Value` (not `Option<i64>`)
    /// so notebook_edit can round-trip the three distinct on-disk shapes qwen
    /// preserves: absent (`None`, e.g. a markdown cell), the JSON literal `null`
    /// (`Some(Value::Null)`, a normalized/never-run code cell), and a number
    /// (`Some(Value::Number)`, an executed code cell). `Option<i64>` collapses
    /// absent and `null` onto the same `None`, losing qwen's `normalizeEditedCell`
    /// output where a code cell carries `"execution_count": null`. The custom
    /// deserializer keeps a present `null` as `Some(Value::Null)` rather than
    /// `None` (serde's default folds them together), so absent vs. present-null
    /// survives a parse -> edit -> serialize round-trip.
    #[serde(
        default,
        deserialize_with = "deserialize_present_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_count: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Every other on-disk key (e.g. `attachments` on a markdown cell). qwen
    /// round-trips the whole cell object verbatim, so an unmodeled key must
    /// survive a notebook_edit write rather than silently drop.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Cell {
    /// The cell's stable display id (qwen `getCellDisplayId`): its own `id` when
    /// non-empty, else `cell-<index>` (0-based).
    pub fn display_id(&self, index: usize) -> String {
        match &self.id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => format!("cell-{index}"),
        }
    }

    /// Whether this cell's `source` is stored in the array form (qwen's
    /// `Array.isArray(cell.source)`). notebook_edit preserves the surrounding
    /// cells' source shape when it writes an edited or inserted cell.
    pub fn source_is_array(&self) -> bool {
        matches!(self.source, Source::Lines(_))
    }
}

/// The notebook's metadata, just the slice the language lookup needs (qwen
/// `NotebookContent.metadata`). Other keys deserialize into `extra` so a
/// round-trip (P3 3c) does not drop them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_info: Option<LanguageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernelspec: Option<Kernelspec>,
    /// Every other metadata key (authors, title, widgets, ...). Flattened so a
    /// real notebook's metadata survives a notebook_edit write verbatim rather
    /// than collapsing to just the two modeled keys.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LanguageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Every other `language_info` key (version, mimetype, file_extension, ...).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Kernelspec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Every other `kernelspec` key (name, ...).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The top-level notebook (qwen `NotebookContent`). `cells` is required for a
/// valid notebook; everything else is optional.
// qual:allow(srp, god_struct) reason: "serde ipynb wire DTO - fields are independent JSON keys preserved verbatim for notebook round-trip; splitting breaks fidelity"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Notebook {
    #[serde(default)]
    pub cells: Vec<Cell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbformat: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbformat_minor: Option<i64>,
    /// Every other top-level key. qwen round-trips the whole notebook object
    /// verbatim, so a top-level key it does not model must survive a write.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Notebook {
    /// Parse a `.ipynb`'s JSON text, stripping a leading BOM (qwen
    /// `parseNotebook`). A missing/non-array `cells` field surfaces the same
    /// "missing cells array" wording qwen throws.
    pub fn parse(content: &str) -> Result<Notebook, String> {
        // Strip a leading UTF-8 BOM the way qwen slices `﻿`.
        let json = content.strip_prefix('\u{feff}').unwrap_or(content);
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("Invalid notebook: {e}"))?;
        if !value.is_object() {
            return Err("Invalid notebook: expected a JSON object".to_string());
        }
        if !value.get("cells").map(|c| c.is_array()).unwrap_or(false) {
            return Err("Invalid notebook: missing cells array".to_string());
        }
        serde_json::from_value(value).map_err(|e| format!("Invalid notebook: {e}"))
    }

    /// The notebook's language (qwen `getNotebookLanguage`): `language_info.name`
    /// else `kernelspec.language` else `python`.
    pub fn language(&self) -> String {
        self.metadata
            .as_ref()
            .and_then(|m| {
                m.language_info
                    .as_ref()
                    .and_then(|li| li.name.clone())
                    .or_else(|| m.kernelspec.as_ref().and_then(|k| k.language.clone()))
            })
            .unwrap_or_else(|| "python".to_string())
    }

    /// Whether every cell carries a stable, non-empty `id` (qwen
    /// `hasStableCellIds`). notebook_edit uses it to decide whether a structural
    /// edit (insert/delete) leaves the cell ids the model quoted still valid.
    pub fn has_stable_cell_ids(&self) -> bool {
        self.cells
            .iter()
            .all(|c| c.id.as_deref().is_some_and(|id| !id.is_empty()))
    }

    /// The 0-based indexes of every cell whose display id equals `cell_id` (qwen
    /// `findCellIndexesByDisplayId`). More than one means the id is ambiguous.
    pub fn indexes_by_display_id(&self, cell_id: &str) -> Vec<usize> {
        self.cells
            .iter()
            .enumerate()
            .filter(|(i, c)| c.display_id(*i) == cell_id)
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether `cell_id` resolves to MORE THAN ONE cell (qwen
    /// `isAmbiguousCellId`): a `cell-N` fallback can collide with a real cell
    /// whose id is literally `cell-N`, and notebook_edit refuses to guess.
    pub fn is_ambiguous_cell_id(&self, cell_id: &str) -> bool {
        self.indexes_by_display_id(cell_id).len() > 1
    }

    /// The single 0-based index `cell_id` resolves to (qwen `findCellIndex`), or
    /// `None` if it matches no cell or is ambiguous (the caller checks ambiguity
    /// separately for its own error).
    pub fn find_cell_index(&self, cell_id: &str) -> Option<usize> {
        let indexes = self.indexes_by_display_id(cell_id);
        match indexes.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// Whether this notebook's format generates stable cell ids (qwen
    /// `shouldGenerateCellIds`): nbformat 4.5+ (or any nbformat past 4). A
    /// nbformat-4.5 notebook gets a generated id for an inserted cell so the
    /// model can target it on the next edit.
    pub fn should_generate_cell_ids(&self) -> bool {
        let nbformat = self.nbformat.unwrap_or(0);
        nbformat > NBFORMAT_STABLE_IDS_MAJOR
            || (nbformat == NBFORMAT_STABLE_IDS_MAJOR
                && self.nbformat_minor.unwrap_or(0) >= NBFORMAT_STABLE_IDS_MINOR)
    }

    /// A fresh, unused display id for a newly inserted cell (qwen `makeCellId`):
    /// `qwen-cell-<n>` for the first `n` not already a display id, or `None` when
    /// the format does not generate ids ([`Notebook::should_generate_cell_ids`]).
    pub fn make_cell_id(&self) -> Option<String> {
        if !self.should_generate_cell_ids() {
            return None;
        }
        let existing: std::collections::HashSet<String> = self
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| c.display_id(i))
            .collect();
        let mut n = 1;
        loop {
            let candidate = format!("qwen-cell-{n}");
            if !existing.contains(&candidate) {
                return Some(candidate);
            }
            n += 1;
        }
    }

    /// Whether NEW cells should store their source in the array form (qwen
    /// `inferNotebookSourceArrayStyle`): match the first cell that has a source,
    /// else default to the array form.
    pub fn source_array_style(&self) -> bool {
        self.cells
            .iter()
            .find(|c| {
                !matches!(c.source, Source::Text(ref s) if s.is_empty()) || c.source_is_array()
            })
            .map(Cell::source_is_array)
            .unwrap_or(true)
    }

    /// Whether a cell inserted at `insert_at` should store source in the array
    /// form (qwen `inferInsertedCellSourceArrayStyle`): match the cell before it,
    /// else the cell after it, else the notebook-wide style.
    pub fn inserted_source_array_style(&self, insert_at: usize) -> bool {
        if insert_at > 0
            && let Some(prev) = self.cells.get(insert_at - 1)
        {
            return prev.source_is_array();
        }
        if let Some(next) = self.cells.get(insert_at) {
            return next.source_is_array();
        }
        self.source_array_style()
    }
}

impl Source {
    /// Build a [`Source`] from a plain string, in the array form when
    /// `prefer_array` (qwen `toNotebookSource`): split into lines, each keeping
    /// its trailing `\n`, so a round-trip through [`Source::normalize`] is exact.
    /// An empty string becomes an empty array.
    pub fn from_str_preferring(source: &str, prefer_array: bool) -> Source {
        if !prefer_array {
            return Source::Text(source.to_string());
        }
        if source.is_empty() {
            return Source::Lines(Vec::new());
        }
        let mut lines: Vec<String> = Vec::new();
        let mut start = 0;
        let bytes = source.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                lines.push(source[start..=i].to_string());
                start = i + 1;
            }
        }
        if start < source.len() {
            lines.push(source[start..].to_string());
        }
        Source::Lines(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_normalizes_string_and_array_forms() {
        assert_eq!(Source::Text("a\nb".into()).normalize(), "a\nb");
        assert_eq!(
            Source::Lines(vec!["a\n".into(), "b".into()]).normalize(),
            "a\nb"
        );
    }

    #[test]
    fn display_id_is_the_id_when_present_else_cell_index() {
        let with_id = Cell {
            id: Some("abc".into()),
            ..Cell::default()
        };
        assert_eq!(with_id.display_id(2), "abc");
        let no_id = Cell::default();
        assert_eq!(no_id.display_id(2), "cell-2");
        let empty_id = Cell {
            id: Some(String::new()),
            ..Cell::default()
        };
        assert_eq!(empty_id.display_id(4), "cell-4");
    }

    #[test]
    fn language_falls_back_language_info_then_kernelspec_then_python() {
        let li = Notebook {
            metadata: Some(Metadata {
                language_info: Some(LanguageInfo {
                    name: Some("julia".into()),
                    ..LanguageInfo::default()
                }),
                kernelspec: None,
                ..Metadata::default()
            }),
            ..Notebook::default()
        };
        assert_eq!(li.language(), "julia");

        let ks = Notebook {
            metadata: Some(Metadata {
                language_info: None,
                kernelspec: Some(Kernelspec {
                    language: Some("r".into()),
                    display_name: None,
                    ..Kernelspec::default()
                }),
                ..Metadata::default()
            }),
            ..Notebook::default()
        };
        assert_eq!(ks.language(), "r");

        assert_eq!(Notebook::default().language(), "python");
    }

    #[test]
    fn parse_rejects_non_object_and_missing_cells() {
        assert!(Notebook::parse("[]").unwrap_err().contains("JSON object"));
        assert!(
            Notebook::parse("{}")
                .unwrap_err()
                .contains("missing cells array")
        );
    }

    #[test]
    fn parse_strips_a_leading_bom() {
        let nb = Notebook::parse("\u{feff}{\"cells\": []}").unwrap();
        assert!(nb.cells.is_empty());
    }

    #[test]
    fn parse_accepts_both_source_shapes() {
        let nb = Notebook::parse(
            r##"{"cells":[{"cell_type":"code","source":["x=1\n","y=2"]},
                        {"cell_type":"markdown","source":"# hi"}]}"##,
        )
        .unwrap();
        assert_eq!(nb.cells[0].source.normalize(), "x=1\ny=2");
        assert_eq!(nb.cells[1].source.normalize(), "# hi");
    }

    #[test]
    fn deserialize_present_null_keeps_absent_and_null_apart() {
        // A present JSON `null` deserializes to Some(Value::Null) (the custom
        // deserializer), while an ABSENT field takes the default None. This is
        // the distinction a code cell qwen normalized (`execution_count: null`)
        // relies on to round-trip.
        let present_null: Cell =
            serde_json::from_str(r#"{"cell_type":"code","source":"x","execution_count":null}"#)
                .unwrap();
        assert_eq!(present_null.execution_count, Some(serde_json::Value::Null));

        let absent: Cell =
            serde_json::from_str(r#"{"cell_type":"markdown","source":"x"}"#).unwrap();
        assert_eq!(absent.execution_count, None);

        let number: Cell =
            serde_json::from_str(r#"{"cell_type":"code","source":"x","execution_count":7}"#)
                .unwrap();
        assert_eq!(
            number.execution_count,
            Some(serde_json::Value::Number(7.into()))
        );
    }

    #[test]
    fn unmodeled_keys_survive_a_parse_serialize_round_trip() {
        // A real notebook carries keys this leaf does not model at every level:
        // top-level, metadata, language_info.version, kernelspec.name, a cell's
        // own key, and an output's metadata. `#[serde(flatten)] extra` must keep
        // every one across a round-trip - notebook_edit re-serializes the whole
        // object, so a dropped key here is silent data loss on every write.
        let raw = r##"{
 "custom_top": "keep me",
 "cells": [
  {
   "cell_type": "code",
   "attachments": {"a.png": "b64"},
   "execution_count": null,
   "metadata": {},
   "outputs": [
    {"output_type": "stream", "text": "hi\n", "name": "stdout"}
   ],
   "source": ["print('hi')\n"]
  }
 ],
 "metadata": {
  "title": "My Notebook",
  "language_info": {"name": "python", "version": "3.11.4"},
  "kernelspec": {"language": "python", "name": "python3"}
 },
 "nbformat": 4,
 "nbformat_minor": 5
}"##;
        let nb = Notebook::parse(raw).unwrap();
        // Round-trip through serde and re-parse: every unmodeled key is present.
        let round = serde_json::to_value(&nb).unwrap();
        assert_eq!(round["custom_top"], serde_json::json!("keep me"));
        assert_eq!(round["cells"][0]["attachments"]["a.png"], "b64");
        assert_eq!(round["cells"][0]["outputs"][0]["name"], "stdout");
        assert_eq!(round["metadata"]["title"], "My Notebook");
        assert_eq!(round["metadata"]["language_info"]["version"], "3.11.4");
        assert_eq!(round["metadata"]["kernelspec"]["name"], "python3");
    }
}
