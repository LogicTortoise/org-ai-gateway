use crate::prelude::*;
use crate::auth::jwt_chatgpt_account_id;
use crate::auth::jwt_email;
use crate::auth::jwt_exp;
use crate::client_config::GATEWAY_PROVIDER_KEY;
use crate::sse::extract_output_text;
use crate::sse::extract_output_text_from_sse;
use crate::usage::parse_rate_limit_headers;
use crate::usage::synthesize_rate_limit_from_error;
use crate::util::codex_http_client;
use crate::util::truncate_text;

pub(crate) async fn call_codex_responses_api(
    state: &AppState,
    account: &UpstreamAccount,
    payload: &RelayRequest,
) -> Result<UpstreamCallResult, UpstreamCallError> {
    // Build the Responses payload and send it through the SAME path the proxy
    // uses (fingerprint + refresh-on-401), so relay behavior can't drift.
    let body = json!({
        "model": payload.model,
        "instructions": "You are a helpful assistant.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": payload.prompt
            }]
        }],
        "store": false,
        "stream": true
    });
    let (response, _account) = send_codex_upstream_with_refresh(state, account, &body)
        .await
        .map_err(|message| UpstreamCallError {
            message,
            rate_limit_snapshot: None,
        })?;

    let status = response.status();
    let rate_limit_snapshot = parse_rate_limit_headers(response.headers());
    let body = response
        .text()
        .await
        .map_err(|e| UpstreamCallError {
            message: format!("failed to read codex response body: {}", e),
            rate_limit_snapshot: rate_limit_snapshot.clone(),
        })?;

    if !status.is_success() {
        let fallback_snapshot =
            rate_limit_snapshot
                .clone()
                .or_else(|| synthesize_rate_limit_from_error("codex", status, &body));
        return Err(UpstreamCallError {
            message: format!(
                "codex upstream error {}: {}",
                status.as_u16(),
                truncate_text(&body, 500)
            ),
            rate_limit_snapshot: fallback_snapshot,
        });
    }

    if let Some(text) = extract_output_text_from_sse(&body) {
        return Ok(UpstreamCallResult {
            output_text: text,
            rate_limit_snapshot,
        });
    }

    let output_text = extract_output_text(&body).ok_or_else(|| UpstreamCallError {
        message: format!(
            "codex response did not contain output text, raw body: {}",
            truncate_text(&body, 500)
        ),
        rate_limit_snapshot: rate_limit_snapshot.clone(),
    })?;
    Ok(UpstreamCallResult {
        output_text,
        rate_limit_snapshot,
    })
}


pub(crate) async fn fetch_codex_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    let client = codex_http_client();
    let bearer = account.bearer();
    if bearer.is_empty() {
        return Err("codex account has empty access token".to_string());
    }

    let mut req = client
        .get("https://chatgpt.com/backend-api/codex/models?client_version=0.125.0")
        .bearer_auth(bearer)
        .header("Accept", "application/json");
    if !account.account_id.trim().is_empty() {
        req = req.header("ChatGPT-Account-ID", account.account_id.trim());
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("failed to fetch codex models: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read codex models response body: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "codex models api error {}: {}",
            status.as_u16(),
            truncate_text(&body, 400)
        ));
    }

    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid codex models response: {}", e))?;
    let arr = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex models response missing `models` field".to_string())?;

    let mut out = Vec::new();
    for item in arr {
        let slug = item
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if slug.is_empty() {
            continue;
        }
        let supported_in_api = item
            .get("supported_in_api")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !supported_in_api {
            continue;
        }
        let display_name = item
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(&slug)
            .to_string();
        out.push(ModelInfo { slug, display_name });
    }
    if out.is_empty() {
        return Err("no supported codex models found for this account".to_string());
    }
    Ok(out)
}


pub(crate) fn ensure_codex_payload_defaults(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        if !obj.contains_key("instructions") {
            obj.insert(
                "instructions".to_string(),
                Value::String("You are a helpful assistant.".to_string()),
            );
        }
        // Privacy guard for the shared pool: FORCE store=false, overriding
        // whatever the client sent. With a shared upstream account, a client
        // that asked for store=true would have its turn persisted server-side
        // under the pool account's identity (and could surface in the account
        // owner's cloud surfaces). Never let that happen.
        obj.insert("store".to_string(), Value::Bool(false));
        // Same reason: don't let a turn be chained onto / saved under a
        // server-side response history keyed to the pool account.
        obj.remove("previous_response_id");
        obj.insert("stream".to_string(), Value::Bool(true));
        if let Some(Value::String(input)) = obj.get("input") {
            let msg = json!([{
                "type":"message",
                "role":"user",
                "content":[{"type":"input_text","text":input}]
            }]);
            obj.insert("input".to_string(), msg);
        }
    }
}


