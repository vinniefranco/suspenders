//! The Active Model operations the Agent actor delegates to (ADR-0033, ADR-0037):
//! the `/model` swap and its off-actor window enrichment, plus the `/model`
//! selector's off-actor model listing. Carved out of [`super`] so the actor keeps
//! the message dispatch and this module owns the Active-Model mechanics.
//!
//! The two fetches (`spawn_model_enrichment`, `spawn_list_models`) run OFF the
//! actor loop (ADR-0011/0017: never block the loop on the network): they clone
//! the boundary + Session and answer back over the actor's own mpsc / a oneshot,
//! so a down host never stalls the actor.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::agent::{AgentState, Msg};
use crate::llm::ProviderModels;
use crate::llm::model::Model;

// The SetModel swap (ADR-0033 amendment): the whole Model swaps - the scoped
// id resolves against the Session's fixed Provider set (Catalog figures for
// known built-in models, config synthesis otherwise), and the per-Model
// budget invariants are re-checked (ADR-0037) so a pick that cannot fit is
// rejected here with the reason, never accepted and exploded later. The next
// spawned Run snapshots it; an in-flight Run is unaffected. A rejected pick
// leaves the Active Model as-is and the Err rides back to the caller.
pub(super) fn swap_active_model(state: &mut AgentState, scoped: &str) -> Result<(), String> {
    let model = state.session.resolve_model(scoped)?;
    state.session.validate_model_budget(&model)?;
    state.model = model.clone();
    // The picked Model resolved sync from config/fallback; enrich its window
    // from the server OFF the actor (ADR-0037, ADR-0011/0017: never block the
    // loop on the network). The Active Model applies now on its guessed window;
    // the enriched window folds in when the reply lands - guarded on the pick
    // still being active (see `apply_enriched_model`).
    spawn_model_enrichment(state, model);
    Ok(())
}

// The off-actor window enrichment for a `/model` swap (ADR-0037): clone the
// boundary and the Session, discover the server window, and message the enriched
// Model back to the actor. A discovery failure yields the sync-resolved Model
// unchanged, so the swap always lands.
fn spawn_model_enrichment(state: &AgentState, model: Model) {
    let llm = Arc::clone(&state.llm);
    let session = state.session.clone();
    let self_tx = state.self_tx.clone();
    tokio::spawn(async move {
        let enriched = session.enrich_model_window(llm.as_ref(), model).await;
        let _ = self_tx.send(Msg::EnrichedModel(enriched));
    });
}

// Folds an off-actor enriched Model back onto the Active Model (ADR-0037). Guards
// on the scoped id still matching: a second `/model` swap between the spawn and
// this reply must win, so a stale enrichment for the superseded pick is dropped.
pub(super) fn apply_enriched_model(state: &mut AgentState, model: Model) {
    if state.model.scoped_id() == model.scoped_id() {
        state.model = model;
    }
}

// The ListModels fetch, OFF the actor (ADR-0011/0017: never block the actor
// loop on the network). Clone the boundary and the Session's fixed Provider
// set, then answer the oneshot from the spawned task: `llm::offerings` walks
// every Provider - live discovery for customs, the Catalog for built-ins
// (ADR-0037).
pub(super) fn spawn_list_models(
    state: &AgentState,
    reply: oneshot::Sender<Vec<ProviderModels>>,
) {
    let llm = Arc::clone(&state.llm);
    let providers = state.session.providers.clone();
    tokio::spawn(async move {
        let _ = reply.send(crate::llm::offerings(llm.as_ref(), &providers).await);
    });
}
