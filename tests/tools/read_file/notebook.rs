
use super::*;

fn parse(raw: &str) -> Notebook {
    Notebook::parse(raw).unwrap()
}

#[test]
fn empty_notebook_is_the_verbatim_placeholder() {
    assert_eq!(
        format(&parse(r#"{"cells":[]}"#)).content,
        "(empty notebook)"
    );
}

#[test]
fn code_cell_with_execution_count_and_output() {
    let nb = parse(
        r#"{"metadata":{"language_info":{"name":"python"}},
               "cells":[{"cell_type":"code","execution_count":5,"source":"print(1)",
                         "outputs":[{"output_type":"stream","text":"1\n"}]}]}"#,
    );
    let out = format(&nb).content;
    assert_eq!(
        out,
        "Jupyter Notebook (python, 1 cells)\n\n\
--- Code Cell cell-0 [5] ---\n\
```python\n\
print(1)\n\
```\n\
Output:\n\
1\n"
    );
}

#[test]
fn code_cell_without_execution_count_has_no_label() {
    let nb = parse(r#"{"cells":[{"cell_type":"code","execution_count":null,"source":"x=1"}]}"#);
    let out = format(&nb).content;
    assert!(out.contains("--- Code Cell cell-0 ---"));
    assert!(!out.contains('['));
}

#[test]
fn markdown_and_raw_cells_use_their_own_markers() {
    let nb = parse(
        r##"{"cells":[{"cell_type":"markdown","source":"# Title","id":"md1"},
                        {"cell_type":"raw","source":"raw text"}]}"##,
    );
    let out = format(&nb).content;
    assert!(out.contains("--- Markdown Cell md1 ---\n# Title"));
    assert!(out.contains("--- Raw Cell cell-1 ---\nraw text"));
}

#[test]
fn execute_result_text_plain_is_rendered_ansi_stripped() {
    let nb = parse(
        r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"execute_result",
                           "data":{"text/plain":"\u001b[31mred\u001b[0m"}}]}]}"#,
    );
    let out = format(&nb).content;
    assert!(out.contains("Output:\nred"));
    assert!(!out.contains('\u{1b}'));
}

#[test]
fn non_text_output_surfaces_a_sanitized_mime_placeholder() {
    let nb = parse(
        r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"display_data",
                           "data":{"image/png":"...","not a mime":"x"}}]}]}"#,
    );
    let out = format(&nb).content;
    assert!(out.contains("[non-text output: image/png]"));
    assert!(!out.contains("not a mime"));
}

#[test]
fn error_output_joins_ename_evalue_traceback() {
    let nb = parse(
        r#"{"cells":[{"cell_type":"code","source":"x",
               "outputs":[{"output_type":"error","ename":"ValueError","evalue":"bad",
                           "traceback":["\u001b[31mline1","line2"]}]}]}"#,
    );
    let out = format(&nb).content;
    assert!(out.contains("ValueError: bad: line1\nline2"));
}

#[test]
fn large_code_output_is_cut_with_the_verbatim_marker() {
    let big = "a".repeat(LARGE_OUTPUT_THRESHOLD + 50);
    let raw = format!(
        r#"{{"cells":[{{"cell_type":"code","source":"x",
               "outputs":[{{"output_type":"stream","text":{}}}]}}]}}"#,
        serde_json::to_string(&big).unwrap()
    );
    let out = format(&parse(&raw)).content;
    assert!(out.contains(&format!(
            "... [output truncated, total {} chars. Use shell: cat <notebook_path> | jq '.cells[0].outputs']",
            LARGE_OUTPUT_THRESHOLD + 50
        )));
}

#[test]
fn whole_notebook_cell_budget_truncates_with_the_verbatim_marker() {
    // Each cell's source is big enough that a handful blow the 100k budget.
    let big_source = serde_json::to_string(&"z".repeat(30_000)).unwrap();
    let cell = format!(r#"{{"cell_type":"code","source":{big_source}}}"#);
    let cells: Vec<String> = std::iter::repeat_n(cell, 5).collect();
    let raw = format!(r#"{{"cells":[{}]}}"#, cells.join(","));
    let read = format(&parse(&raw));
    assert!(read.is_truncated);
    assert!(
        read.content
            .contains("remaining cells truncated, total 5 cells")
    );
    assert!(read.content.contains("jq '.cells["));
}

#[test]
fn read_surfaces_parse_errors() {
    assert!(read_with_meta("not json").is_err());
    assert!(
        read_with_meta("{}")
            .unwrap_err()
            .contains("missing cells array")
    );
}
