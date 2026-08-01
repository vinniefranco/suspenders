//! The read model the `/mcp` management dialog draws (ADR-0065): a
//! transport-free snapshot of each Managed MCP Server and its discovered tools,
//! built at attach (and rebuilt by the live operations) and held by the Agent.
//!
//! The dialog is pure (ratatui/crossterm-free, ADR-0001) and never touches the
//! manager or the rmcp crate; it reads [`McpServerView`]s. The manager builds
//! them - the annotations and validity here mirror qwen's `MCPServerDisplayInfo`
//! / `MCPToolDisplayInfo`, adapted to Suspenders' vocabulary.

use serde_json::Value;

use crate::mcp::config::McpTransport;

/// The four behavioural hints an MCP server may attach to a tool (the MCP
/// `annotations` block). Each is a plain `bool` - an absent hint is `false`, the
/// hint not asserted (qwen reads them the same way). The dialog's TOOL_LIST and
/// TOOL_DETAIL steps colour a tag per asserted hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpToolAnnotations {
    /// The tool does not modify its environment.
    pub read_only: bool,
    /// The tool may perform destructive updates (meaningful only when not
    /// read-only).
    pub destructive: bool,
    /// Repeated calls with the same arguments have no additional effect.
    pub idempotent: bool,
    /// The tool interacts with an open world of external entities.
    pub open_world: bool,
}

impl McpToolAnnotations {
    /// Whether any hint is asserted - the dialog shows the annotation tags only
    /// when at least one is set (an all-`false` set renders nothing).
    pub fn any(self) -> bool {
        self.read_only || self.destructive || self.idempotent || self.open_world
    }
}

/// A Managed MCP Server's connection status (qwen `MCPServerStatus`). `Connecting`
/// is the transient attach/reconnect state; `Connected` means the peer handshook
/// and its tools listed; `Disconnected` means the attach failed or the link was
/// lost. The dialog and the footer health pill read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerStatus {
    /// Mid-handshake (attach or reconnect in flight).
    Connecting,
    /// Handshook and tools listed - ready to call.
    Connected,
    /// Attach failed, or the link dropped.
    Disconnected,
}

impl McpServerStatus {
    /// The status glyph qwen draws (`utils.getStatusIcon`): `✓` connected, `…`
    /// connecting, `✗` disconnected.
    pub fn icon(self) -> char {
        match self {
            McpServerStatus::Connected => '✓',
            McpServerStatus::Connecting => '…',
            McpServerStatus::Disconnected => '✗',
        }
    }

    /// The lowercase status word the dialog prints beside the glyph
    /// (`t(server.status)`): `connected` / `connecting` / `failed`. qwen's
    /// `STATUS_TEXT` maps `disconnected` to `failed`, which is the word shown.
    pub fn label(self) -> &'static str {
        match self {
            McpServerStatus::Connected => "connected",
            McpServerStatus::Connecting => "connecting",
            McpServerStatus::Disconnected => "failed",
        }
    }
}

/// Which settings scope declared a server (qwen `source`): the lowest scope that
/// names it (workspace shadows user), or an extension. Drives the SERVER_LIST
/// grouping (`User MCPs` / `Project MCPs` / `Extension MCPs`) and the
/// SERVER_DETAIL `Source:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSource {
    /// Declared in the user (global) config.
    User,
    /// Declared in the workspace (project-local) config.
    Workspace,
    /// Contributed by an extension.
    Extension,
}

/// One discovered MCP tool as the dialog sees it (qwen `MCPToolDisplayInfo`): the
/// bare server tool name, its description, its [`McpToolAnnotations`], and its
/// input schema (rendered as the parameter list in TOOL_DETAIL). Validity is
/// derived, not stored (qwen `isToolValid`): a tool the model can call has both a
/// non-empty name and a non-empty description.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolView {
    /// The bare server tool name (not the `mcp__…` wire name).
    pub name: String,
    /// The tool description; empty when the server reported none.
    pub description: String,
    /// The behavioural hints the server attached.
    pub annotations: McpToolAnnotations,
    /// The tool's JSON-Schema input, rendered as the TOOL_DETAIL parameter list.
    pub input_schema: Value,
}

