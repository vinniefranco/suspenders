//! The rmcp-confined connection layer for [`super::McpManager`] (ADR-0056): the
//! ONE place the `rmcp` wire crate is touched. Transport construction, the
//! `serve` handshake, `list_tools`, the OAuth Bearer resolution, and the
//! `CallToolResult` decode all live here; nothing else in the crate imports
//! rmcp. The manager above works against the [`McpConn`](crate::mcp::McpConn)
//! seam and the plain [`ServerAttach`] this module returns.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::mcp::config::{McpServerConfig, McpTransport};
use crate::mcp::view::{McpToolAnnotations, McpToolView};
use crate::mcp::{McpBlock, McpCallResult, McpConn, McpError};
use crate::tool::Tool;

// ---- rmcp imports, CONFINED to this module ---------------------------------
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientJsonRpcMessage, ContentBlock, ResourceContents,
    ServerJsonRpcMessage, ToolAnnotations,
};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::Transport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

/// The default per-server connect timeout for a stdio server (its process may
/// need to boot).
const DEFAULT_STDIO_TIMEOUT_MS: u64 = 30_000;

/// The default per-server connect timeout for an HTTP server.
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 5_000;

/// The default per-server connect timeout for a legacy HTTP+SSE server (same as
/// HTTP - both are a network handshake, not a process boot).
const DEFAULT_SSE_TIMEOUT_MS: u64 = 5_000;

/// One server's successful attach: its shared conn, its admitted tools, and the
/// matching [`McpToolView`] read model for each.
pub(super) type ServerAttach = (Arc<dyn McpConn>, Vec<Box<dyn Tool>>, Vec<McpToolView>);

/// Builds the [`McpTool`](crate::mcp::adapter::McpTool) boxes for one connected
/// server from its shared conn and discovered tool views (ADR-0065 Phase C). The
/// one place a tool view + conn become an executable box: `connect_one` calls it
/// at attach, and [`McpManager::adapters`](super::McpManager::adapters) calls it
/// to regenerate the set after a live op - so a rebuilt box is byte-identical to
/// the original.
pub(super) fn build_adapters(
    name: &str,
    conn: &Arc<dyn McpConn>,
    tool_views: &[McpToolView],
    timeout_ms: Option<u64>,
) -> Vec<Box<dyn Tool>> {
    tool_views
        .iter()
        .map(|view| {
            let mcp_tool = crate::mcp::adapter::McpTool::new(
                crate::mcp::adapter::McpToolInfo::new(
                    name,
                    view.name.clone(),
                    view.description.clone(),
                    view.input_schema.clone(),
                ),
                Arc::clone(conn),
                timeout_ms,
            );
            Box::new(mcp_tool) as Box<dyn Tool>
        })
        .collect()
}

/// Maps rmcp's optional [`ToolAnnotations`] to the dialog's flat
/// [`McpToolAnnotations`]: an absent block, or an absent hint within it, reads as
/// `false` (the hint not asserted).
fn annotations_of(annotations: &Option<ToolAnnotations>) -> McpToolAnnotations {
    match annotations {
        Some(a) => McpToolAnnotations {
            read_only: a.read_only_hint.unwrap_or(false),
            destructive: a.destructive_hint.unwrap_or(false),
            idempotent: a.idempotent_hint.unwrap_or(false),
            open_world: a.open_world_hint.unwrap_or(false),
        },
        None => McpToolAnnotations::default(),
    }
}

