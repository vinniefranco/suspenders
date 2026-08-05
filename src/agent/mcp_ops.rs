//! The live `/mcp` dialog operations (ADR-0065 Phases C & D) and the
//! Session-stable tool-set rebuild they share - carved out of the Agent actor
//! ([`super`]) so the actor keeps the message dispatch and this module owns the
//! reconnect / enable-disable / authenticate / clear-auth mechanics.
//!
//! Each op runs INLINE on the actor loop (the operator is mid-dialog, exclusive
//! like a Run's own awaits, ADR-0017): it mutates the manager + the Session and
//! rebuilds `session_tools` so the NEXT Run sees the fresh tools, the same swap
//! rule a `SetModel` takes. [`rebuild_session_tools`] is `pub(crate)` because
//! `init_agent` builds the launch tool set with it too; it is re-exported from
//! [`super`] so callers still reach it as `crate::agent::rebuild_session_tools`.

use std::sync::Arc;

use crate::agent::AgentState;
use crate::event::Event;

/// Assembles the Session-stable tool set (F8, ADR-0056; ADR-0065 Phase C): the
/// built-ins, then the discovered MCP `adapters`, then the one `skill` tool, then
/// the one `agent` tool - the ONE order `init_agent` builds it in and every live
/// `/mcp` op rebuilds it in, so a rebuild is order-identical to the launch set.
/// The skill + agent tools are re-minted over the retained managers (their
/// dynamic catalogs are unchanged), so only the MCP slice actually differs after
/// a reconnect/enable/disable. The result is a fresh `Arc<[...]>`: swapping it on
/// the Agent lands on the NEXT Run's capture, never an in-flight Run's.
pub(crate) fn rebuild_session_tools(
    adapters: Vec<Box<dyn crate::tool::Tool>>,
    skill_manager: &Arc<crate::skills::SkillManager>,
    subagents: &Arc<crate::subagents::SubagentRegistry>,
) -> Arc<[Box<dyn crate::tool::Tool>]> {
    let mut all = crate::tools::tools();
    all.extend(adapters);
    all.push(Box::new(crate::tools::skill::SkillTool::new(Arc::clone(
        skill_manager,
    ))));
    all.push(Box::new(crate::tools::agent::AgentTool::new(Arc::clone(
        subagents,
    ))));
    all.into()
}

// Rebuilds the Session-stable tool set from the manager's CURRENT MCP adapters
// (ADR-0065 Phase C): the one line every live MCP op ends on. The manager has
// already applied the reconnect/enable/disable, so `adapters()` reflects the new
// connected set; `rebuild_session_tools` re-mints the built-ins + skill + agent
// tools around them. Swapping `state.session_tools` lands on the NEXT Run's
// capture - an in-flight Run keeps the `Arc` it already cloned (ADR-0033's swap
// rule, ADR-0017's read-only guest).
fn refresh_session_tools(state: &mut AgentState) {
    let adapters = state.mcp.adapters();
    state.session_tools = rebuild_session_tools(adapters, &state.skill_manager, &state.subagents);
}

// The Reconnect action (ADR-0065 Phase C, qwen `discoverToolsForServer`):
// re-attach the named server and rebuild the tool set from the manager's fresh
// adapters. A failed re-attach is fail-open (a Disconnected view + a launch
// notice), so this always completes; an unknown server is a no-op the manager
// absorbs. Runs INLINE on the actor loop, so the swap is exclusive and lands on
// the next Run.
pub(super) async fn mcp_reconnect(state: &mut AgentState, name: String) {
    state.mcp.reconnect(&name).await;
    // Surface a fresh connect failure the same way `init_agent` does (the
    // fail-open report seam, ADR-0018): a reconnect that fails again is a
    // visible skip.
    for (server, reason) in state.mcp.failures() {
        if server == name {
            let _ = state.events.send(Event::fail_open_report(
                format!("mcp server {server}"),
                format!("could not connect - {reason}"),
            ));
        }
    }
    refresh_session_tools(state);
}

