//! The per-server MCP config (ADR-0056): what the user writes under
//! `mcp_servers` in `config.json`, and the pure rules that turn one entry into a
//! resolved [`McpTransport`] and an allow/deny tool filter.
//!
//! Suspenders is config-native snake_case (ADR-0031), so the key is
//! `mcp_servers` and every field is snake_case. qwen-code names the same map
//! `mcpServers` (camelCase); the divergence is deliberate - a config port stays
//! in Suspenders' idiom, not the source's.
//!
//! The wire shape stays FLAT (qwen fidelity): a user writes `{command, args,
//! ...}` XOR `{http_url, headers}` XOR `{url, headers}` at the top level,
//! alongside the common `timeout_ms`/`trust`/`include_tools`/`exclude_tools`.
//! The three transport keys are mutually exclusive: `command` is stdio,
//! `http_url` is streamable-HTTP, and `url` is the legacy MCP HTTP+SSE transport
//! (qwen names this key `url`, distinct from streamable-HTTP's `http_url`). But
//! the in-memory model makes the transport a SUM TYPE ([`McpTransport`]) so the
//! "more than one transport key" and "neither" states are UNREPRESENTABLE, not a
//! deferred runtime check. The flat wire and the sum type are bridged by a
//! hand-written [`Deserialize`]/[`Serialize`] on [`McpServerConfig`]: parse reads
//! the known flat keys, rejects an unknown key (`deny_unknown_fields` parity) and
//! the transport-key ambiguity AT PARSE TIME (with field context), then folds the
//! transport fields into the sum type; serialize writes the same flat shape back.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One MCP server's config entry. The [`transport`](McpServerConfig::transport)
/// is a sum type - stdio XOR HTTP, never both or neither - so a malformed entry
/// cannot be constructed or parsed; the rest is the common cross-transport
/// config (timeout, trust, the tool filter). The wire shape is flat (see the
/// module docs); construct one from a [`McpTransport`] via [`McpServerConfig::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    /// The resolved transport - exactly one of stdio, streamable-HTTP, or SSE.
    /// The flat wire keys (`command`/`args`/`env`/`cwd`, `http_url`/`headers`, or
    /// `url`/`headers`) fold into this at parse time; the more-than-one / neither
    /// ambiguity is a parse error, not a stored illegal state.
    pub transport: McpTransport,
    /// The per-server connect + call timeout in milliseconds; absent uses the
    /// transport default (30s stdio, 5s HTTP).
    pub timeout_ms: Option<u64>,
    /// Whether the user trusts this server's tools (parsed + stored for
    /// ADR-0056 parity; gates nothing in P1c - MCP-call approval is out of
    /// scope).
    pub trust: Option<bool>,
    /// An allowlist: when `Some`, ONLY these tool names are admitted (before
    /// exclusion). Absent admits every discovered tool.
    pub include_tools: Option<Vec<String>>,
    /// A denylist: these tool names are always dropped, even if included.
    pub exclude_tools: Vec<String>,
    /// The per-server OAuth 2.0 config (ADR-0065 Phase D, qwen's `oauth` block):
    /// a nested object under the flat `oauth` wire key. Absent means the server
    /// carries no OAuth (static headers only, ADR-0056); present drives the
    /// [`oauth`](crate::mcp::oauth) provider + the Bearer-token injection at
    /// connect.
    pub oauth: Option<McpOAuthConfig>,
}