/// Attaches one server: resolve the transport, connect (bounded by the server's
/// timeout), list tools, and build an [`McpTool`](crate::mcp::adapter::McpTool)
/// over a shared [`RmcpConn`] for each admitted tool. Any await error is an
/// `Err(reason)` the caller records + skips (fail-open).
///
/// `oauth_tokens_path` is the token store to resolve a Bearer for an OAuth-enabled
/// server (ADR-0065 Phase D): a stored+valid token is injected at connect (an
/// `Authorization: Bearer` header for HTTP, or the SSE `token_param_name` query),
/// and a connect that fails with the token in hand triggers one forced
/// refresh-then-retry. A server with no OAuth (or no stored token) connects
/// exactly as before.
pub(super) async fn connect_one(
    name: &str,
    cfg: &McpServerConfig,
    oauth_tokens_path: Option<&str>,
) -> Result<ServerAttach, String> {
    let default_timeout = match &cfg.transport {
        McpTransport::Stdio { .. } => DEFAULT_STDIO_TIMEOUT_MS,
        McpTransport::Http { .. } => DEFAULT_HTTP_TIMEOUT_MS,
        McpTransport::Sse { .. } => DEFAULT_SSE_TIMEOUT_MS,
    };
    let timeout = Duration::from_millis(cfg.timeout_ms.unwrap_or(default_timeout));

    // Resolve the OAuth Bearer for an enabled server (proactively refreshing an
    // expired token), else `None` (no OAuth or no stored token). The resolution
    // is impure (disk + possible network) but transport-free, so it stays above
    // the rmcp seam (it lives in `mcp::oauth`).
    let mut auth = resolve_oauth(name, cfg, oauth_tokens_path).await;

    // The handshake is bounded per server: a stuck server times out into a
    // recorded failure rather than hanging the Agent's startup. An OAuth server
    // whose connect fails with a token in hand gets ONE forced-refresh retry (the
    // token may have been revoked server-side between the expiry check and now).
    let service = match tokio::time::timeout(timeout, serve(cfg.transport.clone(), &auth)).await {
        Ok(Ok(service)) => service,
        outcome => {
            let first_error = match outcome {
                Ok(Err(e)) => format!("connect failed: {e}"),
                _ => format!("connect timed out after {}ms", timeout.as_millis()),
            };
            // Retry once with a force-refreshed token, but only when we HAD one to
            // begin with (a plain unauthenticated failure is just a failure).
            let Some(refreshed) = force_refresh_oauth(name, cfg, oauth_tokens_path).await else {
                return Err(first_error);
            };
            auth = Some(refreshed);
            tokio::time::timeout(timeout, serve(cfg.transport.clone(), &auth))
                .await
                .map_err(|_| format!("connect timed out after {}ms", timeout.as_millis()))?
                .map_err(|e| format!("connect failed after token refresh: {e}"))?
        }
    };

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

    // The view read model captures what the dialog draws (annotations, validity,
    // schema) off each admitted rmcp tool definition; the executable McpTool boxes
    // are then built FROM the views (via `build_adapters`), so the manager can
    // rebuild the same boxes later over the retained conn + views without a fresh
    // connect (ADR-0065 Phase C).
    let tool_views: Vec<McpToolView> = listed
        .into_iter()
        .filter(|tool| cfg.admits(tool.name.as_ref()))
        .map(|tool| McpToolView {
            name: tool.name.to_string(),
            description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
            annotations: annotations_of(&tool.annotations),
            input_schema: Value::Object((*tool.input_schema).clone()),
        })
        .collect();

    let conn = conn as Arc<dyn McpConn>;
    let tools = build_adapters(name, &conn, &tool_views, cfg.timeout_ms);
    Ok((conn, tools, tool_views))
}

/// A resolved OAuth Bearer for a connect (ADR-0065 Phase D): the access token and
/// how it rides the transport. An HTTP server carries it as an `Authorization:
/// Bearer` header; an SSE server with a `token_param_name` carries it as that
/// query parameter instead (qwen's `tokenParamName`). Transport-free (a bare
/// access token + placement), resolved in `mcp::oauth` above the rmcp seam.
struct ResolvedAuth {
    access_token: String,
    /// The SSE query-parameter name to carry the token as; `None` uses the
    /// `Authorization: Bearer` header (the HTTP case).
    token_param_name: Option<String>,
}

/// Resolves the OAuth Bearer for a server (ADR-0065 Phase D): `None` unless the
/// server has `oauth.enabled`, a token store is configured, and a stored (or
/// freshly refreshed) valid token exists. Proactively refreshes an expired token
/// through [`oauth::McpOAuthProvider::valid_token`]. Fail-soft: any storage /
/// refresh error yields `None`, so a token problem degrades to an unauthenticated
/// connect (which the server rejects with a clear failure) rather than a crash.
async fn resolve_oauth(
    name: &str,
    cfg: &McpServerConfig,
    oauth_tokens_path: Option<&str>,
) -> Option<ResolvedAuth> {
    let oauth = cfg.oauth.as_ref()?;
    if oauth.enabled != Some(true) {
        return None;
    }
    let path = oauth_tokens_path?;
    let storage = crate::mcp::oauth::McpOAuthTokenStorage::new(path);
    let provider = crate::mcp::oauth::McpOAuthProvider::new(storage);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let token = provider.valid_token(name, oauth, now).await.ok()??;
    Some(ResolvedAuth {
        access_token: token.access_token,
        token_param_name: oauth.token_param_name.clone(),
    })
}

