use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::auth::identify_caller;
use crate::pool::select_healthy_account;
use crate::provider::claude::fetch_claude_models;
use crate::provider::codex::fetch_codex_models;
use crate::provider::codex::fetch_codex_models_raw;
use crate::provider::codex::CODEX_MODEL_TEMPLATE_JSON;
use crate::provider::cursor::fetch_cursor_models;
use crate::provider::ollama::fetch_ollama_models;
use crate::provider::deepseek::deepseek_model_catalog;
use crate::provider::glm::fetch_glm_models;
use crate::provider::glm::glm_model_catalog;
use crate::provider::kimi::fetch_kimi_models;
use crate::provider::kimi::kimi_model_catalog;
use crate::provider::minimax::minimax_model_catalog;
use crate::provider::trae::fetch_trae_models;
use crate::provider::trae::trae_model_catalog;

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

/// Kimi models: prefer the LIVE list from a connected Kimi account's OpenAI-
/// compatible `/models` endpoint, falling back to the static catalog
/// (`KIMI_MODELS` override, else the built-in list) when no account is connected
/// or the live fetch fails. Since Kimi's base URL defaults to Moonshot's public
/// endpoint, the catalog is always resolvable.
pub(crate) async fn get_kimi_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let mut account_id = String::new();
    let mut models = kimi_model_catalog();
    if let Some(account) = select_healthy_account(&state, "kimi", &user_id, None, false, false).await {
        account_id = account.id.clone();
        match fetch_kimi_models(&account).await {
            Ok(live) if !live.is_empty() => models = live,
            Ok(_) => {}
            Err(e) => warn!("kimi live model list failed, using static catalog: {}", e),
        }
    }

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "kimi".to_string(),
            account_id,
            owner_user_id: user_id,
            models,
        }),
    )
        .into_response()
}

/// Trae models: prefer the LIVE list from the connected sidecar's `/v1/models`
/// (authoritative — the sidecar owns the id → real Trae config-name mapping),
/// falling back to the static catalog (`TRAE_MODELS` override, else the built-in
/// list mirroring the sidecar's `models.json`) when no account is connected or
/// the sidecar is unreachable.
pub(crate) async fn get_trae_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let mut account_id = String::new();
    let mut models = trae_model_catalog();
    if let Some(account) = select_healthy_account(&state, "trae", &user_id, None, false, false).await {
        account_id = account.id.clone();
        match fetch_trae_models(&account).await {
            Ok(live) if !live.is_empty() => models = live,
            Ok(_) => {}
            Err(e) => warn!("trae live model list failed, using static catalog: {}", e),
        }
    }

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: "trae".to_string(),
            account_id,
            owner_user_id: user_id,
            models,
        }),
    )
        .into_response()
}

/// MiniMax models. STATIC only: MiniMax's Anthropic-compatible surface exposes no
/// `/models` endpoint, so there is no live list to prefer — the catalog comes from
/// `MINIMAX_MODELS` if set, else the built-in list. `account_id` is still filled
/// in when an account exists so the UI can show which one a selection would use.
pub(crate) async fn get_minimax_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    provider_static_models(state, headers, "minimax", minimax_model_catalog()).await
}

/// DeepSeek models. STATIC only, deliberately: DeepSeek's `GET /models` lists the
/// ids of their OpenAI surface (`deepseek-chat`, `deepseek-reasoner`), not the ids
/// this Anthropic surface documents, so a live fetch would offer models that then
/// get silently remapped. Override with `DEEPSEEK_MODELS`.
pub(crate) async fn get_deepseek_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    provider_static_models(state, headers, "deepseek", deepseek_model_catalog()).await
}