/// One MCP server's OAuth 2.0 config (ADR-0065 Phase D, qwen's `MCPOAuthConfig`):
/// the nested `oauth` object a server entry may carry. Every field is snake_case
/// (ADR-0031); the qwen names are camelCase (`clientId`, `authorizationUrl`, ...),
/// the deliberate config-port divergence. All fields are optional - a minimal
/// `{"enabled": true}` on an HTTP server lets the provider DISCOVER the
/// authorization/token endpoints from the MCP server URL's `/.well-known/*`
/// metadata and REGISTER a public client dynamically, so a user need supply
/// nothing but the flag. Explicit fields short-circuit that discovery.
///
/// Every field is `skip_serializing_if` absent + `default` on parse, so a
/// parse -> serialize round-trip writes back exactly the keys the user wrote (an
/// absent field never round-trips as an explicit `null`), matching how the flat
/// [`McpServerConfig`] keys behave.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpOAuthConfig {
    /// Whether OAuth is enabled for this server (qwen `enabled`). Absent reads as
    /// unset; the dialog's Authenticate action and the connect-time token
    /// injection treat `Some(true)` as on.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enabled: Option<bool>,
    /// A pre-registered OAuth client id (qwen `clientId`). Absent triggers dynamic
    /// client registration against the discovered/`registration_url` endpoint.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_id: Option<String>,
    /// The client secret for a confidential client (qwen `clientSecret`); a public
    /// client (the default, `token_endpoint_auth_method: none`) has none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_secret: Option<String>,
    /// The authorization endpoint (qwen `authorizationUrl`); absent is discovered
    /// from the MCP server's `/.well-known/*` metadata.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub authorization_url: Option<String>,
    /// The token endpoint (qwen `tokenUrl`); absent is discovered alongside the
    /// authorization endpoint.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub token_url: Option<String>,
    /// The scopes to request (qwen `scopes`), space-joined onto the authorization
    /// + token requests.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scopes: Option<Vec<String>>,
    /// The audiences to request (qwen `audiences`), space-joined onto the
    /// authorization + token requests.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub audiences: Option<Vec<String>>,
    /// The redirect URI (qwen `redirectUri`); absent uses the hand-rolled
    /// localhost callback (`http://localhost:<port><path>`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub redirect_uri: Option<String>,
    /// For an SSE connection, the query-parameter name the access token rides as
    /// (qwen `tokenParamName`) instead of an `Authorization` header.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub token_param_name: Option<String>,
    /// The dynamic client registration endpoint (qwen `registrationUrl`); absent
    /// is discovered from the authorization server metadata.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registration_url: Option<String>,
}

/// A resolved transport: exactly one of the three shapes a server config
/// expresses. The manager builds the concrete rmcp transport from this (the only
/// place the wire crate is touched), so the resolution stays pure and
/// unit-tested. Modelling it as a sum type (rather than a bag of `Option`s) is
/// what makes "more than one transport" and "neither" unrepresentable.
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    /// A stdio server: spawn `command` with `args`, `env` layered over the
    /// inherited environment, in `cwd` (or the inherited cwd).
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    /// A streamable-HTTP server at `url`, sending the static `headers` with
    /// every request.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    /// A legacy MCP HTTP+SSE server at `url` (qwen's `url` key, distinct from
    /// [`Http`](Self::Http)'s `http_url`): the client GETs `url` as a
    /// `text/event-stream`, the server's first `endpoint` event names the URL to
    /// POST JSON-RPC to, and responses arrive as `message` events on the open
    /// stream. `headers` ride both the GET and the POST.
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl McpServerConfig {
    /// Builds a config from a resolved [`McpTransport`] and the default (empty)
    /// common config. Tests and the manager construct through this instead of a
    /// struct literal so the flat wire fields never leak back as public state.
    pub fn new(transport: McpTransport) -> McpServerConfig {
        McpServerConfig {
            transport,
            timeout_ms: None,
            trust: None,
            include_tools: None,
            exclude_tools: Vec::new(),
            oauth: None,
        }
    }

    /// Whether a discovered tool is admitted onto the registry. `include_tools`
    /// is an allowlist when present (a tool absent from it is dropped);
    /// `exclude_tools` always removes. Both filter by the bare server tool name.
    /// An include entry admits the tool when it EQUALS the name or starts with
    /// `"<name>("` - qwen's paren form, so `foo(a,b)` in the allowlist still
    /// admits the tool `foo` (the args after the paren are ignored here).
    pub fn admits(&self, tool_name: &str) -> bool {
        if let Some(include) = &self.include_tools
            && !include
                .iter()
                .any(|t| t == tool_name || t.starts_with(&format!("{tool_name}(")))
        {
            return false;
        }
        !self.exclude_tools.iter().any(|t| t == tool_name)
    }
}