/// Forces a token refresh and re-resolves the Bearer (ADR-0065 Phase D's
/// 401-retry leg): passing `now = u64::MAX` makes `valid_token` treat any stored
/// token as expired, so it refreshes through the stored refresh token. `None`
/// when there is no OAuth, no store, or no refresh path - in which case the
/// caller keeps its first connect error.
async fn force_refresh_oauth(
    name: &str,
    cfg: &McpServerConfig,
    oauth_tokens_path: Option<&str>,
) -> Option<ResolvedAuth> {
    let oauth = cfg.oauth.as_ref()?;
    if oauth.enabled != Some(true) {
        return None;
    }
    let path = oauth_tokens_path?;
    let storage = crate::mcp::oauth::McpOAuthTokenStorage::new(path);
    let provider = crate::mcp::oauth::McpOAuthProvider::new(storage);
    let token = provider.valid_token(name, oauth, u64::MAX).await.ok()??;
    Some(ResolvedAuth {
        access_token: token.access_token,
        token_param_name: oauth.token_param_name.clone(),
    })
}

/// Builds the concrete rmcp transport and runs the client handshake. The `()`
/// client handler is the default no-op handler - Suspenders is a pure MCP
/// client (it consumes tools, it does not serve any).
///
/// `auth` is the resolved OAuth Bearer (ADR-0065 Phase D), injected at connect for
/// an HTTP or SSE server: a `token_param_name` rides it as a query parameter (the
/// SSE shape), else it rides an `Authorization: Bearer` header. Stdio servers
/// ignore it (OAuth is an HTTP concern). `None` connects unauthenticated as
/// before. The SSE arm hand-rolls the legacy HTTP+SSE transport
/// ([`SseClientTransport`]) because rmcp 3.0.1 dropped its standalone SSE client.
async fn serve(
    transport: McpTransport,
    auth: &Option<ResolvedAuth>,
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
            let mut header_map = build_header_map(&headers)?;
            // Inject the OAuth Bearer (ADR-0065 Phase D): an SSE `token_param_name`
            // appends it as a query parameter on the URL, else it rides the
            // `Authorization: Bearer` header alongside the static headers.
            let url = match auth {
                Some(ResolvedAuth {
                    access_token,
                    token_param_name: Some(param),
                }) => append_query_param(&url, param, access_token),
                Some(ResolvedAuth { access_token, .. }) => {
                    let val = http::HeaderValue::from_str(&format!("Bearer {access_token}"))
                        .map_err(|e| format!("bad Bearer token: {e}"))?;
                    header_map.insert(http::header::AUTHORIZATION, val);
                    url
                }
                None => url,
            };
            // `custom_headers` wants a `HashMap<HeaderName, HeaderValue>`. A
            // `HeaderMap` iterates as `(Option<HeaderName>, _)` (a `None` repeats
            // the prior name for a multi-valued header); ours are single-valued,
            // built by the one shared builder, so unwrapping the name per entry
            // reconstructs that map with no parallel construction path.
            let custom: std::collections::HashMap<_, _> = header_map
                .into_iter()
                .filter_map(|(name, val)| name.map(|name| (name, val)))
                .collect();
            let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
            let transport = StreamableHttpClientTransport::from_config(config);
            Ok(().serve(transport).await?)
        }
        McpTransport::Sse { url, headers } => {
            // Legacy MCP HTTP+SSE: build the header map, fold in the OAuth Bearer
            // (a `token_param_name` rides the SSE GET url as a query param, else an
            // `Authorization: Bearer` header on both GET and POST), then open the
            // stream + read its `endpoint` event before handing the transport to
            // rmcp's `serve` handshake.
            let mut header_map = build_header_map(&headers)?;
            let url = match auth {
                Some(ResolvedAuth {
                    access_token,
                    token_param_name: Some(param),
                }) => append_query_param(&url, param, access_token),
                Some(ResolvedAuth { access_token, .. }) => {
                    let val = http::HeaderValue::from_str(&format!("Bearer {access_token}"))
                        .map_err(|e| format!("bad Bearer token: {e}"))?;
                    header_map.insert(http::header::AUTHORIZATION, val);
                    url
                }
                None => url,
            };
            let transport = SseClientTransport::connect(&url, header_map).await?;
            Ok(().serve(transport).await?)
        }
    }
}

