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

fn text(row: &McpRow) -> String {
    row.spans.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn tool_detail_shows_description_and_parameters() {
    let mut t = tool("query", "runs a query");
    t.input_schema = json!({
        "properties": {
            "q": { "type": "string", "description": "the query" },
            "limit": { "type": "number" }
        },
        "required": ["q"]
    });
    let content = tool_detail_content(&t);
    let joined: Vec<String> = content.iter().map(text).collect();
    assert!(joined.contains(&"Description:".to_string()));
    assert!(joined.contains(&"runs a query".to_string()));
    assert!(joined.contains(&"Parameters:".to_string()));
    assert!(
        joined
            .iter()
            .any(|r| r == "• q(required): string - the query")
    );
    assert!(joined.iter().any(|r| r == "• limit: number"));
}

#[test]
fn an_invalid_tool_detail_shows_the_cannot_be_called_warning() {
    let t = tool("bad", ""); // missing description
    let joined: Vec<String> = tool_detail_content(&t).iter().map(text).collect();
    assert!(joined.contains(&"Warning: This tool cannot be called by the LLM".to_string()));
    assert!(joined.contains(&"Reason: missing description".to_string()));
    assert!(
        joined.contains(
            &"Tools must have both name and description to be used by the LLM.".to_string()
        )
    );
}