/// The flat wire keys a server entry may carry. `deny_unknown_fields` parity: a
/// key outside this set is a parse error, so a typo stays loud.
const FIELDS: &[&str] = &[
    "command",
    "args",
    "env",
    "cwd",
    "http_url",
    "url",
    "headers",
    "timeout_ms",
    "trust",
    "include_tools",
    "exclude_tools",
    "oauth",
];

/// Deserializes the FLAT wire shape into the sum-type config. The transport keys
/// (`command`/`args`/`env`/`cwd`, `http_url`/`headers`, and `url`/`headers`) are
/// read into locals; the "exactly one of command/http_url/url" rule is enforced
/// HERE, so "more than one" and "neither" are parse errors with the field context
/// rather than a deferred runtime check. An unknown key errors
/// (`deny_unknown_fields` parity).
impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<McpServerConfig, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConfigVisitor;

        impl<'de> Visitor<'de> for ConfigVisitor {
            type Value = McpServerConfig;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an MCP server config (a `command`, `http_url`, or `url` entry)")
            }

            fn visit_map<A>(self, mut map: A) -> Result<McpServerConfig, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut command: Option<String> = None;
                let mut args: Option<Vec<String>> = None;
                let mut env: Option<BTreeMap<String, String>> = None;
                let mut cwd: Option<String> = None;
                let mut http_url: Option<String> = None;
                let mut url: Option<String> = None;
                let mut headers: Option<BTreeMap<String, String>> = None;
                let mut timeout_ms: Option<u64> = None;
                let mut trust: Option<bool> = None;
                let mut include_tools: Option<Vec<String>> = None;
                let mut exclude_tools: Option<Vec<String>> = None;
                let mut oauth: Option<McpOAuthConfig> = None;

                // Read a value into its slot, erroring on a duplicate key (serde
                // derive's behaviour, kept so a repeated flat key stays a loud
                // parse error). Inlined as a macro rather than a helper fn so it
                // reads as part of the visitor's own body.
                macro_rules! set_once {
                    ($slot:ident) => {{
                        if $slot.is_some() {
                            return Err(de::Error::duplicate_field(stringify!($slot)));
                        }
                        $slot = Some(map.next_value()?);
                    }};
                }

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "command" => set_once!(command),
                        "args" => set_once!(args),
                        "env" => set_once!(env),
                        "cwd" => set_once!(cwd),
                        "http_url" => set_once!(http_url),
                        "url" => set_once!(url),
                        "headers" => set_once!(headers),
                        "timeout_ms" => set_once!(timeout_ms),
                        "trust" => set_once!(trust),
                        "include_tools" => set_once!(include_tools),
                        "exclude_tools" => set_once!(exclude_tools),
                        "oauth" => set_once!(oauth),
                        // deny_unknown_fields parity: a typo'd key is a loud error.
                        other => return Err(de::Error::unknown_field(other, FIELDS)),
                    }
                }

                // The transport rule, enforced at parse time: exactly one of
                // `command` / `http_url` / `url`. "more than one" and "neither"
                // become deserialize errors (with the offending shape named)
                // rather than a stored illegal state. Each arm also rejects the
                // OTHER transports' keys, so a key that does not belong to the
                // chosen shape is a loud error rather than a value that silently
                // round-trips away. `headers` is valid on `http_url` AND `url`;
                // `args`/`env`/`cwd` are stdio-only.
                let transport = match (command, http_url, url) {
                    (Some(command), None, None) => {
                        if headers.is_some() {
                            return Err(de::Error::custom(
                                "`headers` set on a stdio server (`command`) - headers belong to an HTTP (`http_url`) or SSE (`url`) server",
                            ));
                        }
                        McpTransport::Stdio {
                            command,
                            args: args.unwrap_or_default(),
                            env: env.unwrap_or_default(),
                            cwd,
                        }
                    }
                    (None, Some(url), None) => {
                        if args.is_some() || env.is_some() || cwd.is_some() {
                            return Err(de::Error::custom(
                                "`args`/`env`/`cwd` set on an HTTP server (`http_url`) - those belong to a stdio server (`command`)",
                            ));
                        }
                        McpTransport::Http {
                            url,
                            headers: headers.unwrap_or_default(),
                        }
                    }
                    (None, None, Some(url)) => {
                        if args.is_some() || env.is_some() || cwd.is_some() {
                            return Err(de::Error::custom(
                                "`args`/`env`/`cwd` set on an SSE server (`url`) - those belong to a stdio server (`command`)",
                            ));
                        }
                        McpTransport::Sse {
                            url,
                            headers: headers.unwrap_or_default(),
                        }
                    }
                    (None, None, None) => {
                        return Err(de::Error::custom(
                            "none of `command`, `http_url`, or `url` set - an MCP server needs one transport",
                        ));
                    }
                    _ => {
                        return Err(de::Error::custom(
                            "more than one of `command`, `http_url`, `url` set - an MCP server has exactly one transport (stdio, HTTP, or SSE)",
                        ));
                    }
                };

                Ok(McpServerConfig {
                    transport,
                    timeout_ms,
                    trust,
                    include_tools,
                    exclude_tools: exclude_tools.unwrap_or_default(),
                    oauth,
                })
            }
        }

        deserializer.deserialize_map(ConfigVisitor)
    }
}

