//! The `suspenders mcp add | remove | list` CLI (ADR-0065): the command-line
//! front the persist/remove/compose seams already grew for. Before this, a user
//! could only add an MCP server by hand-editing `config.json`; these subcommands
//! mirror qwen-code's `qwen mcp add/remove/list` in Suspenders' snake_case idiom.
//!
//! The module splits along the pure/impure line the rest of the config seam
//! keeps: [`build_server_config`] is a PURE function turning parsed flags into an
//! [`McpServerConfig`], doing ALL the validation (transport/positional/header/env/
//! oauth rules) with clear error strings and NO filesystem, so the wire-fidelity
//! rules are unit-tested with literals. The impure part - resolving the scope
//! path, calling the persist/remove/compose seams, and printing - lives in
//! [`dispatch`], the one place `main` hands off to. `main` stays thin: it parses
//! the clap tree and calls [`dispatch`].
//!
//! The positional/flag surface matches qwen exactly: `mcp add [options] <name>
//! <commandOrUrl> [args...]`, where `<commandOrUrl>` is the stdio command (with
//! `[args...]` its args) OR the http/sse URL (where `[args...]` are invalid), and
//! the transport is chosen by `-t/--transport`. Headers belong to http/sse only
//! (a header on a stdio server is a loud error, matching how `config.rs` rejects
//! `headers` on a stdio entry); OAuth flags apply to http/sse and, when any is
//! present, set `oauth.enabled = Some(true)` so the server authenticates.

use std::collections::BTreeMap;

use clap::{Args, Subcommand, ValueEnum};

use crate::mcp::config::{McpOAuthConfig, McpServerConfig, McpTransport};

/// The `mcp` subcommand tree (qwen `qwen mcp <add|remove|list>`). Held by
/// `main`'s top-level `Command::Mcp`, so the run path (bare `suspenders`,
/// headless, `--write-config`) is untouched by adding it.
#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Add or update an MCP server in a settings scope.
    // Boxed (not part of the help text): `McpAddArgs` carries a dozen optional
    // flags, so boxing it keeps the enum small (`clippy::large_enum_variant`)
    // without changing the surface.
    Add(Box<McpAddArgs>),
    /// Remove an MCP server from a settings scope.
    Remove(McpRemoveArgs),
    /// List the configured MCP servers across scopes.
    List(McpListArgs),
}

/// Which settings scope a `mcp add`/`remove` targets (qwen `-s/--scope`). `User`
/// is the global XDG config; `Project` is the workspace `.suspenders/config.json`
/// under the project root. Named `project` on the wire (qwen's word) even though
/// the internal source is `Workspace`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[value(rename_all = "lower")]
pub enum McpScope {
    /// The user (global) config in the XDG config home.
    #[default]
    User,
    /// The workspace (project-local) `.suspenders/config.json`.
    Project,
}

/// The transport a `mcp add` builds (qwen `-t/--transport`). Mirrors the three
/// arms of [`McpTransport`], chosen at the CLI edge so the positional
/// `<commandOrUrl>` is interpreted correctly (a command for stdio, a URL for
/// http/sse).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[value(rename_all = "lower")]
pub enum McpTransportKind {
    /// A local process the client spawns and speaks stdio to.
    #[default]
    Stdio,
    /// A streamable-HTTP server (the `http_url` transport).
    Http,
    /// A legacy MCP HTTP+SSE server (the `url` transport).
    Sse,
}