/// A hand-rolled legacy MCP HTTP+SSE client transport (implements rmcp's
/// [`Transport<RoleClient>`]). WHY hand-rolled: rmcp 3.0.1 dropped its standalone
/// SSE client (only `TokioChildProcess` + `StreamableHttpClientTransport` ship in
/// its `transport` module), so the legacy HTTP+SSE protocol is implemented here,
/// inside the one rmcp-facing file, keeping the wire crate confined to this seam.
///
/// The protocol (qwen's `SSEClientTransport`):
/// 1. GET the SSE `url`; the server opens a `text/event-stream`.
/// 2. The first `endpoint` event's `data:` is the (possibly relative) URL to POST
///    JSON-RPC to; we resolve it against the SSE url and keep it for `send`.
/// 3. `send` POSTs a JSON-RPC message to that endpoint.
/// 4. `receive` yields the next `message` event's JSON off the still-open GET
///    stream, decoded into a [`ServerJsonRpcMessage`].
///
/// The custom headers ride BOTH the GET (opened in [`connect`](Self::connect))
/// and every POST. The SSE parsing reuses [`sse_stream::SseStream`] (rmcp's own
/// framer) over reqwest's byte stream, so we do not re-implement `event:`/`data:`
/// framing.
struct SseClientTransport {
    /// The shared reqwest client + custom headers + resolved POST endpoint,
    /// cloned into each `send` future so the future is `'static` (the trait
    /// requires it, since sends may run concurrently). Behind an `Arc` so a clone
    /// is a refcount bump, not a header-map copy.
    post: Arc<PostContext>,
    /// The open SSE GET stream, decoded to `message`-event JSON-RPC. `receive`
    /// pulls the next server message from it; behind a `Mutex` so `&mut self`
    /// receives are sequential (the trait's contract) while `send` clones only
    /// `post`.
    stream: Mutex<SseMessageStream>,
}

/// The shared state a `send` future needs: where to POST and with what headers.
struct PostContext {
    client: reqwest::Client,
    endpoint: String,
    headers: http::HeaderMap,
}

/// The decoded server-message side of the SSE stream: the still-open GET body,
/// framed by `sse_stream` and filtered to `message` events parsed into
/// [`ServerJsonRpcMessage`]. A boxed stream keeps the concrete reqwest/sse-stream
/// generics off the transport type.
type SseMessageStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = ServerJsonRpcMessage> + Send>>;

/// A send/receive/parse failure on the SSE transport. rmcp's `Transport` trait
/// requires the error be `std::error::Error + Send + Sync + 'static`; this wraps a
/// message string (the underlying reqwest/serde errors are already stringified at
/// the boundary).
#[derive(Debug)]
struct SseTransportError(String);

impl std::fmt::Display for SseTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSE transport error: {}", self.0)
    }
}

impl std::error::Error for SseTransportError {}

impl SseClientTransport {
    /// Opens the SSE GET stream and reads the leading `endpoint` event, returning a
    /// transport primed to POST to the resolved endpoint and to yield subsequent
    /// `message` events. Any HTTP / framing / missing-endpoint failure is an
    /// `Err` the caller records as a connect failure.
    async fn connect(
        url: &str,
        headers: http::HeaderMap,
    ) -> Result<SseClientTransport, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let response = client
            .get(url)
            .headers(headers.clone())
            .header(http::header::ACCEPT, "text/event-stream")
            .send()
            .await?
            .error_for_status()?;

        // Frame the response body with rmcp's own SSE parser (over reqwest's
        // byte stream) rather than re-implementing `event:`/`data:` splitting.
        let byte_stream = response.bytes_stream();
        let mut sse = sse_stream::SseStream::from_bytes_stream(byte_stream);

        // The first meaningful event MUST be `endpoint`; its data is the POST URL,
        // resolved relative to the SSE url (qwen's `SSEClientTransport`). Skip any
        // leading keep-alive comments / dataless frames until it arrives.
        let endpoint = loop {
            let event = sse
                .next()
                .await
                .ok_or_else(|| SseTransportError("SSE stream closed before endpoint".into()))?
                .map_err(|e| SseTransportError(format!("SSE read failed: {e}")))?;
            if event.event.as_deref() == Some("endpoint") {
                let data = event
                    .data
                    .ok_or_else(|| SseTransportError("SSE endpoint event had no data".into()))?;
                break resolve_url(url, data.trim());
            }
        };

