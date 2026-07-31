//! [`McpManager`] - the fail-open connect + discovery front for the MCP
//! subsystem, and [`RmcpConn`], the ONE place the `rmcp` wire crate is touched
//! (ADR-0056).
//!
//! [`McpManager::connect`] walks the Session's `mcp_servers` map and attaches
//! each server on its own. A server that will not resolve its transport, will
//! not connect, or will not list its tools is recorded as a `(server, reason)`
//! failure and skipped - the Agent's built-in tools and its other MCP servers
//! carry on (fail-open, qwen's mcp-client-manager). A successful server's
//! admitted tools become [`crate::mcp::adapter::McpTool`]s over a shared
//! [`RmcpConn`], which owns the live rmcp service so the transport worker stays
//! alive for the Session.
//!
//! Everything rmcp lives below this line: the transport construction, the
//! `serve` handshake, `list_tools`, and the `CallToolResult` decode. Nothing
//! else in the crate imports rmcp - the rest of the subsystem works against the
//! [`McpConn`] seam.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::mcp::config::{McpServerConfig, McpTransport};
use crate::mcp::{McpBlock, McpCallResult, McpConn, McpError};
use crate::tool::Tool;

// ---- rmcp imports, CONFINED to this module ---------------------------------
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

/// The default per-server connect timeout for a stdio server (its process may
/// need to boot).
const DEFAULT_STDIO_TIMEOUT_MS: u64 = 30_000;

/// The default per-server connect timeout for an HTTP server.
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 5_000;

/// The attached MCP servers plus the per-server connect failures. The Agent
/// holds one for the Session's lifetime (the [`conns`](McpManager) keep each
/// server's rmcp service alive); [`failures`](McpManager::failures) feeds the
/// per-server launch notices the Agent emits after connect.
///
/// The derived [`Default`] (empty connections, empty failures) is the same shape
/// [`McpManager::connect`] yields for an empty server map - a test that needs an
/// [`crate::agent`] state without attaching any MCP server reaches for it.
#[derive(Default)]
pub struct McpManager {
    /// The live connections, one per successfully-attached server. Held ONLY to
    /// keep the underlying rmcp service (and its transport worker) alive for the
    /// Session's lifetime - each McpTool holds its own `Arc` clone to actually
    /// call. Never read outside tests, hence the allow.
    #[allow(dead_code)]
    conns: Vec<Arc<dyn McpConn>>,
    /// The `(server, reason)` failures recorded during connect - a malformed
    /// transport, a failed handshake, a failed discovery. Fail-open: each is a
    /// skip, never a crash.
    failures: Vec<(String, String)>,
}

impl McpManager {
    /// Attaches every configured MCP server, fail-open, and returns the manager
    /// plus the discovered [`McpTool`](crate::mcp::adapter::McpTool)s (already
    /// filtered by each server's include/exclude). A server that cannot resolve
    /// its transport, connect, or list its tools is recorded as a failure and
    /// skipped. An empty map yields an empty manager and no tools.
    pub async fn connect(
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> (McpManager, Vec<Box<dyn Tool>>) {
        // Connect every server CONCURRENTLY: N dead servers no longer stack their
        // timeouts (N x 30s serially would block the Agent actor before it could
        // serve a single message). Each `connect_one` is bounded by its own
        // timeout, so the wall-clock cost collapses from N timeouts to ~1.
        let attached = futures_util::future::join_all(
            servers
                .iter()
                .map(|(name, cfg)| async move { (name.as_str(), connect_one(name, cfg).await) }),
        )
        .await;

        // Reassemble DETERMINISTICALLY: `join_all` preserves input (server-name-
        // sorted, from the BTreeMap) order regardless of which server's handshake
        // finished first, so the tool set + failure list are stable across runs.
        // (An explicit BTreeMap keyed by name would give the same order; the
        // preserved order is used directly.)
        let (conns, tools, failures) = assemble(attached);
        (McpManager { conns, failures }, tools)
    }

    /// The per-server connect failures (`(server, reason)`), server-name-sorted.
    /// The Agent surfaces one launch notice per entry after connect (see
    /// `init_agent`). Fail-open means a broken server is here, not a crash.
    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }

