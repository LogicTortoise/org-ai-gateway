use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::auth::identify_caller;
use crate::pool::select_healthy_account;
use crate::provider::claude::fetch_claude_models;
use crate::provider::codex::fetch_codex_models;
use crate::provider::cursor::fetch_cursor_models;
use crate::provider::ollama::fetch_ollama_models;
use crate::provider::glm::fetch_glm_models;
use crate::provider::glm::glm_model_catalog;

pub(crate) async fn get_codex_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };

    let selected_account = select_healthy_account(&state, "codex", &user_id, None, false, false).await;
    let selected_account = match selected_account {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error":"no codex account available for this user",
                    "hint":"先执行步骤1绑定账号"
                })),
            )
                .into_response();
        }
    };

    let models = match fetch_codex_models(&selected_account).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": e,
                    "provider":"codex"
                })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "codex".to_string(),
            account_id: selected_account.id,
            owner_user_id: selected_account.owner_user_id,
            models,
        }),
    )
        .into_response()
}


pub(crate) async fn get_claude_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };

    let selected_account = select_healthy_account(&state, "claude", &user_id, None, false, false).await;
    let selected_account = match selected_account {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error":"no claude account available for this user",
                    "hint":"先执行步骤1绑定 Claude 账号"
                })),
            )
                .into_response();
        }
    };

    let models = match fetch_claude_models(&selected_account).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": e,
                    "provider":"claude"
                })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "claude".to_string(),
            account_id: selected_account.id,
            owner_user_id: selected_account.owner_user_id,
            models,
        }),
    )
        .into_response()
}


pub(crate) async fn get_cursor_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let selected_account = select_healthy_account(&state, "cursor", &user_id, None, false, false).await;
    let selected_account = match selected_account {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "no cursor account available for this user",
                    "hint": "先执行步骤1绑定 Cursor 账号"
                })),
            )
                .into_response();
        }
    };

    let models = match fetch_cursor_models(&selected_account).await {
        Ok(m) => m,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e, "provider": "cursor" })))
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "cursor".to_string(),
            account_id: selected_account.id,
            owner_user_id: selected_account.owner_user_id,
            models,
        }),
    )
        .into_response()
}


pub(crate) async fn get_ollama_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let selected_account = select_healthy_account(&state, "ollama", &user_id, None, false, false).await;
    let selected_account = match selected_account {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "no ollama account available for this user",
                    "hint": "先连接本地 ollama (POST /v1/provider/connect/ollama)"
                })),
            )
                .into_response();
        }
    };

    let models = match fetch_ollama_models(&selected_account).await {
        Ok(m) => m,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e, "provider": "ollama" })))
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "ollama".to_string(),
            account_id: selected_account.id,
            owner_user_id: selected_account.owner_user_id,
            models,
        }),
    )
        .into_response()
}


/// GLM models: prefer the LIVE list from a connected GLM account's OpenAI-compatible
/// `/models` endpoint (so new models like glm-5.2 appear automatically), and fall
/// back to the static catalog (`GLM_MODELS` override, else the built-in list) when
/// no account is connected or the live fetch fails.
pub(crate) async fn get_glm_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let mut account_id = String::new();
    let mut models = glm_model_catalog();
    if let Some(account) = select_healthy_account(&state, "glm", &user_id, None, false, false).await {
        account_id = account.id.clone();
        match fetch_glm_models(&account).await {
            Ok(live) if !live.is_empty() => models = live,
            Ok(_) => {}
            Err(e) => warn!("glm live model list failed, using static catalog: {}", e),
        }
    }

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "glm".to_string(),
            account_id,
            owner_user_id: user_id,
            models,
        }),
    )
        .into_response()
}

pub(crate) async fn proxy_models_codex(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let account =
        select_healthy_account(&state, "codex", &user_id, None, false, !caller.owner_trusted).await;
    let account = match account {
        Some(a) => a,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"no codex account available"})),
            )
                .into_response();
        }
    };
    let models = match fetch_codex_models(&account).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
        }
    };
    (StatusCode::OK, Json(json!({ "models": models }))).into_response()
}


pub(crate) async fn proxy_models_openai(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let account =
        select_healthy_account(&state, "codex", &user_id, None, false, !caller.owner_trusted).await;
    let account = match account {
        Some(a) => a,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"no codex account available"})),
            )
                .into_response();
        }
    };
    let models = match fetch_codex_models(&account).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
        }
    };
    let data: Vec<Value> = models
        .into_iter()
        .map(|m| {
            json!({
                "id": m.slug,
                "object": "model",
                "created": 0,
                "owned_by": "org-ai-gateway"
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "object": "list", "data": data })),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelsResponse {
    pub(crate) provider: String,
    pub(crate) account_id: String,
    pub(crate) owner_user_id: String,
    pub(crate) models: Vec<ModelInfo>,
}