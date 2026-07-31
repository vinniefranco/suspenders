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
#[path = "../../tests/unit/mcp/result.rs"]
mod tests;