    /// The count of live connections, for tests + diagnostics.
    #[cfg(test)]
    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }
}

/// One server's successful attach: its shared conn plus its admitted tools.
type ServerAttach = (Arc<dyn McpConn>, Vec<Box<dyn Tool>>);

/// One server's connect outcome, paired with its name so the assembly stays
/// keyed even though the concurrent connects finish out of order.
type Attached<'a> = (&'a str, Result<ServerAttach, String>);

/// The assembled manager parts: the live conns, the flattened tool set, and the
/// `(server, reason)` failures.
type Assembled = (
    Vec<Arc<dyn McpConn>>,
    Vec<Box<dyn Tool>>,
    Vec<(String, String)>,
);

/// Folds the per-server connect outcomes into the manager's parts, IN THE ORDER
/// GIVEN (the caller hands them server-name-sorted, so the tool set + failure
/// list are stable across runs regardless of completion order). A `Ok` server
/// contributes its conn + tools; an `Err` server contributes a `(name, reason)`
/// failure and nothing else (fail-open).
fn assemble(attached: Vec<Attached<'_>>) -> Assembled {
    let mut conns: Vec<Arc<dyn McpConn>> = Vec::new();
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for (name, outcome) in attached {
        match outcome {
            Ok((conn, server_tools)) => {
                conns.push(conn);
                tools.extend(server_tools);
            }
            Err(reason) => failures.push((name.to_string(), reason)),
        }
    }

    (conns, tools, failures)
}

/// Attaches one server: resolve the transport, connect (bounded by the server's
/// timeout), list tools, and build an [`McpTool`](crate::mcp::adapter::McpTool)
/// over a shared [`RmcpConn`] for each admitted tool. Any await error is an
/// `Err(reason)` the caller records + skips (fail-open).
async fn connect_one(name: &str, cfg: &McpServerConfig) -> Result<ServerAttach, String> {
    let transport = cfg.transport()?;
    let default_timeout = match &transport {
        McpTransport::Stdio { .. } => DEFAULT_STDIO_TIMEOUT_MS,
        McpTransport::Http { .. } => DEFAULT_HTTP_TIMEOUT_MS,
    };
    let timeout = Duration::from_millis(cfg.timeout_ms.unwrap_or(default_timeout));

    // The handshake is bounded per server: a stuck server times out into a
    // recorded failure rather than hanging the Agent's startup.
    let service = tokio::time::timeout(timeout, serve(transport))
        .await
        .map_err(|_| format!("connect timed out after {}ms", timeout.as_millis()))?
        .map_err(|e| format!("connect failed: {e}"))?;

    // `list_all_tools` loops over `next_cursor` so a paginated server's later
    // pages are not silently dropped (plain `list_tools` returns only the first).
    // It returns `Result<Vec<Tool>, _>` directly - no `.tools` field. Still
    // bounded by the per-server timeout.
    let listed = tokio::time::timeout(timeout, service.peer().list_all_tools())
        .await
        .map_err(|_| "list_tools timed out".to_string())?
        .map_err(|e| format!("list_tools failed: {e}"))?;

    // The RmcpConn owns the running service so its transport worker stays alive
    // for the Session; every McpTool for this server shares the one Arc.
    let conn: Arc<RmcpConn> = Arc::new(RmcpConn {
        peer: service.peer().clone(),
        _service: service,
    });

    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for tool in listed {
        let tool_name = tool.name.to_string();
        if !cfg.admits(&tool_name) {
            continue;
        }
        let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
        let input_schema = Value::Object((*tool.input_schema).clone());
        let mcp_tool = crate::mcp::adapter::McpTool::new(
            crate::mcp::adapter::McpToolInfo::new(name, tool_name, description, input_schema),
            Arc::clone(&conn) as Arc<dyn McpConn>,
            cfg.timeout_ms,
        );
        tools.push(Box::new(mcp_tool));
    }

    Ok((conn as Arc<dyn McpConn>, tools))
}

