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
//! ...}` XOR `{http_url, headers}` at the top level, alongside the common
//! `timeout_ms`/`trust`/`include_tools`/`exclude_tools`. But the in-memory model
//! makes the transport a SUM TYPE ([`McpTransport`]) so the "both `command` and
//! `http_url`" and "neither" states are UNREPRESENTABLE, not a deferred runtime
//! check. The flat wire and the sum type are bridged by a hand-written
//! [`Deserialize`]/[`Serialize`] on [`McpServerConfig`]: parse reads the known
//! flat keys, rejects an unknown key (`deny_unknown_fields` parity) and the
//! command-XOR-http_url ambiguity AT PARSE TIME (with field context), then folds
//! the transport fields into the sum type; serialize writes the same flat shape
//! back.

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
    /// The resolved transport - exactly one of stdio or HTTP. The flat wire keys
    /// (`command`/`args`/`env`/`cwd` or `http_url`/`headers`) fold into this at
    /// parse time; the both/neither ambiguity is a parse error, not a stored
    /// illegal state.
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
}

/// A resolved transport: exactly one of the two shapes a server config
/// expresses. The manager builds the concrete rmcp transport from this (the only
/// place the wire crate is touched), so the resolution stays pure and
/// unit-tested. Modelling it as a sum type (rather than a bag of `Option`s) is
/// what makes "both transports" and "neither" unrepresentable.
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
    "headers",
    "timeout_ms",
    "trust",
    "include_tools",
    "exclude_tools",
];

/// Deserializes the FLAT wire shape into the sum-type config. The transport keys
/// (`command`/`args`/`env`/`cwd` and `http_url`/`headers`) are read into locals;
/// the command-XOR-http_url rule is enforced HERE, so "both" and "neither" are
/// parse errors with the field context rather than a deferred runtime check. An
/// unknown key errors (`deny_unknown_fields` parity).
impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<McpServerConfig, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConfigVisitor;

        impl<'de> Visitor<'de> for ConfigVisitor {
            type Value = McpServerConfig;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an MCP server config (a `command` or `http_url` entry)")
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
                let mut headers: Option<BTreeMap<String, String>> = None;
                let mut timeout_ms: Option<u64> = None;
                let mut trust: Option<bool> = None;
                let mut include_tools: Option<Vec<String>> = None;
                let mut exclude_tools: Option<Vec<String>> = None;

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
                        "headers" => set_once!(headers),
                        "timeout_ms" => set_once!(timeout_ms),
                        "trust" => set_once!(trust),
                        "include_tools" => set_once!(include_tools),
                        "exclude_tools" => set_once!(exclude_tools),
                        // deny_unknown_fields parity: a typo'd key is a loud error.
                        other => return Err(de::Error::unknown_field(other, FIELDS)),
                    }
                }

                // The transport rule, enforced at parse time: command XOR
                // http_url. "both" and "neither" become deserialize errors (with
                // the offending shape named) rather than a stored illegal state.
                // Each arm also rejects the OTHER transport's keys, so a key that
                // does not belong to the chosen shape is a loud error rather than
                // a value that silently round-trips away.
                let transport = match (command, http_url) {
                    (Some(command), None) => {
                        if headers.is_some() {
                            return Err(de::Error::custom(
                                "`headers` set on a stdio server (`command`) - headers belong to an HTTP server (`http_url`)",
                            ));
                        }
                        McpTransport::Stdio {
                            command,
                            args: args.unwrap_or_default(),
                            env: env.unwrap_or_default(),
                            cwd,
                        }
                    }
                    (None, Some(url)) => {
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
                    (Some(_), Some(_)) => {
                        return Err(de::Error::custom(
                            "both `command` and `http_url` set - an MCP server is either stdio (command) or HTTP (http_url), not both",
                        ));
                    }
                    (None, None) => {
                        return Err(de::Error::custom(
                            "neither `command` nor `http_url` set - an MCP server needs one transport",
                        ));
                    }
                };

                Ok(McpServerConfig {
                    transport,
                    timeout_ms,
                    trust,
                    include_tools,
                    exclude_tools: exclude_tools.unwrap_or_default(),
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
        }
        len += usize::from(self.timeout_ms.is_some());
        len += usize::from(self.trust.is_some());
        len += usize::from(self.include_tools.is_some());
        len += usize::from(!self.exclude_tools.is_empty());

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
        s.end()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/mcp/config.rs"]
mod tests;