impl McpToolView {
    /// Whether the model can call this tool (qwen `isToolValid`): both a name and
    /// a description are present. An invalid tool still lists in TOOL_LIST, tagged
    /// with its [`invalid_reasons`](McpToolView::invalid_reasons).
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty() && !self.description.is_empty()
    }

    /// The missing fields that make a tool invalid (qwen `getToolInvalidReasons`),
    /// in the order qwen lists them: `missing name`, then `missing description`.
    /// Empty when the tool is valid.
    pub fn invalid_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.name.is_empty() {
            reasons.push("missing name");
        }
        if self.description.is_empty() {
            reasons.push("missing description");
        }
        reasons
    }
}

/// One Managed MCP Server as the dialog sees it (qwen `MCPServerDisplayInfo`):
/// its name, [`status`](McpServerView::status), [`source`](McpServerView::source)
/// scope, a display of its transport, its discovered [`tools`](McpServerView::tools),
/// and the flags the SERVER_DETAIL actions key off (`is_disabled`,
/// `has_oauth_tokens`, `trust`). A failed attach carries its reason in
/// [`error`](McpServerView::error).
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerView {
    /// The user-chosen server name (the `mcp_servers` map key).
    pub name: String,
    /// The connection status.
    pub status: McpServerStatus,
    /// The settings scope that declared the server.
    pub source: McpSource,
    /// The transport rendered for the `Command:` line, e.g.
    /// `python -m srv (stdio)` or `https://host (http)` (qwen `formatServerCommand`).
    pub transport_display: String,
    /// The stdio working directory, when the config set one (the
    /// `Working Directory:` line; absent for HTTP or an unset cwd).
    pub cwd: Option<String>,
    /// Whether the user marked the server trusted (`trust: true`).
    pub trust: bool,
    /// The server's discovered, admitted tools (already include/exclude
    /// filtered).
    pub tools: Vec<McpToolView>,
    /// Whether the server is disabled (named in an `mcp.excluded` list).
    pub is_disabled: bool,
    /// Whether stored OAuth tokens exist for the server (gates the
    /// `Clear Authentication` / `Re-authenticate` actions).
    pub has_oauth_tokens: bool,
    /// The failure reason when the attach failed (the `Error:` line); `None` for
    /// a connected server.
    pub error: Option<String>,
}

impl McpServerView {
    /// The count of discovered tools (the `Tools:` line and the TOOL_LIST
    /// header).
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// The count of invalid tools (missing name or description) - the
    /// `N invalid tools` warning in SERVER_LIST and the `(N invalid)` suffix on
    /// the `Tools:` line.
    pub fn invalid_tool_count(&self) -> usize {
        self.tools.iter().filter(|t| !t.is_valid()).count()
    }
}

/// The footer MCP-health count (ADR-0065 Phase F, qwen `MCPHealthPill`'s
/// `disconnectedCount`): the servers that are `Disconnected` AND not disabled. A
/// `Connecting` server is deliberately NOT counted (qwen suppresses it to avoid
/// boot/reconnect flicker), nor is a disabled one (the operator turned it off).
/// The pill shows `N MCP{s} offline` when this is positive, hidden at zero.
pub fn mcp_offline_count(servers: &[McpServerView]) -> usize {
    servers
        .iter()
        .filter(|s| s.status == McpServerStatus::Disconnected && !s.is_disabled)
        .count()
}

/// Renders a transport for the SERVER_DETAIL `Command:` line, matching qwen's
/// `formatServerCommand`: an HTTP server shows `<url> (http)`; a stdio server
/// shows `<command> <args...> (stdio)` with the args joined by spaces and the
/// trailing space trimmed when there are none.
pub fn format_transport(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Http { url, .. } => format!("{url} (http)"),
        McpTransport::Stdio { command, args, .. } => {
            // qwen joins `${command} ${args} (stdio)` and trims; with no args that
            // leaves a stray double space. Build the parts cleanly instead so the
            // line is single-spaced whether or not there are args.
            let mut parts = Vec::with_capacity(args.len() + 1);
            parts.push(command.as_str());
            parts.extend(args.iter().map(String::as_str));
            format!("{} (stdio)", parts.join(" "))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/mcp/view.rs"]
mod tests;
