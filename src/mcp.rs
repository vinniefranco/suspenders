//! The MCP client subsystem: attach an external MCP server, discover its tools,
//! and expose each as a deferred Suspenders Tool the model reaches through
//! `tool_search` (F8, ADR-0056).
//!
//! ## The seam
//!
//! [`McpConn`] is the transport-free boundary a Tool Call reaches an MCP server
//! through: give it a tool name and its arguments, get back an [`McpCallResult`].
//! The one production impl is [`manager::RmcpConn`], which owns a live rmcp
//! service peer - the ONLY place the `rmcp` crate is touched. Everything else in
//! this module (the config, the result collapse, the [`adapter::McpTool`]) works
//! against the seam, so a fake conn drives the unit tests and the wire crate
//! stays confined.
//!
//! ## Fail-open
//!
//! A broken server never crashes startup. [`manager::McpManager::connect`]
//! attaches each server on its own, records a `(server, reason)` failure for any
//! that will not resolve/connect, and skips it - the Agent's other servers and
//! its built-in tools carry on. This mirrors qwen's mcp-client-manager: a bad
//! server is a recorded skip, not a launch failure.
//!
//! ## Naming + discovery
//!
//! A discovered tool takes the wire name `mcp__<server>__<tool>` (qwen's
//! convention, sanitized + elided by [`adapter::valid_name`]). MCP tools are ALL
//! deferred: they never ride the wire list the model sees at Run start, and are
//! surfaced on demand through `tool_search` (which weighs them slightly higher
//! because discovery is the only way the model reaches them). Their `is_mcp()`
//! is true, which the tool-search scorer reads.
//!
//! ## Out of scope (ADR-0056, narrowed by ADR-0065)
//!
//! MCP prompts + resources; SSE and websocket transports; MCP-call
//! approval-gating (the `trust` flag is parsed + stored, gates nothing); inline
//! multimodal tool-result data (media collapses to placeholder text per
//! ADR-0039). ADR-0065 brought OAuth (the [`oauth`] module: PKCE, discovery,
//! dynamic registration, token storage, Bearer injection at connect) and live
//! mid-session reconnect (the [`manager`] live ops) INTO scope; a dropped server
//! still surfaces as an `is_error` Tool Result until a reconnect re-attaches it.

pub mod adapter;
pub mod config;
pub mod manager;
pub mod oauth;
pub mod result;
pub mod view;

pub use config::{McpOAuthConfig, McpServerConfig, McpTransport};
pub use view::{
    McpServerStatus, McpServerView, McpSource, McpToolAnnotations, McpToolView, format_transport,
    mcp_offline_count,
};

use serde_json::Value;

/// The transport-free boundary a Tool Call reaches an MCP server through. The
/// [`adapter::McpTool`] holds one as `Arc<dyn McpConn>` and calls it; the
/// production impl ([`manager::RmcpConn`]) decodes rmcp's `CallToolResult` into
/// the [`McpCallResult`] value below, so nothing above this seam sees the wire
/// crate.
#[async_trait::async_trait]
pub trait McpConn: Send + Sync {
    /// Calls the remote tool by its server-side name with the model-supplied
    /// arguments, yielding the decoded result or a transport/protocol
    /// [`McpError`]. `tool` is the bare server tool name (not the `mcp__…`
    /// wire name).
    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<McpCallResult, McpError>;
}

/// A decoded MCP tool-call result: the server's content blocks and whether the
/// call reported an error. The transport-free mirror of rmcp's `CallToolResult`
/// (decoded in [`manager::RmcpConn`]); [`result::render`] collapses it to the
/// canonical text a Tool Result carries (ADR-0039).
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    pub content: Vec<McpBlock>,
    pub is_error: bool,
}

/// One MCP content block, transport-free. Media and blob resources carry only
/// their descriptor (kind + mime): the model sees a placeholder line, never the
/// raw bytes (multimodal inline data is out of scope for P1c, ADR-0056).
#[derive(Debug, Clone, PartialEq)]
pub enum McpBlock {
    /// A text block; its text enters the Tool Result verbatim.
    Text(String),
    /// An image or audio block. `kind` is `"image"` or `"audio"`; only the
    /// descriptor is kept, so the model sees a placeholder line.
    Media { kind: String, mime: String },
    /// An embedded resource. Text resources carry their text; a blob carries
    /// only its mime (a placeholder line, no bytes).
    EmbeddedResource {
        text: Option<String>,
        mime: Option<String>,
    },
    /// A resource link: a labelled pointer to a resource the model may fetch
    /// later (fetching is out of scope for P1c).
    ResourceLink { label: String, uri: String },
}

/// A transport/protocol failure from an MCP call (a dropped server, a decode
/// error, a timeout). The [`adapter::McpTool`] maps it to an `is_error` Tool
/// Result so a broken server never crashes the Run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpError(pub String);
