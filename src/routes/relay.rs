use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::pool::select_healthy_account;
use crate::pool::storage::append_audit;
use crate::provider::claude::call_claude_messages_api;
use crate::provider::codex::call_codex_responses_api;
use crate::provider::cursor::call_cursor_relay_api;
use crate::provider::normalize_model_for_provider;
use crate::provider::route_provider;

/// Legacy MVP entry point (`POST /v1/gateway/relay`). Superseded by the
/// protocol-native proxy routes in `routes::proxy`, which add retry/failover
/// and token accounting; kept for backwards compatibility with early clients.
pub(crate) async fn relay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RelayRequest>,
) -> impl IntoResponse {
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

    let request_id = Uuid::new_v4().to_string();
    let routed_provider = route_provider(&payload.model, payload.preferred_provider.as_deref());
    let owned_only = match crate::quota::enforce_user_quota(&state, &routed_provider, &user_id, true).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let effective_model = normalize_model_for_provider(&payload.model, &routed_provider);
    let upstream_payload = RelayRequest {
        prompt: payload.prompt.clone(),
        model: effective_model.clone(),
        preferred_provider: payload.preferred_provider.clone(),
    };
    let selected_account =
        select_healthy_account(&state, &routed_provider, &user_id, None, owned_only, false).await;
    let selected_account = match selected_account {
        Some(account) => account,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("provider `{}` has no connected account. Please connect an account first.", routed_provider),
                    "hint": "Call /v1/provider/connect (or use the UI section '步骤1 绑定上游账号')."
                })),
            )
                .into_response();
        }
    };

    let upstream_result = match call_upstream(&state, &selected_account, &upstream_payload).await {
        Ok(v) => v,
        Err(err) => {
            if let Some(snapshot) = err.rate_limit_snapshot.clone() {
                crate::capacity::store_rate_limit(&state, &selected_account.id, snapshot).await;
            }
            let failed_audit = AuditRecord {
                request_id: request_id.clone(),
                user_id: user_id.clone(),
                model: effective_model.clone(),
                routed_provider: routed_provider.clone(),
                upstream_account_id: selected_account.id.clone(),
                upstream_owner_user_id: selected_account.owner_user_id.clone(),
                prompt_length: payload.prompt.chars().count(),
                output_length: 0,
                status: format!("upstream_error: {}", err.message),
                created_at: Utc::now(),
                tokens: TokenUsage::default(),
            };
            if let Err(write_err) = append_audit(&state, &failed_audit).await {
                error!("failed writing upstream-error audit record: {}", write_err);
            }
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": err.message, "provider": routed_provider })),
            )
                .into_response();
        }
    };

    let audit = AuditRecord {
        request_id: request_id.clone(),
        user_id: user_id.clone(),
        model: effective_model.clone(),
        routed_provider: routed_provider.clone(),
        upstream_account_id: selected_account.id.clone(),
        upstream_owner_user_id: selected_account.owner_user_id.clone(),
        prompt_length: payload.prompt.chars().count(),
        output_length: upstream_result.output_text.chars().count(),
        status: "success".to_string(),
        created_at: Utc::now(),
        tokens: TokenUsage::default(),
    };

    if let Some(snapshot) = upstream_result.rate_limit_snapshot.clone() {
        crate::capacity::store_rate_limit(&state, &selected_account.id, snapshot).await;
    }

    // The upstream call already succeeded (and consumed quota); a failed audit
    // write must not turn that into a client-visible error. Mirrors the policy
    // in `write_proxy_audit`.
    if let Err(e) = append_audit(&state, &audit).await {
        error!("failed writing audit record: {}", e);
    }

    let response = RelayResponse {
        request_id,
        user_id,
        routed_provider,
        upstream_account_id: selected_account.id,
        upstream_owner_user_id: selected_account.owner_user_id,
        model: effective_model,
        status: "success".to_string(),
        message: "request routed and executed via real upstream".to_string(),
        output_text: upstream_result.output_text,
    };

    (StatusCode::OK, Json(response)).into_response()
}


pub(crate) async fn call_upstream(
    state: &AppState,
    account: &UpstreamAccount,
    payload: &RelayRequest,
) -> Result<UpstreamCallResult, UpstreamCallError> {
    match account.provider.as_str() {
        "codex" => call_codex_responses_api(state, account, payload).await,
        "claude" => call_claude_messages_api(state, account, payload).await,
        "cursor" => call_cursor_relay_api(account, payload).await,
        other => Err(UpstreamCallError {
            message: format!("unsupported provider `{}`", other),
            rate_limit_snapshot: None,
        }),
    }
}


