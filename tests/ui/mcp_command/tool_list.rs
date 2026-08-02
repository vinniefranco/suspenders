use super::*;
use crate::mcp::McpToolAnnotations;
use serde_json::json;

fn tool(name: &str, description: &str) -> McpToolView {
    McpToolView {
        name: name.to_string(),
        description: description.to_string(),
        annotations: McpToolAnnotations::default(),
        input_schema: json!({}),
    }
}

fn annotated(name: &str, a: McpToolAnnotations) -> McpToolView {
    McpToolView {
        name: name.to_string(),
        description: "d".to_string(),
        annotations: a,
        input_schema: json!({}),
    }
}

fn text(row: &McpRow) -> String {
    row.spans.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn a_valid_tool_row_shows_its_annotation_words_in_qwen_order() {
    let a = McpToolAnnotations {
        read_only: true,
        destructive: true,
        idempotent: true,
        open_world: true,
    };
    let row = tool_row(&annotated("t", a), false);
    // qwen order: destructive, read-only, open-world, idempotent.
    assert!(text(&row).contains("destructive, read-only, open-world, idempotent"));
}

#[test]
fn an_invalid_tool_row_shows_the_invalid_reason_not_annotations() {
    let mut t = tool("bad", ""); // missing description
    t.annotations = McpToolAnnotations {
        read_only: true,
        ..Default::default()
    };
    let row = tool_row(&t, false);
    assert!(text(&row).contains("invalid: missing description"));
    assert!(
        !text(&row).contains("read-only"),
        "invalid hides annotations"
    );
}

#[test]
fn the_scroll_offset_tracks_the_active_row_within_bounds() {
    // Below the window: no scroll.
    assert_eq!(scroll_offset(0, 20), 0);
    assert_eq!(scroll_offset(8, 20), 0);
    // At/after the last visible row: the window follows the active row.
    assert_eq!(scroll_offset(9, 20), 0);
    assert_eq!(scroll_offset(10, 20), 1);
    // Clamped so the window never runs past the end.
    assert_eq!(scroll_offset(19, 20), 10);
}
