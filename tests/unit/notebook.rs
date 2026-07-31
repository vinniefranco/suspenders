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

    let absent: Cell = serde_json::from_str(r#"{"cell_type":"markdown","source":"x"}"#).unwrap();
    assert_eq!(absent.execution_count, None);

    let number: Cell =
        serde_json::from_str(r#"{"cell_type":"code","source":"x","execution_count":7}"#).unwrap();
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
