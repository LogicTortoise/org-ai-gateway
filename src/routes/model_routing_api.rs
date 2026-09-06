use crate::auth::identify_caller;
use crate::prelude::*;
use crate::provider::chains::ChainSlot;
use crate::provider::model_routing::{ModelRouting, persist_and_publish_model_routing};

fn routing_envelope(routing: ModelRouting) -> Value {
    json!({
        "routing": routing,
        "allowed": {
            "codex": ChainSlot::Codex.allowed_providers(),
            "claude": ChainSlot::Claude.allowed_providers(),
        },
    })
}

/// `GET /v1/provider/model-routing` — return the canonical routing document
/// plus the per-slot list of legal providers, so the UI can build the route
/// editor's provider picker without an extra round trip. Mirrors the chains
/// API's `{routing|chains, allowed}` envelope.
pub(crate) async fn get_model_routing(State(state): State<AppState>) -> impl IntoResponse {
    let routing = state.model_routing.read().await.clone();
    (StatusCode::OK, Json(routing_envelope(routing))).into_response()
}

/// `PUT /v1/provider/model-routing` — replace the entire routing document.
/// Input is normalized once via `ModelRouting::normalize`, then published
/// atomically (disk → memory, under the persistence lock). Returns the
/// canonical persisted document so the client renders exactly what the
/// gateway stored, without re-running normalization on its side.
pub(crate) async fn update_model_routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(candidate): Json<ModelRouting>,
) -> impl IntoResponse {
    if !identify_caller(&headers).owner_trusted {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "只有受信任的本机用户可以修改模型路由"})),
        )
            .into_response();
    }
    let canonical = match ModelRouting::normalize(candidate) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    if let Err(e) = persist_and_publish_model_routing(&state, canonical.clone()).await {
        error!("failed persisting model routing: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("保存失败: {}", e)})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(routing_envelope(canonical))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::chains::ChainSlot;
    use crate::provider::model_routing::{ModelRoute, ModelRouting, SlotRouting};

    #[test]
    fn get_envelope_includes_allowed_providers_per_slot() {
        // Build a canonical document directly (no I/O) and verify the
        // envelope shape matches the chains API: per-slot `allowed` list
        // mirrors `ChainSlot::allowed_providers()` exactly.
        let routing = ModelRouting {
            version: 1,
            slots: [(
                "codex".into(),
                SlotRouting {
                    rules: [(
                        "gpt-5".into(),
                        ModelRoute {
                            provider: "codex".into(),
                            model: "gpt-5".into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        };
        let envelope = routing_envelope(routing);
        let codex_allowed = envelope["allowed"]["codex"].as_array().unwrap();
        let claude_allowed = envelope["allowed"]["claude"].as_array().unwrap();
        assert_eq!(
            codex_allowed,
            json!(ChainSlot::Codex.allowed_providers())
                .as_array()
                .unwrap()
        );
        assert_eq!(
            claude_allowed,
            json!(ChainSlot::Claude.allowed_providers())
                .as_array()
                .unwrap()
        );
    }

    #[test]
    fn update_normalizes_before_persisting() {
        // The API normalizes input; this is the unit-level guarantee that
        // whatever the caller puts in `ModelRouting::normalize` is what
        // comes back out of the wrapper. The end-to-end envelope shape is
        // covered by the live get_envelope_includes_allowed_providers_per_slot.
        let raw = ModelRouting {
            version: 1,
            slots: [(
                "  CODEX  ".into(),
                SlotRouting {
                    rules: [(
                        " GPT-5.6-LUNA ".into(),
                        ModelRoute {
                            provider: "  MiniMax  ".into(),
                            model: "  MiniMax-M3  ".into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        };
        let canon = ModelRouting::normalize(raw).expect("normalize");
        let codex = canon.slots.get("codex").expect("slot present");
        assert_eq!(codex.rules["gpt-5.6-luna"].provider, "minimax");
        assert_eq!(codex.rules["gpt-5.6-luna"].model, "MiniMax-M3");
    }
}