/// Send to Codex, transparently refreshing the OAuth token on 401 and retrying
/// once (mirrors Claude's `send_claude_upstream_with_refresh`). Accounts that
/// can't be refreshed (API-key-only) get their original 401 response back, so
/// callers see the upstream's real error body instead of a synthetic refresh
/// failure.
pub(crate) async fn send_codex_upstream_with_refresh(
    state: &AppState,
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<(reqwest::Response, UpstreamAccount), String> {
    let first = send_codex_upstream(account, payload).await?;
    if first.status() != StatusCode::UNAUTHORIZED || !codex_account_refreshable(account) {
        return Ok((first, account.clone()));
    }
    let refreshed = refresh_codex_account_tokens(state, account).await?;
    info!(
        "codex token refreshed for account {}, retrying proxy request",
        refreshed.account_label
    );
    let retried = send_codex_upstream(&refreshed, payload).await?;
    Ok((retried, refreshed))
}


/// Refresh a Codex account's OAuth tokens and persist the result. The
/// single-flight / mark-dead / persist mechanics live in the shared
/// `provider::refresh_account_tokens`; only the token-endpoint call and its
/// field mapping are Codex-specific.
pub(crate) async fn refresh_codex_account_tokens(
    state: &AppState,
    account: &UpstreamAccount,
) -> Result<UpstreamAccount, String> {
    crate::provider::refresh_account_tokens(state, account, |refresh_token| async move {
        let refresh = request_codex_token_refresh(&refresh_token).await?;
        let access_token = refresh.access_token.unwrap_or_default().trim().to_string();
        if access_token.is_empty() {
            return Err("token refresh response missing access_token".to_string());
        }
        let id_token = refresh.id_token;
        let account_id = id_token.as_deref().and_then(jwt_chatgpt_account_id);
        let expires_at = jwt_exp(&access_token);
        Ok(crate::provider::TokenUpdate {
            access_token,
            refresh_token: refresh.refresh_token,
            id_token,
            account_id,
            expires_at,
        })
    })
    .await
}


/// The ChatGPT account email, read offline from the id_token (preferred) or the
/// access token's `https://api.openai.com/profile.email` claim. Used to label
/// the account with its real identity instead of the generic default.
pub(crate) fn codex_account_email(account: &UpstreamAccount) -> Option<String> {
    jwt_email(account.id_token.trim())
        .or_else(|| jwt_email(account.access_token.trim()))
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
}

/// Whether a Codex account is an OAuth account we can refresh (has a refresh
/// token and a JWT-shaped access token). API-key-only records aren't refreshable.
pub(crate) fn codex_account_refreshable(account: &UpstreamAccount) -> bool {
    !account.refresh_token.trim().is_empty()
        && account.access_token.trim().split('.').count() == 3
}

/// Decide whether a Codex token is due for a proactive refresh. Same policy as
/// Claude (`provider::token_needs_refresh`), with the JWT `exp` claim as the
/// expiry fallback when the runtime doesn't carry one.
fn codex_needs_refresh(account: &UpstreamAccount, now: DateTime<Utc>) -> bool {
    let exp = account
        .runtime
        .expires_at
        .or_else(|| jwt_exp(account.access_token.trim()));
    crate::provider::token_needs_refresh(account, now, exp)
}

/// Proactive refresh loop for Codex OAuth tokens (shared skeleton in
/// `provider::run_token_refresh_loop`). Without this, a pooled Codex account's
/// access token silently lapses and the account only "comes back" if its owner
/// happens to re-import `auth.json` — so shared Codex accounts slowly die.
pub(crate) async fn run_codex_token_refresh(state: AppState) {
    crate::provider::run_token_refresh_loop(
        state,
        crate::provider::Provider::Codex,
        codex_account_refreshable,
        codex_needs_refresh,
        |state, account| Box::pin(refresh_codex_account_tokens(state, account)),
    )
    .await
}


pub(crate) async fn request_codex_token_refresh(refresh_token: &str) -> Result<CodexRefreshResponse, String> {
    let client = codex_http_client();
    let response = client
        .post("https://auth.openai.com/oauth/token")
        .header("Content-Type", "application/json")
        .json(&CodexRefreshRequest {
            client_id: CODEX_REFRESH_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await
        .map_err(|e| format!("failed to call token refresh endpoint: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed reading token refresh response: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "token refresh failed {}: {}",
            status.as_u16(),
            truncate_text(&body, 400)
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid token refresh response: {}", e))
}


pub(crate) async fn send_codex_upstream(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let client = codex_http_client();
    let bearer = account.bearer();
    if bearer.is_empty() {
        return Err("connected codex account has empty access token".to_string());
    }
    let mut req = crate::fingerprint::codex::apply_codex_fingerprint(
        client
            .post("https://chatgpt.com/backend-api/codex/responses")
            .bearer_auth(bearer)
            .header("Accept", "application/json, text/event-stream"),
    )
    .json(payload);
    if !account.account_id.trim().is_empty() {
        req = req.header("ChatGPT-Account-ID", account.account_id.trim());
    }
    req.send()
        .await
        .map_err(|e| format!("failed to call codex upstream: {}", e))
}


pub(crate) fn codex_ws_upstream_url() -> String {
    std::env::var("CODEX_UPSTREAM_WS_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_WS_UPSTREAM_URL.to_string())
}


pub(crate) fn parse_codex_auth_json(raw: &str) -> Result<ParsedCodexCreds, String> {
    let parsed: CodexAuthJson =
        serde_json::from_str(raw).map_err(|e| format!("invalid auth.json: {}", e))?;
    let tokens = parsed
        .tokens
        .ok_or_else(|| "auth.json missing `tokens` object".to_string())?;
    let access_token = tokens.access_token.unwrap_or_default().trim().to_string();
    if access_token.is_empty() {
        return Err("auth.json missing tokens.access_token".to_string());
    }
    Ok(ParsedCodexCreds {
        access_token,
        refresh_token: tokens.refresh_token.unwrap_or_default(),
        id_token: tokens.id_token.unwrap_or_default(),
        account_id: tokens.account_id.unwrap_or_default(),
    })
}


pub(crate) fn codex_bootstrap_payload(
    user_id: &str,
    base_url: &str,
) -> Result<CodexBootstrapResponse, String> {
    // Display-only snippet describing what `应用` merges into config.toml.
    let config_toml = format!(
        "# 仅合并以下内容进 ~/.codex/config.toml（其余保持不变）\n\
         model_provider = \"{key}\"\n\n\
         [model_providers.{key}]\n\
         name = \"Codex via org-ai-gateway\"\n\
         base_url = \"{url}\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true\n",
        key = GATEWAY_PROVIDER_KEY,
        url = base_url,
    );

    let steps = vec![
        "1) 备份并合并 ~/.codex/config.toml：只新增网关 provider，其余设置原样保留".to_string(),
        "2) auth.json 完全不改动 —— 本地 Codex 仍是你自己的真实账号，不会触发账号校验".to_string(),
        "3) 直接发请求即可：对话流量经网关，用共享池账号转发到上游".to_string(),
        "4) 客户端与终端都适用，且无需退出重启".to_string(),
    ];

    Ok(CodexBootstrapResponse {
        user_id: user_id.to_string(),
        codex_config_toml: config_toml,
        codex_auth_json: json!("auth.json 不会被修改"),
        steps,
    })
}

// ---------------------------------------------------------------------------
// Codex wire types (auth.json shape, OAuth refresh, bootstrap response).
// ---------------------------------------------------------------------------

pub(crate) const CODEX_REFRESH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const DEFAULT_CODEX_WS_UPSTREAM_URL: &str =
    "wss://chatgpt.com/backend-api/codex/realtime";

#[derive(Debug, Deserialize)]
pub(crate) struct CodexAuthJson {
    pub(crate) tokens: Option<CodexTokens>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexTokens {
    pub(crate) access_token: Option<String>,
    pub(crate) refresh_token: Option<String>,
    pub(crate) id_token: Option<String>,
    pub(crate) account_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ParsedCodexCreds {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    pub(crate) account_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodexRefreshRequest<'a> {
    pub(crate) client_id: &'static str,
    pub(crate) grant_type: &'static str,
    pub(crate) refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexRefreshResponse {
    pub(crate) access_token: Option<String>,
    pub(crate) refresh_token: Option<String>,
    pub(crate) id_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodexBootstrapResponse {
    pub(crate) user_id: String,
    pub(crate) codex_config_toml: String,
    pub(crate) codex_auth_json: Value,
    pub(crate) steps: Vec<String>,
}