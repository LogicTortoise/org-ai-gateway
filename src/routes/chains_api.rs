//! Read/update the global provider-priority chains (`/v1/provider/chains`).
//! GET is open to any identified caller (the UI reads it to render the panel);
//! PUT is restricted to owner-trusted callers (it changes routing for everyone).
use crate::prelude::*;
use crate::auth::identify_caller;
use crate::provider::chains::{persist_chains, ChainCfg, ChainSlot, ProviderChains};

/// `GET /v1/provider/chains` — return the current chains plus, for the UI, the
/// set of providers legal in each slot.
pub(crate) async fn get_chains(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let _caller = identify_caller(&headers);
    let chains = state.chains.read().await.clone();
    (
        StatusCode::OK,
        Json(json!({
            "chains": chains,
            "allowed": {
                "codex": ChainSlot::Codex.allowed_providers(),
                "claude": ChainSlot::Claude.allowed_providers(),
            },
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChainsUpdateRequest {
    /// New config for the Codex slot (omit to leave it unchanged).
    #[serde(default)]
    pub(crate) codex: Option<ChainCfg>,
    /// New config for the Claude slot (omit to leave it unchanged).
    #[serde(default)]
    pub(crate) claude: Option<ChainCfg>,
}

/// `PUT /v1/provider/chains` — replace one or both slots' chains. Each incoming
/// chain is validated (illegal providers dropped, de-duplicated, non-empty
/// guaranteed) before being stored and persisted.
pub(crate) async fn update_chains(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChainsUpdateRequest>,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    if !caller.owner_trusted {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "只有受信任的本机用户可以修改优先级链路" })),
        )
            .into_response();
    }

    {
        let mut chains = state.chains.write().await;
        let mut updated: ProviderChains = chains.clone();
        if let Some(cfg) = &payload.codex {
            updated.apply_validated(ChainSlot::Codex, cfg.mode, &cfg.providers);
        }
        if let Some(cfg) = &payload.claude {
            updated.apply_validated(ChainSlot::Claude, cfg.mode, &cfg.providers);
        }
        *chains = updated;
    }

    if let Err(e) = persist_chains(&state).await {
        error!("failed persisting provider chains: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("保存失败: {}", e) })),
        )
            .into_response();
    }

    let chains = state.chains.read().await.clone();
    (StatusCode::OK, Json(json!({ "chains": chains }))).into_response()
}