/// Builds the concrete rmcp transport and runs the client handshake. The `()`
/// client handler is the default no-op handler - Suspenders is a pure MCP
/// client (it consumes tools, it does not serve any).
async fn serve(
    transport: McpTransport,
) -> Result<RunningService<RoleClient, ()>, Box<dyn std::error::Error + Send + Sync>> {
    match transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            for (key, value) in env {
                cmd.env(key, value);
            }
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            // rmcp's `TokioChildProcess::new` inherits child stderr, so a stdio
            // server logging to stderr would write straight onto the ratatui
            // screen. Null it via the builder; stdin/stdout stay piped (the
            // transport needs them). `.spawn()` yields `(proc, stderr_handle)` -
            // the handle is `None` under a null stderr, so it is discarded.
            let (child, _stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::null())
                .spawn()?;
            Ok(().serve(child).await?)
        }
        McpTransport::Http { url, headers } => {
            let mut custom = std::collections::HashMap::new();
            for (key, value) in headers {
                let name = http::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|e| format!("bad header name {key:?}: {e}"))?;
                let val = http::HeaderValue::from_str(&value)
                    .map_err(|e| format!("bad header value for {key:?}: {e}"))?;
                custom.insert(name, val);
            }
            let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
            let transport = StreamableHttpClientTransport::from_config(config);
            Ok(().serve(transport).await?)
        }
    }
}

/// The production [`McpConn`]: a live rmcp client peer. This is the sole place
/// the wire crate's `CallToolResult` is decoded into the transport-free
/// [`McpCallResult`]. It owns the [`RunningService`] so the transport worker
/// outlives every call.
struct RmcpConn {
    peer: Peer<RoleClient>,
    /// Kept solely to hold the rmcp service (and its transport worker) alive for
    /// the Session; never read after construction.
    _service: RunningService<RoleClient, ()>,
}

#[async_trait::async_trait]
impl McpConn for RmcpConn {
    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<McpCallResult, McpError> {
        // The arguments must be a JSON object on the wire; a non-object (or
        // absent) input becomes no arguments.
        let arguments = match arguments {
            Value::Object(map) => Some(map),
            _ => None,
        };
        // `CallToolRequestParams` is `#[non_exhaustive]`, so it is built from
        // its `Default` and the two fields we set (rather than a struct literal).
        let mut params = CallToolRequestParams::default();
        params.name = tool.to_string().into();
        params.arguments = arguments;
        let result = self
            .peer
            .call_tool(params)
            .await
            .map_err(|e| McpError(format!("MCP call_tool failed: {e}")))?;
        Ok(decode(result))
    }
}

/// Decodes rmcp's `CallToolResult` into the transport-free [`McpCallResult`].
/// The one wire->value boundary; every content-block variant maps to an
/// [`McpBlock`] the rest of the subsystem understands.
fn decode(result: CallToolResult) -> McpCallResult {
    let content = result.content.into_iter().map(decode_block).collect();
    McpCallResult {
        content,
        is_error: result.is_error.unwrap_or(false),
    }
}

