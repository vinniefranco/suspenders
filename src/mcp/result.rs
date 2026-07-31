//! Collapsing an [`McpCallResult`] to the canonical text a Tool Result carries
//! (ADR-0039, ADR-0056).
//!
//! Suspenders' Tool Results stay canonical text: no rich display channel, no
//! inline media. So an MCP result's content blocks fold to one string, mirroring
//! qwen's `getDisplayFromParts` collapse (text verbatim, media as a placeholder
//! line). An error result comes back `Err(joined)` so the Tools dispatch marks
//! it `is_error` - the same shape every other tool's failure takes.

use crate::mcp::{McpBlock, McpCallResult};

/// Collapses the result to the canonical Tool Result text. Text blocks enter
/// verbatim; media and blob resources become a placeholder line that names the
/// tool (ADR-0056: no inline data, but the model still learns which tool
/// provided what); a resource link becomes a `Resource Link:` line. Parts join
/// on newlines; an empty result is the empty string. When the server flagged the
/// call an error, the joined text comes back `Err` so the dispatch marks the
/// Tool Result `is_error`; an empty error joins to a non-empty fallback so the
/// model never sees a blank error. `tool_name` is the tool's wire name, so the
/// placeholder reads exactly as qwen's (`[Tool '<tool_name>' provided ...]`).
pub fn render(result: &McpCallResult, tool_name: &str) -> Result<String, String> {
    let parts: Vec<String> = result
        .content
        .iter()
        .map(|block| render_block(block, tool_name))
        .collect();
    let joined = parts.join("\n");
    if result.is_error {
        if joined.is_empty() {
            Err("the MCP tool reported an error with no message".to_string())
        } else {
            Err(joined)
        }
    } else {
        Ok(joined)
    }
}

/// One content block to its canonical line. Media carries only its descriptor
/// (ADR-0056: the model sees a placeholder, never the bytes). The placeholder
/// names the tool, mirroring qwen's `mcp-tool.ts`
/// (`[Tool '<tool_name>' provided the following ...]`), so the model can tell
/// which tool returned which media.
fn render_block(block: &McpBlock, tool_name: &str) -> String {
    match block {
        McpBlock::Text(text) => text.clone(),
        McpBlock::Media { kind, mime } => {
            format!(
                "[Tool '{tool_name}' provided the following {kind} data with mime-type: {mime}]"
            )
        }
        // A text resource yields its text; a blob (no text) yields a placeholder
        // line with the mime, since Suspenders carries no inline data.
        McpBlock::EmbeddedResource { text, mime } => match text {
            Some(text) => text.clone(),
            None => {
                let mime = mime.as_deref().unwrap_or("application/octet-stream");
                format!(
                    "[Tool '{tool_name}' provided the following embedded resource with mime-type: {mime}]"
                )
            }
        },
        McpBlock::ResourceLink { label, uri } => format!("Resource Link: {label} at {uri}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(content: Vec<McpBlock>) -> McpCallResult {
        McpCallResult {
            content,
            is_error: false,
        }
    }

    #[test]
    fn text_block_is_verbatim() {
        assert_eq!(
            render(&ok(vec![McpBlock::Text("hello".into())]), "read_file"),
            Ok("hello".to_string())
        );
    }

    #[test]
    fn media_block_is_a_placeholder_line_naming_the_tool() {
        let result = ok(vec![McpBlock::Media {
            kind: "image".into(),
            mime: "image/png".into(),
        }]);
        assert_eq!(
            render(&result, "screenshot"),
            Ok(
                "[Tool 'screenshot' provided the following image data with mime-type: image/png]"
                    .to_string()
            )
        );
    }

    #[test]
    fn embedded_text_resource_yields_its_text() {
        let result = ok(vec![McpBlock::EmbeddedResource {
            text: Some("file body".into()),
            mime: Some("text/plain".into()),
        }]);
        assert_eq!(render(&result, "read_file"), Ok("file body".to_string()));
    }

    #[test]
    fn embedded_blob_resource_is_a_placeholder_line_naming_the_tool() {
        let result = ok(vec![McpBlock::EmbeddedResource {
            text: None,
            mime: Some("application/pdf".into()),
        }]);
        assert_eq!(
            render(&result, "fetch"),
            Ok(
                "[Tool 'fetch' provided the following embedded resource with mime-type: application/pdf]"
                    .to_string()
            )
        );
    }

    #[test]
    fn resource_link_is_a_labelled_line() {
        let result = ok(vec![McpBlock::ResourceLink {
            label: "report".into(),
            uri: "file:///r.txt".into(),
        }]);
        assert_eq!(
            render(&result, "list"),
            Ok("Resource Link: report at file:///r.txt".to_string())
        );
    }

    #[test]
    fn parts_join_on_newlines() {
        let result = ok(vec![
            McpBlock::Text("line one".into()),
            McpBlock::Text("line two".into()),
        ]);
        assert_eq!(
            render(&result, "tool"),
            Ok("line one\nline two".to_string())
        );
    }

    #[test]
    fn empty_result_is_the_empty_string() {
        assert_eq!(render(&ok(vec![]), "tool"), Ok(String::new()));
    }

    #[test]
    fn an_error_result_comes_back_err() {
        let result = McpCallResult {
            content: vec![McpBlock::Text("boom".into())],
            is_error: true,
        };
        assert_eq!(render(&result, "tool"), Err("boom".to_string()));
    }

    #[test]
    fn an_empty_error_result_gets_a_non_empty_fallback() {
        let result = McpCallResult {
            content: vec![],
            is_error: true,
        };
        assert_eq!(
            render(&result, "tool"),
            Err("the MCP tool reported an error with no message".to_string())
        );
    }
}
