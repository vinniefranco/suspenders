//! The TOOL_DETAIL step render (ADR-0065 Phase E, qwen's `ToolDetailStep`): one
//! tool's annotated name header, its invalid warning (when invalid), its
//! Description, and its Parameters from the JSON-Schema input. One pure builder
//! over the [`McpToolView`] read model (ADR-0001/0019);
//! [`super::McpDialog::view`] calls [`tool_detail_view`].

use crate::mcp::McpToolView;

use super::row::{McpDialogView, McpRow, McpSpan, McpStyle, back_footer};

/// The TOOL_DETAIL step (qwen `ToolDetailStep`): the tool name header (with
/// annotation tags), the invalid warning (when invalid), the Description, and
/// the Parameters from the input schema.
pub(super) fn tool_detail_view(tool: &McpToolView) -> McpDialogView {
    McpDialogView {
        header: tool_detail_header(tool),
        content: tool_detail_content(tool),
        footer: back_footer(),
    }
}

/// The TOOL_DETAIL header (qwen `renderStepHeader` TOOL_DETAIL): the tool name +
/// a bracketed tag per asserted annotation, each in qwen's colour (destructive
/// red, idempotent yellow, read-only green, open-world primary), then the server
/// name - here the tool's own name is the header, the tags follow it.
fn tool_detail_header(tool: &McpToolView) -> Vec<McpRow> {
    let mut spans = vec![McpSpan::bold(McpStyle::Accent, tool.name.clone())];
    let a = tool.annotations;
    if a.destructive {
        spans.push(McpSpan::new(McpStyle::Error, " [destructive]"));
    }
    if a.idempotent {
        spans.push(McpSpan::new(McpStyle::Warning, " [idempotent]"));
    }
    if a.read_only {
        spans.push(McpSpan::new(McpStyle::Success, " [read-only]"));
    }
    if a.open_world {
        spans.push(McpSpan::new(McpStyle::Primary, " [open-world]"));
    }
    vec![McpRow::new(spans)]
}

/// The TOOL_DETAIL content (qwen `ToolDetailStep`): the invalid warning (when
/// invalid), the Description, and the Parameters list from the input schema.
fn tool_detail_content(tool: &McpToolView) -> Vec<McpRow> {
    let mut rows = Vec::new();
    if !tool.is_valid() {
        rows.push(McpRow::bold_styled(
            McpStyle::Error,
            "Warning: This tool cannot be called by the LLM",
        ));
        rows.push(McpRow::styled(
            McpStyle::Error,
            format!("Reason: {}", tool.invalid_reasons().join(", ")),
        ));
        rows.push(McpRow::styled(
            McpStyle::Secondary,
            "Tools must have both name and description to be used by the LLM.",
        ));
        rows.push(McpRow::blank());
    }
    if !tool.description.is_empty() {
        rows.push(McpRow::bold_styled(McpStyle::Primary, "Description:"));
        rows.push(McpRow::styled(McpStyle::Primary, tool.description.clone()));
    }
    let params = parameter_rows(&tool.input_schema);
    if !params.is_empty() {
        if !rows.is_empty() {
            rows.push(McpRow::blank());
        }
        rows.push(McpRow::bold_styled(McpStyle::Primary, "Parameters:"));
        rows.extend(params);
    }
    rows
}

/// The TOOL_DETAIL parameter rows (qwen `SchemaSummary` / `ParametersList`): one
/// `• name(required): type - description` line per JSON-Schema property, `type`
/// defaulting to `any` and the ` - description` omitted when there is none. An
/// absent/empty `properties` yields no rows.
fn parameter_rows(schema: &serde_json::Value) -> Vec<McpRow> {
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    properties
        .iter()
        .map(|(name, param)| {
            let is_required = required.contains(&name.as_str());
            McpRow::styled(
                McpStyle::Secondary,
                parameter_line(name, param, is_required),
            )
        })
        .collect()
}

/// One parameter's `• name(required): type - description` line (qwen
/// `renderParameter`): the `(required)` suffix only when required, the `type`
/// defaulting to `any`, and the ` - description` only when the schema names one.
fn parameter_line(name: &str, param: &serde_json::Value, required: bool) -> String {
    let ty = param.get("type").and_then(|t| t.as_str()).unwrap_or("any");
    let description = param
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let required = if required { "(required)" } else { "" };
    let trailer = if description.is_empty() {
        String::new()
    } else {
        format!(" - {description}")
    };
    format!("• {name}{required}: {ty}{trailer}")
}

#[cfg(test)]
#[path = "../../../tests/ui/mcp_command/tool_detail.rs"]
mod tests;