/// The parsed `mcp add` invocation. The positional split is qwen's: `name` keys
/// the server, `command_or_url` is the stdio command OR the http/sse URL, and
/// `args` are the stdio command's arguments (invalid for http/sse). Every flag
/// is optional; the pure [`build_server_config`] enforces which combinations are
/// legal per transport.
#[derive(Args, Debug)]
pub struct McpAddArgs {
    /// The settings scope to write to.
    #[arg(short, long, value_enum, default_value_t = McpScope::User)]
    pub scope: McpScope,
    /// The transport kind, which decides how `command_or_url`/`args` are read.
    #[arg(short, long, value_enum, default_value_t = McpTransportKind::Stdio)]
    pub transport: McpTransportKind,
    /// A `KEY=VALUE` environment pair for a stdio server (repeatable).
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// A `Name: Value` header for an http/sse server (repeatable).
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    pub header: Vec<String>,
    /// The per-server connect/call timeout in milliseconds.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Mark the server trusted (`trust: true`).
    #[arg(long)]
    pub trust: bool,
    /// A comma-separated tool allowlist (`include_tools`).
    #[arg(long, value_name = "a,b,c")]
    pub include_tools: Option<String>,
    /// A comma-separated tool denylist (`exclude_tools`).
    #[arg(long, value_name = "a,b,c")]
    pub exclude_tools: Option<String>,
    /// OAuth: a pre-registered client id (http/sse).
    #[arg(long)]
    pub oauth_client_id: Option<String>,
    /// OAuth: the client secret for a confidential client (http/sse).
    #[arg(long)]
    pub oauth_client_secret: Option<String>,
    /// OAuth: the authorization endpoint (http/sse).
    #[arg(long)]
    pub oauth_authorization_url: Option<String>,
    /// OAuth: the token endpoint (http/sse).
    #[arg(long)]
    pub oauth_token_url: Option<String>,
    /// OAuth: comma-separated scopes to request (http/sse).
    #[arg(long, value_name = "a,b,c")]
    pub oauth_scopes: Option<String>,
    /// OAuth: comma-separated audiences to request (http/sse).
    #[arg(long, value_name = "a,b,c")]
    pub oauth_audiences: Option<String>,
    /// OAuth: the query-parameter name the token rides as, for an sse server
    /// (`token_param_name`) instead of an `Authorization` header.
    #[arg(long, value_name = "NAME")]
    pub oauth_token_param_name: Option<String>,
    /// OAuth: the redirect URI (http/sse).
    #[arg(long)]
    pub oauth_redirect_uri: Option<String>,
    /// The user-chosen server name (the `mcp_servers` map key).
    pub name: String,
    /// The stdio command, or the http/sse URL.
    pub command_or_url: String,
    /// The stdio command's arguments (invalid for http/sse). The
    /// `trailing_var_arg` + `allow_hyphen_values` pair captures everything after
    /// `<command_or_url>` VERBATIM, hyphen-led flags included, so a natural
    /// invocation like `mcp add srv python -m my_server --port 8080` passes the
    /// command's own flags through untouched (qwen parity) rather than clap
    /// parsing them as `mcp add`'s own options. Put every `mcp add` option
    /// BEFORE the command name.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// The parsed `mcp remove` invocation: the scope and the server name.
#[derive(Args, Debug)]
pub struct McpRemoveArgs {
    /// The settings scope to remove from.
    #[arg(short, long, value_enum, default_value_t = McpScope::User)]
    pub scope: McpScope,
    /// The server name to remove.
    pub name: String,
}

/// The parsed `mcp list` invocation. `list` composes BOTH scopes (user +
/// workspace) so it needs no `--scope`; it takes the same `--root` the run path
/// does so the workspace file resolves under the intended project.
#[derive(Args, Debug)]
pub struct McpListArgs {
    /// The project root whose workspace config is composed (defaults to the
    /// current directory, like the run path).
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

/// Builds an [`McpServerConfig`] from parsed `mcp add` flags, applying every
/// validation rule PURELY (no filesystem): the transport selects how the
/// positional `command_or_url`/`args` are read, headers are http/sse-only,
/// `args` are stdio-only, `env` pairs must be `KEY=VALUE`, headers must be
/// `Name: Value`, and any OAuth flag arms `oauth.enabled = Some(true)`. The
/// produced config round-trips through [`McpServerConfig`]'s Serialize/
/// Deserialize (the tests lock this), so it never yields a shape the parse-time
/// invariants would reject. Errors are user-facing strings; the caller
/// ([`dispatch`]) surfaces them at the `anyhow` boundary.
pub fn build_server_config(args: &McpAddArgs) -> Result<McpServerConfig, String> {
    let transport = match args.transport {
        McpTransportKind::Stdio => {
            if !args.header.is_empty() {
                return Err(
                    "`--header` is only valid for an http or sse transport (headers belong to an HTTP or SSE server, not a stdio one)".into(),
                );
            }
            McpTransport::Stdio {
                command: args.command_or_url.clone(),
                args: args.args.clone(),
                env: parse_env(&args.env)?,
                cwd: None,
            }
        }
        McpTransportKind::Http => {
            reject_args_for_url(args, "http")?;
            McpTransport::Http {
                url: args.command_or_url.clone(),
                headers: parse_headers(&args.header)?,
            }
        }
        McpTransportKind::Sse => {
            reject_args_for_url(args, "sse")?;
            McpTransport::Sse {
                url: args.command_or_url.clone(),
                headers: parse_headers(&args.header)?,
            }
        }
    };

    let mut config = McpServerConfig::new(transport);
    config.timeout_ms = args.timeout;
    // `--trust` is a bare flag; only record it when set, so an unset add leaves
    // `trust` absent (it round-trips off the wire) rather than writing `false`.
    config.trust = args.trust.then_some(true);
    config.include_tools = parse_tool_list(args.include_tools.as_deref());
    config.exclude_tools = parse_tool_list(args.exclude_tools.as_deref()).unwrap_or_default();
    config.oauth = build_oauth(args);

    Ok(config)
}

/// Rejects the stdio-only `args` positional on an http/sse add (qwen errors the
/// same way): a URL transport takes no command arguments.
fn reject_args_for_url(args: &McpAddArgs, kind: &str) -> Result<(), String> {
    if args.args.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "positional arguments are only valid for a stdio transport; the {kind} transport takes a single URL and no args (got {} extra: {})",
            args.args.len(),
            args.args.join(" ")
        ))
    }
}