        // The remaining stream carries `message` events (the server's JSON-RPC
        // responses + notifications); map each to a decoded message, dropping
        // anything that is not a parseable message frame (keep-alives, comments).
        let message_stream = sse
            .filter_map(|event| async move {
                let event = event.ok()?;
                if event.event.as_deref() != Some("message") {
                    return None;
                }
                let data = event.data?;
                serde_json::from_str::<ServerJsonRpcMessage>(&data).ok()
            })
            .boxed();

        Ok(SseClientTransport {
            post: Arc::new(PostContext {
                client,
                endpoint,
                headers,
            }),
            stream: Mutex::new(message_stream),
        })
    }
}

impl Transport<RoleClient> for SseClientTransport {
    type Error = SseTransportError;

    fn send(
        &mut self,
        item: ClientJsonRpcMessage,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Clone the shared POST context so the future owns everything it needs and
        // is `'static` (the trait requires it, since sends may run concurrently).
        let post = Arc::clone(&self.post);
        async move {
            let body = serde_json::to_vec(&item)
                .map_err(|e| SseTransportError(format!("serialize JSON-RPC failed: {e}")))?;
            post.client
                .post(&post.endpoint)
                .headers(post.headers.clone())
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| SseTransportError(format!("POST to endpoint failed: {e}")))?
                .error_for_status()
                .map_err(|e| SseTransportError(format!("endpoint returned error: {e}")))?;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        self.stream.lock().await.next().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // Dropping the boxed byte stream closes the underlying GET connection;
        // there is no server-side session to tear down in the legacy protocol.
        Ok(())
    }
}

/// Resolves a (possibly relative) endpoint URL from an SSE `endpoint` event
/// against the SSE GET url (qwen resolves the endpoint relative to the SSE url).
/// An absolute URL (has a scheme) is returned as-is; a root-relative path
/// (`/messages`) replaces the base's path; any other relative value is joined
/// onto the base's directory. No `url` crate: minimal string assembly over the
/// scheme/authority the base already carries.
fn resolve_url(base: &str, endpoint: &str) -> String {
    if endpoint.contains("://") {
        return endpoint.to_string();
    }
    // Split the base into scheme+authority (up to the path) so a relative
    // endpoint can be re-rooted. `base` is a well-formed http(s) URL here (the GET
    // just succeeded against it).
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let authority_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    let origin = &base[..authority_end];
    if let Some(stripped) = endpoint.strip_prefix('/') {
        return format!("{origin}/{stripped}");
    }
    // A bare relative value joins onto the base path's directory (everything up
    // to and including the last `/`).
    let base_no_query = base.split('?').next().unwrap_or(base);
    let dir_end = base_no_query
        .rfind('/')
        .map(|i| i + 1)
        .unwrap_or(base_no_query.len());
    format!("{}{endpoint}", &base_no_query[..dir_end])
}

/// Turns a server's static `BTreeMap<name, value>` headers into a typed
/// [`http::HeaderMap`], the ONE header-construction path both the HTTP and SSE
/// arms of [`serve`] share (the HTTP arm collects the result into the `HashMap`
/// `custom_headers` wants). A malformed name or value is a loud error naming the
/// offending header, preserving each arm's prior behavior.
fn build_header_map(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<http::HeaderMap, String> {
    let mut header_map = http::HeaderMap::new();
    for (key, value) in headers {
        let name = http::HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| format!("bad header name {key:?}: {e}"))?;
        let val = http::HeaderValue::from_str(value)
            .map_err(|e| format!("bad header value for {key:?}: {e}"))?;
        header_map.insert(name, val);
    }
    Ok(header_map)
}

/// Appends a `key=value` query parameter to a URL (ADR-0065 Phase D, the SSE
/// `token_param_name` injection): picks `?` or `&` by whether the URL already
/// carries a query. Minimal string assembly (no `url` crate); the token is a
/// base64url access token, so it needs no escaping.
fn append_query_param(url: &str, key: &str, value: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{key}={value}")
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
#[path = "../../../tests/mcp/manager/connect.rs"]
mod tests;
