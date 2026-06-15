use crate::prelude::*;
use crate::sse::extract_claude_output_text;
use crate::sse::extract_claude_output_text_from_sse;
use crate::usage::parse_rate_limit_headers;
use crate::usage::synthesize_rate_limit_from_error;
use crate::util::expand_home;
use crate::util::http_client;
use crate::util::truncate_text;

pub(crate) async fn call_claude_messages_api(
    state: &AppState,
    account: &UpstreamAccount,
    payload: &RelayRequest,
) -> Result<UpstreamCallResult, UpstreamCallError> {
    let body = json!({
        "model": payload.model,
        "system": CLAUDE_CODE_SYSTEM_PREFIX,
        "messages": [{
            "role": "user",
            "content": payload.prompt
        }],
        "max_tokens": 1024,
        "stream": false
    });
    // Refresh-on-401/403 like the proxy path; without it, relay calls in the
    // token-expiry window fail even though a refresh would have succeeded.
    let (response, _account) = send_claude_upstream_with_refresh(state, account, &body)
        .await
        .map_err(|e| UpstreamCallError {
            message: e,
            rate_limit_snapshot: None,
        })?;
    let status = response.status();
    let rate_limit_snapshot = parse_rate_limit_headers(response.headers());
    let body = response
        .text()
        .await
        .map_err(|e| UpstreamCallError {
            message: format!("failed to read claude response body: {}", e),
            rate_limit_snapshot: rate_limit_snapshot.clone(),
        })?;

    if !status.is_success() {
        let fallback_snapshot =
            rate_limit_snapshot
                .clone()
                .or_else(|| synthesize_rate_limit_from_error("claude", status, &body));
        return Err(UpstreamCallError {
            message: format!(
                "claude upstream error {}: {}",
                status.as_u16(),
                truncate_text(&body, 500)
            ),
            rate_limit_snapshot: fallback_snapshot,
        });
    }

    if let Some(text) = extract_claude_output_text_from_sse(&body) {
        return Ok(UpstreamCallResult {
            output_text: text,
            rate_limit_snapshot,
        });
    }
    let output_text = extract_claude_output_text(&body).ok_or_else(|| UpstreamCallError {
        message: format!(
            "claude response did not contain output text, raw body: {}",
            truncate_text(&body, 500)
        ),
        rate_limit_snapshot: rate_limit_snapshot.clone(),
    })?;
    Ok(UpstreamCallResult {
        output_text,
        rate_limit_snapshot,
    })
}


pub(crate) async fn fetch_claude_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    let client = http_client();
    let token = account.access_token.trim();
    if token.is_empty() {
        return Err("claude account has empty access token".to_string());
    }

    let req = apply_claude_auth_headers(
        client
            .get("https://api.anthropic.com/v1/models")
            .header("Accept", "application/json"),
        token,
    );

    let response = req
        .send()
        .await
        .map_err(|e| format!("failed to fetch claude models: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read claude models response body: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "claude models api error {}: {}",
            status.as_u16(),
            truncate_text(&body, 400)
        ));
    }

    let value: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid claude models response: {}", e))?;
    let arr = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "claude models response missing `data` field".to_string())?;

    let mut out = Vec::new();
    for item in arr {
        let slug = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if slug.is_empty() {
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
        return Err("no claude models found for this account".to_string());
    }
    Ok(out)
}


/// Claude Code OAuth client + token endpoint (mirrors `claude_auth.go`).
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_OAUTH_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

#[derive(Debug, Serialize)]
struct ClaudeRefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
    scope: &'static str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeRefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