/// Parses `KEY=VALUE` env pairs into the stdio env map. A pair missing `=`, or
/// with an empty key, is a loud error naming the offending entry.
fn parse_env(pairs: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("`--env` must be KEY=VALUE, got: {pair:?}"))?;
        if key.is_empty() {
            return Err(format!("`--env` KEY must be non-empty, got: {pair:?}"));
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Parses `Name: Value` headers into the http/sse header map. A header missing
/// `:`, or with an empty name, is a loud error; the value is trimmed of the one
/// conventional leading space after the colon.
fn parse_headers(headers: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for header in headers {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| format!("`--header` must be `Name: Value`, got: {header:?}"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("`--header` name must be non-empty, got: {header:?}"));
        }
        map.insert(name.to_string(), value.trim_start().to_string());
    }
    Ok(map)
}

/// Splits a comma-separated `--include-tools`/`--exclude-tools` value into a
/// name list, trimming whitespace and dropping empties. `None` (the flag absent)
/// yields `None`; a present-but-all-empty value yields `Some(vec![])`, matching
/// the flag having been given.
fn parse_tool_list(raw: Option<&str>) -> Option<Vec<String>> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// Builds the optional OAuth block from the `--oauth-*` flags: when ANY is
/// present, the block is emitted with `enabled = Some(true)` (so the server
/// authenticates) plus whichever explicit endpoints/credentials/scopes/audiences/
/// token-param were given; when NONE is present, the server carries no OAuth
/// (`None`). Scopes and audiences are the same comma-split as the tool lists.
/// `registration_url` is intentionally NOT a CLI flag: dynamic client
/// registration discovers its endpoint from the authorization server metadata,
/// so there is nothing for a user to author.
fn build_oauth(args: &McpAddArgs) -> Option<McpOAuthConfig> {
    let scopes = parse_tool_list(args.oauth_scopes.as_deref());
    let audiences = parse_tool_list(args.oauth_audiences.as_deref());
    let any = args.oauth_client_id.is_some()
        || args.oauth_client_secret.is_some()
        || args.oauth_authorization_url.is_some()
        || args.oauth_token_url.is_some()
        || args.oauth_redirect_uri.is_some()
        || args.oauth_token_param_name.is_some()
        || scopes.is_some()
        || audiences.is_some();
    if !any {
        return None;
    }
    Some(McpOAuthConfig {
        enabled: Some(true),
        client_id: args.oauth_client_id.clone(),
        client_secret: args.oauth_client_secret.clone(),
        authorization_url: args.oauth_authorization_url.clone(),
        token_url: args.oauth_token_url.clone(),
        scopes,
        audiences,
        redirect_uri: args.oauth_redirect_uri.clone(),
        token_param_name: args.oauth_token_param_name.clone(),
        registration_url: None,
    })
}