/// Serializes back to the FLAT wire shape: the transport's fields are written at
/// the top level (matching what the user wrote), alongside the common keys.
/// `skip_serializing_if`-equivalent guards keep an empty collection / absent
/// option off the wire, so a parse -> serialize round-trip is faithful.
impl Serialize for McpServerConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Count the fields to emit so `serialize_struct`'s hint is right.
        let mut len = 0;
        match &self.transport {
            McpTransport::Stdio { args, env, cwd, .. } => {
                len += 1; // command
                len += usize::from(!args.is_empty());
                len += usize::from(!env.is_empty());
                len += usize::from(cwd.is_some());
            }
            McpTransport::Http { headers, .. } => {
                len += 1; // http_url
                len += usize::from(!headers.is_empty());
            }
            McpTransport::Sse { headers, .. } => {
                len += 1; // url
                len += usize::from(!headers.is_empty());
            }
        }
        len += usize::from(self.timeout_ms.is_some());
        len += usize::from(self.trust.is_some());
        len += usize::from(self.include_tools.is_some());
        len += usize::from(!self.exclude_tools.is_empty());
        len += usize::from(self.oauth.is_some());

        let mut s = serializer.serialize_struct("McpServerConfig", len)?;
        match &self.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                s.serialize_field("command", command)?;
                if !args.is_empty() {
                    s.serialize_field("args", args)?;
                }
                if !env.is_empty() {
                    s.serialize_field("env", env)?;
                }
                if let Some(cwd) = cwd {
                    s.serialize_field("cwd", cwd)?;
                }
            }
            McpTransport::Http { url, headers } => {
                s.serialize_field("http_url", url)?;
                if !headers.is_empty() {
                    s.serialize_field("headers", headers)?;
                }
            }
            McpTransport::Sse { url, headers } => {
                s.serialize_field("url", url)?;
                if !headers.is_empty() {
                    s.serialize_field("headers", headers)?;
                }
            }
        }
        if let Some(timeout_ms) = &self.timeout_ms {
            s.serialize_field("timeout_ms", timeout_ms)?;
        }
        if let Some(trust) = &self.trust {
            s.serialize_field("trust", trust)?;
        }
        if let Some(include_tools) = &self.include_tools {
            s.serialize_field("include_tools", include_tools)?;
        }
        if !self.exclude_tools.is_empty() {
            s.serialize_field("exclude_tools", &self.exclude_tools)?;
        }
        if let Some(oauth) = &self.oauth {
            s.serialize_field("oauth", oauth)?;
        }
        s.end()
    }
}

#[cfg(test)]
#[path = "../../tests/mcp/config.rs"]
mod tests;