/// One rmcp `ContentBlock` to an [`McpBlock`]. Media and blob resources keep
/// only their descriptor (ADR-0056: no inline data).
fn decode_block(block: ContentBlock) -> McpBlock {
    match block {
        ContentBlock::Text(text) => McpBlock::Text(text.text),
        ContentBlock::Image(image) => McpBlock::Media {
            kind: "image".to_string(),
            mime: image.mime_type,
        },
        ContentBlock::Audio(audio) => McpBlock::Media {
            kind: "audio".to_string(),
            mime: audio.mime_type,
        },
        ContentBlock::Resource(embedded) => match embedded.resource {
            ResourceContents::TextResourceContents {
                text, mime_type, ..
            } => McpBlock::EmbeddedResource {
                text: Some(text),
                mime: mime_type,
            },
            ResourceContents::BlobResourceContents { mime_type, .. } => {
                McpBlock::EmbeddedResource {
                    text: None,
                    mime: mime_type,
                }
            }
            // `ResourceContents` is `#[non_exhaustive]`; a future variant becomes
            // a bare placeholder rather than failing the decode.
            _ => McpBlock::EmbeddedResource {
                text: None,
                mime: None,
            },
        },
        ContentBlock::ResourceLink(resource) => McpBlock::ResourceLink {
            // A resource link's display label is its title when present, else
            // its programmatic name (qwen: `title || name`).
            label: resource.title.unwrap_or(resource.name),
            uri: resource.uri,
        },
        // `ContentBlock` is `#[non_exhaustive]`; a future block kind collapses to
        // an empty text line rather than failing the decode.
        _ => McpBlock::Text(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_map_yields_an_empty_manager_and_no_tools() {
        let (manager, tools) = McpManager::connect(&BTreeMap::new()).await;
        assert_eq!(manager.conn_count(), 0);
        assert!(manager.failures().is_empty());
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn a_config_invalid_server_is_recorded_and_skipped_without_touching_rmcp() {
        // Both transports set => `transport()` errs BEFORE any rmcp code runs.
        // The server is recorded as a failure and skipped; no connection is made.
        let mut servers = BTreeMap::new();
        servers.insert(
            "broken".to_string(),
            McpServerConfig {
                command: Some("cmd".into()),
                http_url: Some("https://x.test".into()),
                ..Default::default()
            },
        );
        let (manager, tools) = McpManager::connect(&servers).await;
        assert_eq!(manager.conn_count(), 0);
        assert!(tools.is_empty());
        assert_eq!(manager.failures().len(), 1);
        assert_eq!(manager.failures()[0].0, "broken");
        assert!(manager.failures()[0].1.contains("both"));
    }

    /// A tiny [`McpConn`] the assembly test can hand a real (if inert) conn, so
    /// two `Ok` servers each contribute a conn + a tool without a live server.
    struct StubConn;

    #[async_trait::async_trait]
    impl McpConn for StubConn {
        async fn call_tool(
            &self,
            _tool: &str,
            _arguments: Value,
        ) -> Result<McpCallResult, McpError> {
            Ok(McpCallResult {
                content: vec![],
                is_error: false,
            })
        }
    }

    fn ok_server(server: &str, tool: &str) -> Result<ServerAttach, String> {
        let conn: Arc<dyn McpConn> = Arc::new(StubConn);
        let mcp_tool = crate::mcp::adapter::McpTool::new(
            crate::mcp::adapter::McpToolInfo::new(
                server,
                tool,
                String::new(),
                Value::Object(Default::default()),
            ),
            Arc::clone(&conn),
            None,
        );
        Ok((conn, vec![Box::new(mcp_tool)]))
    }

    #[test]
    fn assemble_lets_two_ok_servers_both_contribute_a_conn_and_tools() {
        let attached = vec![
            ("alpha", ok_server("alpha", "one")),
            ("beta", ok_server("beta", "two")),
        ];
        let (conns, tools, failures) = assemble(attached);
        assert_eq!(conns.len(), 2);
        assert_eq!(tools.len(), 2);
        assert!(failures.is_empty());
        // Deterministic assembly: the tools land in the input (server-name-sorted)
        // order, NOT completion order.
        assert_eq!(tools[0].spec().name, "mcp__alpha__one");
        assert_eq!(tools[1].spec().name, "mcp__beta__two");
    }

    #[test]
    fn assemble_records_an_err_server_as_a_failure_and_keeps_the_ok_one() {
        // Input order is server-name-sorted; the failure list preserves it, so an
        // Err between two Oks lands deterministically keyed by name.
        let attached = vec![
            ("alpha", ok_server("alpha", "one")),
            ("beta", Err("boom".to_string())),
            ("gamma", ok_server("gamma", "three")),
        ];
        let (conns, tools, failures) = assemble(attached);
        assert_eq!(conns.len(), 2);
        assert_eq!(tools.len(), 2);
        assert_eq!(failures, vec![("beta".to_string(), "boom".to_string())]);
    }
}