/// Resolves the config-file path a scope writes to (the impure edge): `User` is
/// the XDG config path; `Project` is the workspace config under `root` (the
/// current dir, like the run path). Split out so [`dispatch`] reads one line.
fn scope_path(scope: McpScope, root: &str) -> String {
    match scope {
        McpScope::User => crate::session::default_config_path(),
        McpScope::Project => crate::session::workspace_config_path(root),
    }
}

/// Executes an `mcp` subcommand (the impure boundary `main` hands off to):
/// resolves the scope path, calls the persist/remove/compose seams, and prints a
/// confirmation. The pure work ([`build_server_config`]) is done first so a
/// validation error surfaces before any file is touched. `add`/`remove` default
/// their workspace root to the current dir; `list` takes `--root`. Errors are
/// returned as `anyhow` for the `main` boundary.
pub fn dispatch(command: McpCommand) -> anyhow::Result<()> {
    match command {
        McpCommand::Add(args) => {
            let args = *args;
            let config = build_server_config(&args).map_err(|e| anyhow::anyhow!("{e}"))?;
            let root = current_dir();
            let path = scope_path(args.scope, &root);
            crate::session::SessionConfig::persist_mcp_server(&path, &args.name, &config)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let summary = crate::mcp::format_transport(&config.transport);
            println!(
                "added MCP server `{}` ({summary}) to {} scope at {path}",
                args.name,
                scope_label(args.scope),
            );
            Ok(())
        }
        McpCommand::Remove(args) => {
            let root = current_dir();
            let path = scope_path(args.scope, &root);
            let removed = crate::session::SessionConfig::remove_mcp_server(&path, &args.name)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if removed {
                println!(
                    "removed MCP server `{}` from {} scope at {path}",
                    args.name,
                    scope_label(args.scope),
                );
            } else {
                println!(
                    "no MCP server `{}` in {} scope at {path}",
                    args.name,
                    scope_label(args.scope),
                );
            }
            Ok(())
        }
        McpCommand::List(args) => {
            let root = args
                .root
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(current_dir);
            let user = crate::session::default_config_path();
            let workspace = crate::session::workspace_config_path(&root);
            // Compose is fail-soft on an absent scope file (an empty overlay), so
            // a fresh project with no config lists cleanly rather than erroring.
            let config = crate::session::SessionConfig::compose(&user, Some(&workspace))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            print_list(&config, &mut |line| println!("{line}"));
            Ok(())
        }
    }
}

/// Renders the `mcp list` output through a line sink (a sink so the rendering is
/// testable without stdout): each server as `name  <transport>  [scope]`,
/// alphabetical (the config map is a BTreeMap), or a single friendly empty-state
/// line when none are configured. Split from [`dispatch`] so the compose is the
/// only impure part.
fn print_list(config: &crate::session::SessionConfig, out: &mut dyn FnMut(&str)) {
    let mut any = false;
    for (name, server, source) in config.servers_with_source() {
        any = true;
        let summary = crate::mcp::format_transport(&server.transport);
        out(&format!("{name}  {summary}  [{}]", source_label(source)));
    }
    if !any {
        out("no MCP servers configured (add one with `suspenders mcp add`)");
    }
}

/// The scope's lower-case label for a confirmation line (`user`/`project`),
/// matching the `--scope` value the user passed.
fn scope_label(scope: McpScope) -> &'static str {
    match scope {
        McpScope::User => "user",
        McpScope::Project => "project",
    }
}

/// The `mcp list` scope annotation for an [`McpSource`](crate::mcp::McpSource):
/// the word matching the `--scope` value (`project` for a workspace server), so
/// the listing and the add/remove confirmations speak the same vocabulary.
fn source_label(source: crate::mcp::McpSource) -> &'static str {
    match source {
        crate::mcp::McpSource::User => "user",
        crate::mcp::McpSource::Workspace => "project",
        crate::mcp::McpSource::Extension => "extension",
    }
}

/// The current directory as a string, the default workspace root the CLI uses
/// (mirroring the run path's `default_root`). Falls back to `.` if the cwd is
/// unreadable, the same lenient rule.
fn current_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into())
}

#[cfg(test)]
#[path = "../../tests/mcp/cli.rs"]
mod tests;