/// Shared body for providers whose catalog is static (no live `/models` fetch).
async fn provider_static_models(
    state: AppState,
    headers: HeaderMap,
    provider: &str,
    models: Vec<ModelInfo>,
) -> Response {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };
    let account_id = select_healthy_account(&state, provider, &user_id, None, false, false)
        .await
        .map(|a| a.id)
        .unwrap_or_default();

    (
        StatusCode::OK,
        Json(ProviderModelsResponse {
            provider: provider.to_string(),
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
    let mut models = match fetch_codex_models_raw(&account).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
        }
    };
    // Merge the live upstream catalog with the gateway's own advertised
    // aliases (default: the GPT-5.6 Bedrock-style ids — `gpt-5.6-sol` /
    // `gpt-5.6-terra` / `gpt-5.6-luna` — that Codex recognises but the Codex
    // backend's catalog doesn't list). Without this, Codex's `list_models`
    // refresh silently drops any model the user has set in `config.toml`
    // that isn't a real Codex-backend id, and surfaces an "unknown model"
    // warning at startup.
    //
    // Synthesize each alias by deep-copying the FIRST real upstream entry
    // (so the synthetic shape matches the live schema byte-for-byte — Codex
    // 0.144+ `ModelsClient` rejects entries missing fields the real catalog
    // carries, e.g. `supports_reasoning_summaries`, `context_window`,
    // `default_reasoning_level`, …) and overwriting only `slug` /
    // `display_name`. When the upstream catalog is empty, fall back to a
    // schema-only build from `CODEX_MODEL_TEMPLATE_JSON`.
    let schema_template = models.first().cloned();
    for slug in advertised_models() {
        if models.iter().any(|m| {
            m.get("slug").and_then(|v| v.as_str()) == Some(slug.as_str())
        }) {
            continue;
        }
        models.push(synthetic_codex_model(&slug, schema_template.as_ref()));
    }
    (StatusCode::OK, Json(json!({ "models": models }))).into_response()
}

/// Build a Codex-backend-shaped entry for a slug the gateway wants to
/// advertise but that doesn't exist in the upstream Codex backend catalog.
///
/// If a `reference` (typically the first live upstream entry) is supplied,
/// start by deep-copying it and overwrite only `slug` / `display_name`. This
/// is the safe path — the synthetic inherits every schema field the live
/// catalog carries, so Codex 0.144+ decoders accept it.
///
/// Falls back to a schema-only build from `CODEX_MODEL_TEMPLATE_JSON` when
/// the live catalog is empty (e.g. every Codex account just refreshed off).
fn synthetic_codex_model(slug: &str, reference: Option<&Value>) -> Value {
    if let Some(tmpl) = reference {
        let mut obj = tmpl.clone();
        if let Some(root) = obj.as_object_mut() {
            root.insert("slug".to_string(), Value::String(slug.to_string()));
            root.insert("display_name".to_string(), Value::String(slug.to_string()));
        }
        return obj;
    }
    // Fallback: template only has base_instructions + model_messages. Codex
    // may reject these for missing fields, but it's better than nothing
    // when no live account can be reached.
    let template: Value = serde_json::from_str(CODEX_MODEL_TEMPLATE_JSON)
        .expect("CODEX_MODEL_TEMPLATE_JSON must be valid JSON (build-time file)");
    let mut obj = template;
    if let Some(root) = obj.as_object_mut() {
        root.insert("slug".to_string(), Value::String(slug.to_string()));
        root.insert("display_name".to_string(), Value::String(slug.to_string()));
    }
    obj
}

/// Models the gateway advertises in `/v1/models` on top of whatever the live
/// upstream OpenAI catalog returns. Defaults to the Bedrock-style GPT-5.6
/// family that Codex's `model_provider_info` module hard-codes
/// (`openai.gpt-5.6-sol` / `…-terra` / `…-luna`) — these are valid Codex-side
/// model ids but not in OpenAI's public catalog, so a passthrough `/v1/models`
/// would silently drop them and Codex would warn about an unknown model.
///
/// Override via `OAG_ADVERTISED_MODELS` (comma-separated). Set to empty string
/// to disable. Whitespace around entries is trimmed; empty entries are
/// dropped.
fn advertised_models() -> Vec<String> {
    const DEFAULT: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];
    match std::env::var("OAG_ADVERTISED_MODELS") {
        Ok(v) if v.trim().is_empty() => Vec::new(),
        Ok(v) => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => DEFAULT.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(test)]
mod advertised_models_tests {
    use super::advertised_models;
    use std::sync::Mutex;

    // Tests stomp on the process env, so serialise them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_to_bedrock_gpt_5_6_family() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("OAG_ADVERTISED_MODELS");
        let m = advertised_models();
        assert_eq!(m, vec!["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]);
    }

    #[test]
    fn override_replaces_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OAG_ADVERTISED_MODELS", "custom-a, custom-b ,");
        let m = advertised_models();
        assert_eq!(m, vec!["custom-a", "custom-b"]);
        std::env::remove_var("OAG_ADVERTISED_MODELS");
    }

    #[test]
    fn empty_string_disables() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OAG_ADVERTISED_MODELS", "");
        assert!(advertised_models().is_empty());
        std::env::remove_var("OAG_ADVERTISED_MODELS");
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelsResponse {
    pub(crate) provider: String,
    pub(crate) account_id: String,
    pub(crate) owner_user_id: String,
    pub(crate) models: Vec<ModelInfo>,
}