/// Fetch the Claude account's email from the OAuth profile endpoint. Opaque
/// `sk-ant-oat` tokens carry no identity claims (unlike Codex JWTs), so this is
/// the only way to label a Claude account with its real account email.
pub(crate) async fn fetch_claude_account_email(account: &UpstreamAccount) -> Option<String> {
    let token = account.access_token.trim();
    if !token.starts_with("sk-ant-oat") {
        return None;
    }
    let resp = http_client()
        .get("https://api.anthropic.com/api/oauth/profile")
        .bearer_auth(token)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.pointer("/account/email")
        .and_then(|e| e.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether an account is an OAuth account that can be refreshed.
pub(crate) fn claude_account_refreshable(account: &UpstreamAccount) -> bool {
    account.access_token.trim().starts_with("sk-ant-oat")
        && !account.refresh_token.trim().is_empty()
}

/// Call the Claude OAuth token endpoint with a refresh token.
pub(crate) async fn request_claude_token_refresh(
    refresh_token: &str,
) -> Result<ClaudeRefreshResponse, String> {
    let client = http_client();
    let response = client
        .post(CLAUDE_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&ClaudeRefreshRequest {
            grant_type: "refresh_token",
            refresh_token,
            client_id: CLAUDE_OAUTH_CLIENT_ID,
            scope: CLAUDE_OAUTH_SCOPE,
        })
        .send()
        .await
        .map_err(|e| format!("failed to call claude token refresh endpoint: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed reading claude token refresh response: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "claude token refresh failed {}: {}",
            status.as_u16(),
            truncate_text(&body, 400)
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid claude token refresh response: {}", e))
}

/// Refresh a Claude account's OAuth tokens and persist the result. The
/// single-flight / mark-dead / persist mechanics live in the shared
/// `provider::refresh_account_tokens`; only the token-endpoint call and its
/// field mapping are Claude-specific.
pub(crate) async fn refresh_claude_account_tokens(
    state: &AppState,
    account: &UpstreamAccount,
) -> Result<UpstreamAccount, String> {
    crate::provider::refresh_account_tokens(state, account, |refresh_token| async move {
        let refresh = request_claude_token_refresh(&refresh_token).await?;
        let access_token = refresh.access_token.unwrap_or_default().trim().to_string();
        if access_token.is_empty() {
            return Err("claude token refresh response missing access_token".to_string());
        }
        let expires_at = refresh
            .expires_in
            .filter(|s| *s > 0)
            .map(|s| Utc::now() + chrono::Duration::seconds(s));
        Ok(crate::provider::TokenUpdate {
            access_token,
            refresh_token: refresh.refresh_token,
            id_token: None,
            account_id: None,
            expires_at,
        })
    })
    .await
}

/// Send to Claude, transparently refreshing the OAuth token on 401/403 and
/// retrying once (mirrors Codex's `send_codex_upstream_with_refresh`).
pub(crate) async fn send_claude_upstream_with_refresh(
    state: &AppState,
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<(reqwest::Response, UpstreamAccount), String> {
    let first = send_claude_upstream(account, payload).await?;
    let status = first.status();
    let auth_failure = status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN;
    if !auth_failure || !claude_account_refreshable(account) {
        return Ok((first, account.clone()));
    }
    let refreshed = refresh_claude_account_tokens(state, account).await?;
    info!(
        "claude token refreshed for account {}, retrying proxy request",
        refreshed.account_label
    );
    let retried = send_claude_upstream(&refreshed, payload).await?;
    Ok((retried, refreshed))
}

/// Proactive refresh loop for Claude OAuth tokens (shared skeleton in
/// `provider::run_token_refresh_loop`). Ported from `usage_tracking.go`.
pub(crate) async fn run_claude_token_refresh(state: AppState) {
    crate::provider::run_token_refresh_loop(
        state,
        crate::provider::Provider::Claude,
        claude_account_refreshable,
        claude_needs_refresh,
        |state, account| Box::pin(refresh_claude_account_tokens(state, account)),
    )
    .await
}

fn claude_needs_refresh(account: &UpstreamAccount, now: DateTime<Utc>) -> bool {
    crate::provider::token_needs_refresh(account, now, account.runtime.expires_at)
}

pub(crate) async fn send_claude_upstream(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let client = http_client();
    let token = account.access_token.trim();
    if token.is_empty() {
        return Err("connected claude account has empty access token".to_string());
    }
    let is_oauth = token.starts_with("sk-ant-oat");
    let model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("");

    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("Accept", "text/event-stream, application/json");
    // Auth WITHOUT anthropic-beta here: the fingerprint sets the full beta list
    // (including oauth-2025-04-20 when applicable). reqwest's .header() appends,
    // so setting it in both places would emit a duplicate header.
    req = apply_claude_auth_headers_base(req, token);
    req = crate::fingerprint::claude::apply_claude_fingerprint(req, model, is_oauth);

    // For OAuth requests, serialize ourselves so we can stamp the CCH checksum
    // into the billing-attribution system block before sending.
    req = if is_oauth {
        let body = serde_json::to_string(payload)
            .map_err(|e| format!("failed serializing claude payload: {}", e))?;
        let body = crate::fingerprint::claude::replace_cch(&body);
        req.body(body)
    } else {
        req.json(payload)
    };

    req.send()
        .await
        .map_err(|e| format!("failed to call claude upstream: {}", e))
}


pub(crate) fn apply_claude_auth_headers(
    req: reqwest::RequestBuilder,
    token: &str,
) -> reqwest::RequestBuilder {
    let req = apply_claude_auth_headers_base(req, token);
    if token.starts_with("sk-ant-oat") {
        // Claude Code OAuth tokens are only accepted with the OAuth beta header.
        req.header("anthropic-beta", "oauth-2025-04-20")
    } else {
        req
    }
}

/// Auth + version + content-type only, WITHOUT anthropic-beta — for callers that
/// set their own beta list (the message-send fingerprint path).
pub(crate) fn apply_claude_auth_headers_base(
    req: reqwest::RequestBuilder,
    token: &str,
) -> reqwest::RequestBuilder {
    let version = crate::fingerprint::claude::CC_ANTHROPIC_VERSION;
    if token.starts_with("sk-ant-oat") {
        req.bearer_auth(token)
            .header("anthropic-version", version)
            .header("Content-Type", "application/json")
    } else {
        req.header("x-api-key", token)
            .header("anthropic-version", version)
            .header("Content-Type", "application/json")
    }
}

/// The system-prompt prefix Claude Code OAuth tokens require: Anthropic rejects
/// `/v1/messages` calls whose first system block doesn't identify as Claude Code.
/// Single source of truth lives in the fingerprint module (the CCH checksum is
/// computed over this exact string — the two must never diverge).
pub(crate) const CLAUDE_CODE_SYSTEM_PREFIX: &str = crate::fingerprint::claude::CC_SYSTEM_PREFIX;

/// Claude local clients may send extra top-level fields (for local runtime
/// features) that Anthropic `/v1/messages` rejects with "Extra inputs are not
/// permitted". Keep only the fields `/v1/messages` accepts (the allowlist below
/// tracks the current API schema) to avoid 400 schema errors.
pub(crate) fn sanitize_claude_messages_payload(payload: &mut Value) {
    let obj = match payload.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    obj.retain(|k, _| {
        matches!(
            k.as_str(),
            "model"
                | "messages"
                | "max_tokens"
                | "system"
                | "metadata"
                | "stop_sequences"
                | "stream"
                | "temperature"
                | "tool_choice"
                | "tools"
                | "top_k"
                | "top_p"
                | "thinking"
                | "service_tier"
                | "context_management"
                | "output_config"
                | "output_format"
                | "container"
                | "mcp_servers"
                | "cache_control"
                | "betas"
        )
    });
}


pub(crate) fn parse_claude_credentials_json(raw: &str) -> Result<ParsedClaudeCreds, String> {
    let parsed: ClaudeCredentialsJson =
        serde_json::from_str(raw).map_err(|e| format!("invalid credentials.json: {}", e))?;

    if let Some(oauth) = parsed.claude_ai_oauth {
        let access_token = oauth
            .access_token
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if !access_token.is_empty() {
            let refresh_token = oauth
                .refresh_token
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            let expires_at = oauth
                .expires_at
                .and_then(DateTime::<Utc>::from_timestamp_millis);
            return Ok(ParsedClaudeCreds {
                access_token,
                refresh_token,
                expires_at,
            });
        }
    }

    if let Some(api_key) = parsed.api_key.map(|v| v.trim().to_string()) {
        if !api_key.is_empty() {
            return Ok(ParsedClaudeCreds {
                access_token: api_key,
                refresh_token: String::new(),
                expires_at: None,
            });
        }
    }

    Err("credentials.json missing claudeAiOauth.accessToken or api_key".to_string())
}


pub(crate) fn claude_local_candidate_paths(source_path: Option<&str>) -> Vec<String> {
    if let Some(path) = source_path.map(str::trim).filter(|p| !p.is_empty()) {
        return vec![expand_home(path)];
    }
    let mut paths = Vec::new();
    // Primary location matches Claude Code: $CLAUDE_CONFIG_DIR (or ~/.claude) + `.credentials.json`.
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| expand_home("~/.claude"));
    paths.push(format!("{}/.credentials.json", config_dir.trim_end_matches('/')));
    // Legacy / alternative locations (some installs / platforms).
    paths.push(expand_home("~/.claude/.credentials.json"));
    paths.push(expand_home("~/.config/claude/.credentials.json"));
    paths.push(expand_home("~/.claude/credentials.json"));
    paths.push(expand_home("~/.anthropic/credentials.json"));
    paths.dedup();
    paths
}

/// On macOS, Claude Code stores OAuth credentials in the login Keychain under the
/// service name "Claude Code-credentials" (account = $USER), not in a file. Read
/// that JSON blob via the `security` CLI so "import Claude" works on a Mac that
/// logged in through Claude Code.
pub(crate) async fn read_claude_keychain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let user = std::env::var("USER").ok().filter(|u| !u.trim().is_empty())?;
    // tokio::process, not std: `security` can block on a Keychain authorization
    // prompt, and a synchronous wait would pin a tokio worker thread for the
    // whole time.
    let output = tokio::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &user,
            "-w",
            "-s",
            "Claude Code-credentials",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}


pub(crate) fn read_claude_token_from_env() -> Option<String> {
    for key in ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
        if let Ok(val) = std::env::var(key) {
            let token = val.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Claude credential-file wire types (`.credentials.json` / Keychain blob).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeCredentialsJson {
    #[serde(rename = "claudeAiOauth")]
    pub(crate) claude_ai_oauth: Option<ClaudeOauthData>,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeOauthData {
    #[serde(rename = "accessToken")]
    pub(crate) access_token: Option<String>,
    #[serde(rename = "refreshToken", default)]
    pub(crate) refresh_token: Option<String>,
    /// Expiry as Unix milliseconds, as written by Claude Code.
    #[serde(rename = "expiresAt", default)]
    pub(crate) expires_at: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct ParsedClaudeCreds {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at: Option<DateTime<Utc>>,
}