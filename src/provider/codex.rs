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

/// Same upstream call as `fetch_codex_models` but returns the raw JSON
/// objects verbatim (no field thinning). Used by the `/v1/models` route
/// because Codex 0.147+'s `ModelsClient` deserialises the body into
/// `ModelsResponse { models: Vec<ModelInfo> }` where `ModelInfo` has ~30
/// required fields — if we send the thin `{slug, display_name}` shape,
/// every entry fails to decode and `list_models` refresh errors out.
pub(crate) async fn fetch_codex_models_raw(
    account: &UpstreamAccount,
) -> Result<Vec<Value>, String> {
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
        .cloned()
        .ok_or_else(|| "codex models response missing `models` field".to_string())?;

    // Drop entries with `supported_in_api=false` (same filter as the thin
    // version) so the caller doesn't have to redo it; everything else passes
    // through with all original fields intact.
    let out: Vec<Value> = arr
        .into_iter()
        .filter(|item| {
            item.get("supported_in_api")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        })
        .collect();
    if out.is_empty() {
        return Err("no supported codex models found for this account".to_string());
    }
    Ok(out)
}


/// Fields `/backend-api/codex/responses` accepts (Responses API). Anything
/// else — including OpenAI Chat Completions fields the Codex CLI 0.147+ probe
/// path can send — is stripped before the upstream call. A stray `max_tokens`
/// or `temperature` here yields `400 Unsupported parameter` and aborts the
/// client's in-flight task with no retry path (`ErrorClass::Invalid`).
///
/// `max_tokens` is the Chat Completions name; the Responses API equivalent
/// is `max_output_tokens`. We rename it so the client doesn't have to know
/// which wire shape the gateway routes through.
const RESPONSES_API_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "max_output_tokens",
    "metadata",
    "parallel_tool_calls",
    "prompt_cache_key",
    "reasoning",
    "safety_identifier",
    "service_tier",
    "store",
    "stream",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
    "truncation",
    "user",
];

pub(crate) fn ensure_codex_payload_defaults(payload: &mut Value) {
    let obj = match payload.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // Translate the Chat Completions name to the Responses API equivalent
    // before the allowlist runs, so a client that sends `max_tokens` (the
    // legacy OpenAI field name) still gets the right limit applied upstream.
    if let Some(v) = obj.remove("max_tokens") {
        obj.entry("max_output_tokens".to_string())
            .or_insert(v);
    }

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

    // Drop fields the upstream rejects. Must run AFTER the defaults above so
    // our own injections (`instructions`, `store`, `stream`) survive even if
    // a future version of the Responses API stops accepting one of them.
    obj.retain(|k, _| RESPONSES_API_FIELDS.contains(&k.as_str()));
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

/// Embed-time JSON containing the `base_instructions` and `model_messages`
/// blocks copied verbatim from a real Codex backend catalog entry. Used by
/// `routes::models_api::synthetic_codex_model` to populate the two
/// required-by-presence fields that Codex 0.147+'s `ModelsClient` enforces
/// on every entry, so slugs we advertise (e.g. the Bedrock-style GPT-5.6
/// family) decode cleanly without the upstream backend needing to list them.
///
/// Source: captured live from
/// `https://chatgpt.com/backend-api/codex/models` for the `gpt-5.5` slug
/// during 2026-08 debugging. Regenerate by re-fetching and re-saving the
/// file if a future Codex version adds new required fields.
pub(crate) const CODEX_MODEL_TEMPLATE_JSON: &str =
    include_str!("codex_model_template.json");

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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_max_tokens_to_max_output_tokens() {
        // Codex CLI 0.147+ can send `max_tokens` (Chat Completions name) on
        // the Responses path; the upstream rejects it with 400. The rename
        // happens before the allowlist so the new field survives.
        let mut p = json!({ "model": "gpt-5.6-sol", "max_tokens": 4096 });
        ensure_codex_payload_defaults(&mut p);
        assert_eq!(p["max_output_tokens"], 4096);
        assert!(p.get("max_tokens").is_none());
    }

    #[test]
    fn rename_preserves_explicit_max_output_tokens() {
        // If the client already used the right name, don't clobber it with
        // a stale Chat Completions value.
        let mut p = json!({ "max_tokens": 100, "max_output_tokens": 8000 });
        ensure_codex_payload_defaults(&mut p);
        assert_eq!(p["max_output_tokens"], 8000);
        assert!(p.get("max_tokens").is_none());
    }

    #[test]
    fn strips_chat_completions_only_fields() {
        // These don't exist on the Responses API; forwarding them yields
        // `400 Unsupported parameter` and aborts the in-flight task.
        let mut p = json!({
            "model": "gpt-5.6-sol",
            "input": "hi",
            "frequency_penalty": 0.5,
            "presence_penalty": 0.5,
            "logit_bias": {"50256": -100},
            "stop": ["\n\n"],
            "n": 1,
            "seed": 42,
        });
        ensure_codex_payload_defaults(&mut p);
        assert!(p.get("frequency_penalty").is_none());
        assert!(p.get("presence_penalty").is_none());
        assert!(p.get("logit_bias").is_none());
        assert!(p.get("stop").is_none());
        assert!(p.get("n").is_none());
        assert!(p.get("seed").is_none());
    }

    #[test]
    fn keeps_responses_api_fields() {
        let mut p = json!({
            "model": "gpt-5.6-sol",
            "input": "hi",
            "instructions": "be terse",
            "temperature": 0.2,
            "top_p": 0.9,
            "tools": [{"type": "function", "name": "search"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "low"},
            "metadata": {"trace_id": "abc"},
            "prompt_cache_key": "session-1",
            "text": {"format": {"type": "text"}},
            "user": "u-1",
        });
        ensure_codex_payload_defaults(&mut p);
        assert_eq!(p["temperature"], 0.2);
        assert_eq!(p["top_p"], 0.9);
        assert_eq!(p["parallel_tool_calls"], true);
        assert_eq!(p["prompt_cache_key"], "session-1");
        assert_eq!(p["user"], "u-1");
        // Privacy defaults still apply.
        assert_eq!(p["store"], false);
        assert_eq!(p["stream"], true);
        assert!(p.get("previous_response_id").is_none());
    }
}