// The Enable/Disable action (ADR-0065 Phase C, qwen `enable`/`disableServer`):
// persist the target scope's `mcp.excluded` list, update the Session's merged
// excluded set + the manager, and rebuild the tool set. `enabled` is the DESIRED
// state, so a disable is `enabled == false`. The scope is the server's Source: a
// User server writes the user config, a Workspace server the workspace config; an
// Extension server cannot be toggled (qwen refuses it too), an `Err` the dialog
// surfaces. Runs INLINE on the actor loop like `mcp_reconnect`.
pub(super) async fn mcp_set_enabled(
    state: &mut AgentState,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    // The server must be one this Session knows (its Source picks the scope file).
    let Some(source) = state.session.mcp_sources.get(&name).copied() else {
        return Err(format!("unknown MCP server {name:?}"));
    };
    // qwen cannot enable/disable an extension-contributed server: it has no
    // settings file to write the exclusion into.
    let path = match source {
        crate::mcp::McpSource::User => crate::session::default_config_path(),
        crate::mcp::McpSource::Workspace => {
            crate::session::workspace_config_path(&state.session.root)
        }
        crate::mcp::McpSource::Extension => {
            return Err("extension MCP servers cannot be enabled or disabled".to_string());
        }
    };

    // Fold the toggle into the Session's merged excluded set: a disable adds the
    // name, an enable removes it. The set stays de-duplicated so a name persisted
    // once is not written twice.
    let disabled = !enabled;
    let mut excluded: Vec<String> = state
        .session
        .mcp_excluded
        .iter()
        .filter(|n| *n != &name)
        .cloned()
        .collect();
    if disabled {
        excluded.push(name.clone());
    }

    // The scope file holds ONLY the exclusions whose Source is that scope (the
    // merge CONCATENATES scopes, so each file owns its own names). Derive that
    // slice from the new merged set + the recorded Sources, sorted for a stable
    // on-disk order.
    let mut scope_excluded: Vec<String> = excluded
        .iter()
        .filter(|n| state.session.mcp_sources.get(*n).copied() == Some(source))
        .cloned()
        .collect();
    scope_excluded.sort();
    scope_excluded.dedup();
    crate::session::SessionConfig::persist_excluded(&path, &scope_excluded)
        .map_err(|e| format!("could not persist MCP exclusion to {path}: {e}"))?;

    // The write stuck, so update the in-memory Session + manager to match, then
    // rebuild the tool set from the manager's fresh adapters.
    state.session.mcp_excluded = excluded;
    state.mcp.set_disabled(&name, disabled).await;
    refresh_session_tools(state);
    Ok(())
}

// The Authenticate action (ADR-0065 Phase D, qwen's Authenticate step): run the
// OAuth browser flow for the named server, store the token, then re-attach the
// server (reusing the reconnect path so tools re-discover under the fresh Bearer)
// and rebuild the tool set. Progress lines (the copy-the-URL hint + the auth URL)
// stream back over the events broadcast as they arrive, so the dialog (Phase E)
// can render qwen's messages live. Runs INLINE on the actor loop like the other
// live MCP ops, so the browser flow's await holds the loop for its duration (the
// operator is mid-dialog, not mid-Run - the same exclusivity the reconnect takes).
pub(super) async fn mcp_authenticate(state: &mut AgentState, name: String) -> Result<(), String> {
    // The server must be known and carry an OAuth block, and the manager must have
    // a token store to save into (always so outside the empty test manager).
    let (oauth, server_url) = state
        .mcp
        .oauth_target(&name)
        .ok_or_else(|| format!("MCP server {name:?} has no OAuth configuration"))?;
    let tokens_path = state
        .mcp
        .oauth_tokens_path()
        .ok_or_else(|| "no MCP OAuth token store configured".to_string())?
        .to_string();

    // The progress sink forwards each OAuth signal onto the events broadcast as an
    // `McpAuthProgress` (qwen's OauthDisplayMessage / OauthAuthUrl). A clone of the
    // broadcast sender + the server name is moved into the boxed closure.
    let events = state.events.clone();
    let server = name.clone();
    let sink: crate::mcp::oauth::ProgressSink = Box::new(move |progress| {
        let event = match progress {
            crate::mcp::oauth::OAuthProgress::Message(message) => {
                Event::mcp_auth_message(server.clone(), message)
            }
            crate::mcp::oauth::OAuthProgress::AuthUrl(url) => {
                Event::mcp_auth_url(server.clone(), url)
            }
        };
        let _ = events.send(event);
    });

    // Run the flow through a provider over the SAME token store the connect-time
    // injection reads (so a stored token lands where the next attach finds it).
    let storage = crate::mcp::oauth::McpOAuthTokenStorage::new(tokens_path);
    let provider = crate::mcp::oauth::McpOAuthProvider::new(storage);
    provider
        .authenticate(&name, &oauth, server_url.as_deref(), sink)
        .await
        .map_err(|reason| format!("OAuth authentication failed for {name:?}: {reason}"))?;

    // Re-attach the server so its tools re-discover under the fresh Bearer (the
    // token is now stored, so `connect_one`'s injection picks it up), then rebuild
    // the tool set - the same path Reconnect takes.
    mcp_reconnect(state, name).await;
    Ok(())
}

// The Clear Authentication action (ADR-0065 Phase D, qwen `handleClearAuth`):
// delete the server's stored OAuth token, disconnect it (dropping its
// authenticated tools without disabling it), and rebuild the tool set. The
// disconnect leaves the server enabled so a later Authenticate/Reconnect
// re-attaches it. Runs INLINE like the other live MCP ops.
pub(super) async fn mcp_clear_auth(state: &mut AgentState, name: String) -> Result<(), String> {
    let tokens_path = state
        .mcp
        .oauth_tokens_path()
        .ok_or_else(|| "no MCP OAuth token store configured".to_string())?
        .to_string();
    crate::mcp::oauth::McpOAuthTokenStorage::new(tokens_path)
        .delete(&name)
        .map_err(|e| format!("could not clear MCP OAuth token for {name:?}: {e}"))?;
    // Drop the authenticated conn + tools (the server stays enabled), then rebuild
    // the tool set so the next Run no longer sees the server's tools.
    state.mcp.disconnect(&name);
    refresh_session_tools(state);
    Ok(())
}
