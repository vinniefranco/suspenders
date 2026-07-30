//! The per-server MCP config (ADR-0056): what the user writes under
//! `mcp_servers` in `config.json`, and the pure rules that turn one entry into a
//! resolved [`McpTransport`] and an allow/deny tool filter.
//!
//! Suspenders is config-native snake_case (ADR-0031), so the key is
//! `mcp_servers` and every field is snake_case. qwen-code names the same map
//! `mcpServers` (camelCase); the divergence is deliberate - a config port stays
//! in Suspenders' idiom, not the source's.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One MCP server's config entry. A stdio server carries `command` (+ optional
/// `args`/`env`/`cwd`); an HTTP server carries `http_url` (+ optional
/// `headers`). Exactly one of the two transports must be expressed - see
/// [`transport`](McpServerConfig::transport). `deny_unknown_fields` like the
/// rest of the config schema, so a typo'd key is a loud parse error.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// The stdio server's executable. Present => a stdio transport (spawn
    /// `command` with `args`/`env`/`cwd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The stdio server's arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the spawned stdio server, layered over the
    /// inherited environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The working directory for the spawned stdio server; absent inherits the
    /// Agent's cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The HTTP server's endpoint. Present => a streamable-HTTP transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    /// Static headers sent with every HTTP request (OAuth is out of scope,
    /// ADR-0056 - a bearer token is a `headers` entry the user writes by hand).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The per-server connect + call timeout in milliseconds; absent uses the
    /// transport default (30s stdio, 5s HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Whether the user trusts this server's tools (parsed + stored for
    /// ADR-0056 parity; gates nothing in P1c - MCP-call approval is out of
    /// scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<bool>,
    /// An allowlist: when `Some`, ONLY these tool names are admitted (before
    /// exclusion). Absent admits every discovered tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
    /// A denylist: these tool names are always dropped, even if included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
}

/// A resolved transport: exactly one of the two the config expresses. The
/// manager builds the concrete rmcp transport from this (the only place the
/// wire crate is touched), so the resolution rule stays pure and unit-tested.
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
    /// Resolves the config's transport, or `Err` naming the ambiguity. A
    /// command-only entry is stdio; an http_url-only entry is HTTP; both or
    /// neither is a malformed config (a LOUD launch failure, distinct from the
    /// fail-open connect path - see [`crate::mcp::manager`]).
    pub fn transport(&self) -> Result<McpTransport, String> {
        match (&self.command, &self.http_url) {
            (Some(command), None) => Ok(McpTransport::Stdio {
                command: command.clone(),
                args: self.args.clone(),
                env: self.env.clone(),
                cwd: self.cwd.clone(),
            }),
            (None, Some(url)) => Ok(McpTransport::Http {
                url: url.clone(),
                headers: self.headers.clone(),
            }),
            (Some(_), Some(_)) => Err(
                "both `command` and `http_url` set - an MCP server is either stdio (command) or HTTP (http_url), not both"
                    .to_string(),
            ),
            (None, None) => Err(
                "neither `command` nor `http_url` set - an MCP server needs one transport"
                    .to_string(),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio() -> McpServerConfig {
        McpServerConfig {
            command: Some("my-server".into()),
            args: vec!["--flag".into()],
            ..Default::default()
        }
    }

    #[test]
    fn transport_resolves_a_command_only_entry_to_stdio() {
        let cfg = stdio();
        assert_eq!(
            cfg.transport(),
            Ok(McpTransport::Stdio {
                command: "my-server".into(),
                args: vec!["--flag".into()],
                env: BTreeMap::new(),
                cwd: None,
            })
        );
    }

    #[test]
    fn transport_resolves_an_http_url_only_entry_to_http() {
        let cfg = McpServerConfig {
            http_url: Some("https://example.test/mcp".into()),
            headers: BTreeMap::from([("Authorization".into(), "Bearer x".into())]),
            ..Default::default()
        };
        assert_eq!(
            cfg.transport(),
            Ok(McpTransport::Http {
                url: "https://example.test/mcp".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer x".into())]),
            })
        );
    }

    #[test]
    fn transport_rejects_both_transports() {
        let cfg = McpServerConfig {
            command: Some("cmd".into()),
            http_url: Some("https://x.test".into()),
            ..Default::default()
        };
        assert!(cfg.transport().unwrap_err().contains("both"));
    }

    #[test]
    fn transport_rejects_neither_transport() {
        let cfg = McpServerConfig::default();
        assert!(cfg.transport().unwrap_err().contains("neither"));
    }

    #[test]
    fn admits_everything_with_no_filters() {
        let cfg = stdio();
        assert!(cfg.admits("anything"));
    }

    #[test]
    fn admits_treats_include_as_an_allowlist() {
        let cfg = McpServerConfig {
            command: Some("cmd".into()),
            include_tools: Some(vec!["keep".into()]),
            ..Default::default()
        };
        assert!(cfg.admits("keep"));
        assert!(!cfg.admits("drop"));
    }

    #[test]
    fn admits_treats_a_paren_prefixed_include_entry_as_the_tool() {
        // qwen's paren form: `foo(...)` in the allowlist admits the tool `foo`.
        let cfg = McpServerConfig {
            command: Some("cmd".into()),
            include_tools: Some(vec!["keep(a, b)".into()]),
            ..Default::default()
        };
        assert!(cfg.admits("keep"));
        assert!(!cfg.admits("keeper")); // a longer name is NOT a paren match
        assert!(!cfg.admits("drop"));
    }

    #[test]
    fn admits_always_removes_excluded_even_when_included() {
        let cfg = McpServerConfig {
            command: Some("cmd".into()),
            include_tools: Some(vec!["keep".into(), "banned".into()]),
            exclude_tools: vec!["banned".into()],
            ..Default::default()
        };
        assert!(cfg.admits("keep"));
        assert!(!cfg.admits("banned"));
    }
}
