//! The `/mcp` dialog's IMPURE adapter seam (ADR-0065 Phase E, ADR-0001/0011):
//! the async orchestration that talks to the Agent and posts events off the
//! select loop. This is the ONLY part of `mcp_command` that awaits or spawns; the
//! dialog fold ([`super::McpDialog`]) and the render builders ([`super::view`])
//! stay pure. The Composer routes a picked [`super::McpFold::Act`] here and the
//! command router routes the opening effect here.

use crate::event::Event;
use crate::ui::AdapterCtx;

use super::super::screen::{Effect, Screen};
use super::McpAction;

/// Opens the `/mcp` dialog (ADR-0065 Phase E). ALWAYS a live fetch, exactly like
/// `/model` ([`super::super::model_command::run`]): spawn a task that awaits
/// `agent.mcp_views()` OFF the select loop (ADR-0011) and posts an
/// [`Event::McpDialogReady`] through `ctx.selector_tx`; the injected event
/// arrives at the loop's `selector_rx` arm and fills the dialog. `generation` is
/// the activation counter the committing [`Effect::McpCommand`] carried, echoed
/// on the fill so a stale fetch is dropped. The overlay itself was already opened
/// (to a Loading state) by the Composer on commit; this only kicks the fetch.
pub(in crate::ui) async fn run(screen: Screen, ctx: &AdapterCtx<'_>, generation: u64) -> Screen {
    spawn_fetch(ctx, generation);
    screen
}

/// Runs a picked SERVER_DETAIL action against the Agent (ADR-0065 Phase E), then
/// re-fetches views so the dialog reflects the change (qwen's `reloadServers`).
/// `ViewTools` never reaches here (it pushes a step, no Agent call). Each Agent
/// call is fire-and-forget for the render - the fresh views fill lands later via
/// the `selector_rx` arm, exactly like the initial fetch. A failed action still
/// re-fetches (the view then shows the truthful post-failure state).
pub(in crate::ui) async fn act(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    action: McpAction,
    server: String,
    generation: u64,
) -> Screen {
    match action {
        // ViewTools navigates in the pure fold and never emits an action.
        McpAction::ViewTools => return screen,
        McpAction::Reconnect => {
            let _ = ctx.agent.mcp_reconnect(server).await;
        }
        McpAction::Disable => {
            let _ = ctx.agent.mcp_set_enabled(server, false).await;
        }
        McpAction::Enable => {
            let _ = ctx.agent.mcp_set_enabled(server, true).await;
        }
        McpAction::ClearAuth => {
            let _ = ctx.agent.mcp_clear_auth(server).await;
        }
        McpAction::Authenticate => {
            // The OAuth flow streams McpAuthProgress over the events broadcast as
            // it runs (the AUTHENTICATE step folds those); the Result settles when
            // the browser flow finishes. Run it off-loop so the select loop keeps
            // pumping the progress events - a blocking await here would starve the
            // very events the step renders.
            let agent = ctx.agent.clone();
            let tx = ctx.selector_tx.clone();
            tokio::spawn(async move {
                let _ = agent.mcp_authenticate(server).await;
                // Re-fetch once the flow settles so the dialog reflects the fresh
                // tokens/tools when the user pops back to the detail step, and
                // refresh the footer health count (ADR-0065 Phase F).
                let servers = agent.mcp_views().await;
                let _ = tx.send(Event::mcp_health(crate::mcp::mcp_offline_count(&servers)));
                let _ = tx.send(Event::mcp_dialog_ready(generation, servers));
            });
            return screen;
        }
    }
    spawn_fetch(ctx, generation);
    screen
}

/// Spawns the `mcp_views()` fetch off the select loop and posts the resulting
/// [`Event::McpDialogReady`] back through `ctx.selector_tx` (the shared analog of
/// `/model`'s `list_models` fetch). The `mcp_views()` query never fails (a dead
/// Agent answers an empty list), so there is no failure event to post - the empty
/// list already reads as the "No MCP servers configured." state. The same fetch
/// refreshes the footer MCP-health count (ADR-0065 Phase F): the dialog's
/// `McpDialogReady` is consumed by the Composer and never reaches the Screen that
/// holds the pill count, so a paired [`Event::McpHealth`] carries the recomputed
/// offline count to the Screen.
fn spawn_fetch(ctx: &AdapterCtx<'_>, generation: u64) {
    let agent = ctx.agent.clone();
    let tx = ctx.selector_tx.clone();
    tokio::spawn(async move {
        let servers = agent.mcp_views().await;
        let _ = tx.send(Event::mcp_health(crate::mcp::mcp_offline_count(&servers)));
        let _ = tx.send(Event::mcp_dialog_ready(generation, servers));
    });
}

/// The effect the Composer emits to open the dialog (routed by the adapter to
/// [`run`]). A thin re-export of the [`Effect::McpCommand`] shape so the Composer
/// need not name the effect variant inline. `generation` is the activation the
/// fill echoes.
pub(in crate::ui) fn open_effect(generation: u64) -> Effect {
    Effect::McpCommand { generation }
}
