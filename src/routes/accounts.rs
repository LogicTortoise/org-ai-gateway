use crate::prelude::*;
use crate::provider::claude::ParsedClaudeCreds;
use crate::provider::codex::ParsedCodexCreds;
use crate::auth::extract_user_id;
use crate::pool::owner_needs_protection;
use crate::pool::storage::account_usage_map;
use crate::pool::storage::persist_all_accounts;
use crate::pool::storage::read_audit_records;
use crate::pool::storage::upsert_account;
use crate::provider::claude::claude_local_candidate_paths;
use crate::provider::claude::parse_claude_credentials_json;
use crate::provider::claude::read_claude_keychain;
use crate::provider::claude::read_claude_token_from_env;
use crate::provider::codex::parse_codex_auth_json;
use crate::provider::cursor::cursor_token_expiry_from_str;
use crate::usage::compute_owner_usage_7d;
use crate::usage::refresh_rate_limits_from_usage;
use crate::util::expand_home;

/// Derive a dashboard-facing health snapshot from an account's live runtime
/// state. Status precedence mirrors the Go pool: dead > disabled > cooldown >
/// degraded (penalty > 2.0) > healthy.
pub(crate) fn account_health(account: &UpstreamAccount, now: DateTime<Utc>) -> AccountHealth {
    let rt = &account.runtime;
    let cooling = rt.rate_limit_until.map(|t| t > now).unwrap_or(false);
    let status = if rt.dead {
        "dead"
    } else if rt.disabled {
        "disabled"
    } else if cooling {
        "cooldown"
    } else if rt.penalty > 2.0 {
        "degraded"
    } else {
        "healthy"
    };
    AccountHealth {
        status: status.to_string(),
        dead: rt.dead,
        disabled: rt.disabled,
        penalty: rt.penalty,
        backoff_level: rt.backoff_level,
        rate_limit_until: if cooling { rt.rate_limit_until } else { None },
        expires_at: rt.expires_at,
        cyber_access: rt.cyber_access,
    }
}

/// Sanitize a client-supplied share cap: NaN/inf dropped, clamped to 0-100,
/// and 100 normalized to None ("no cap").
pub(crate) fn normalize_share_limit(raw: Option<f64>) -> Option<f64> {
    raw.filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 100.0))
        .filter(|v| *v < 100.0)
}

fn unauthorized(err: String) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response()
}

fn bad_request(body: Value) -> Response {
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// Default account label: the explicit label if given, else `<user>-<provider>`.
fn account_label_or_default(raw: &str, user_id: &str, provider: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("{}-{}", user_id, provider)
    } else {
        trimmed.to_string()
    }
}

/// Credential material a connect handler extracted for a new account.
struct ConnectCreds {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
    expires_at: Option<DateTime<Utc>>,
}

impl ConnectCreds {
    fn token_only(access_token: String, expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            access_token,
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            expires_at,
        }
    }
}

impl From<ParsedCodexCreds> for ConnectCreds {
    fn from(c: ParsedCodexCreds) -> Self {
        Self {
            access_token: c.access_token,
            refresh_token: c.refresh_token,
            id_token: c.id_token,
            account_id: c.account_id,
            expires_at: None,
        }
    }
}

impl From<ParsedClaudeCreds> for ConnectCreds {
    fn from(c: ParsedClaudeCreds) -> Self {
        Self {
            access_token: c.access_token,
            refresh_token: c.refresh_token,
            id_token: String::new(),
            account_id: String::new(),
            expires_at: c.expires_at,
        }
    }
}

/// Reject the gateway's own virtual tokens being re-imported as upstream creds.
fn reject_virtual_codex_creds(creds: &ParsedCodexCreds) -> Option<Response> {
    if creds.access_token.starts_with("user:") || creds.account_id.starts_with("pool-") {
        return Some(bad_request(json!({
            "error": "检测到虚拟 Token",
            "hint": "你当前加载的是网关的虚拟配置。请先在左侧【本地客户端配置】中点击【恢复本地备份】，找回真实的官方 Token 后再进行导入。"
        })));
    }
    None
}

/// Shared tail of every connect handler: build the account record, upsert it
/// into the pool, persist, and shape the response. The connect handlers only
/// differ in how they obtain `creds` — any account-field evolution happens here
/// once instead of in six copies.
// The three share-policy params mirror the request DTO fields one-to-one;
// bundling them into a struct would just rename the same tuple.
#[allow(clippy::too_many_arguments)]
async fn finish_connect(
    state: &AppState,
    user_id: String,
    provider: &str,
    account_label: String,
    creds: ConnectCreds,
    share_enabled: bool,
    share_limit_percent: Option<f64>,
    daily_token_limit: Option<u64>,
) -> Response {
    let mut account = UpstreamAccount {
        id: Uuid::new_v4().to_string(),
        owner_user_id: user_id,
        provider: provider.to_string(),
        account_label,
        access_token: creds.access_token,
        refresh_token: creds.refresh_token,
        id_token: creds.id_token,
        account_id: creds.account_id,
        api_key: String::new(),
        share_enabled,
        share_limit_percent: normalize_share_limit(share_limit_percent),
        daily_token_limit,
        created_at: Utc::now(),
        runtime: AccountRuntime {
            expires_at: creds.expires_at,
            ..AccountRuntime::default()
        },
    };

    // Label the account with its real upstream identity up front (the UI seeds
    // a generic label = current user id). Best-effort: on failure the health
    // probe's backfill retries later. Codex resolves offline; Claude calls its
    // profile endpoint.
    if crate::provider::label_is_generic(&account.account_label, &account.owner_user_id, provider) {
        if let Some(email) = crate::provider::account_identity_email(&account).await {
            account.account_label = email;
        }
    }

    // Rebind to the persisted record: on a re-import upsert_account returns the
    // EXISTING account's id/created_at, so the response must report those (the
    // freshly-minted Uuid here never enters the pool).
    let account = {
        let mut accounts = state.accounts.write().await;
        upsert_account(&mut accounts, account)
    };

    if let Err(e) = persist_all_accounts(state).await {
        error!("failed writing account record: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "failed persisting account record" })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(ConnectAccountResponse {
            account_id: account.id,
            owner_user_id: account.owner_user_id,
            provider: account.provider,
            account_label: account.account_label,
            share_enabled: account.share_enabled,
            share_limit_percent: account.share_limit_percent,
            daily_token_limit: account.daily_token_limit,
            created_at: account.created_at,
        }),
    )
        .into_response()
}

pub(crate) async fn connect_account_legacy(
    _state: State<AppState>,
    _headers: HeaderMap,
    Json(_payload): Json<Value>,
) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "legacy /v1/provider/connect is deprecated",
            "hint": "Use /v1/provider/connect/{codex|claude}/{local|auth-json}"
        })),
    )
        .into_response()
}


pub(crate) async fn connect_codex_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectImportRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };
    let account_label = account_label_or_default(&payload.account_label, &user_id, "codex");

    // A caller-supplied path is confined to $HOME; the default is operator-trusted.
    let expanded = match payload.source_path {
        Some(p) => match crate::util::resolve_confined_home_path(&p) {
            Ok(v) => v,
            Err(e) => return bad_request(json!({ "error": format!("invalid source_path: {}", e) })),
        },
        None => expand_home("~/.codex/auth.json"),
    };
    let raw = match tokio::fs::read_to_string(&expanded).await {
        Ok(v) => v,
        Err(e) => {
            return bad_request(json!({
                "error": format!("failed reading {}", expanded),
                "detail": e.to_string(),
                "hint": "请先在本机完成 Codex 登录，确保 ~/.codex/auth.json 存在"
            }));
        }
    };

    let creds = match parse_codex_auth_json(&raw) {
        Ok(v) => v,
        Err(e) => return bad_request(json!({ "error": e })),
    };
    if let Some(rejection) = reject_virtual_codex_creds(&creds) {
        return rejection;
    }

    finish_connect(
        &state,
        user_id,
        "codex",
        account_label,
        creds.into(),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}


pub(crate) async fn connect_codex_auth_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectImportRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };
    let account_label = account_label_or_default(&payload.account_label, &user_id, "codex");

    let raw = match payload.auth_json {
        Some(v) if !v.trim().is_empty() => v,
        _ => return bad_request(json!({ "error": "auth_json cannot be empty" })),
    };

    let creds = match parse_codex_auth_json(&raw) {
        Ok(v) => v,
        Err(e) => return bad_request(json!({ "error": e })),
    };
    if let Some(rejection) = reject_virtual_codex_creds(&creds) {
        return rejection;
    }

    finish_connect(
        &state,
        user_id,
        "codex",
        account_label,
        creds.into(),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}


pub(crate) async fn connect_claude_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectImportRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };
    let account_label = account_label_or_default(&payload.account_label, &user_id, "claude");

    // Confine a caller-supplied path to $HOME (defaults are operator-trusted).
    let confined_source = match payload.source_path.as_deref() {
        Some(p) => match crate::util::resolve_confined_home_path(p) {
            Ok(v) => Some(v),
            Err(e) => return bad_request(json!({ "error": format!("invalid source_path: {}", e) })),
        },
        None => None,
    };
    let candidate_paths = claude_local_candidate_paths(confined_source.as_deref());
    let mut read_errors: Vec<String> = Vec::new();
    let mut creds: Option<ParsedClaudeCreds> = None;

    // macOS: Claude Code keeps credentials in the Keychain, not a file. Try it first.
    if payload.source_path.is_none() {
        if let Some(raw) = read_claude_keychain().await {
            match parse_claude_credentials_json(&raw) {
                Ok(parsed) => creds = Some(parsed),
                Err(parse_err) => {
                    read_errors.push(format!("keychain:Claude Code-credentials (parse: {})", parse_err))
                }
            }
        }
    }

    for path in &candidate_paths {
        if creds.is_some() {
            break;
        }
        match tokio::fs::read_to_string(path).await {
            Ok(raw) => match parse_claude_credentials_json(&raw) {
                Ok(parsed) => {
                    creds = Some(parsed);
                    break;
                }
                Err(parse_err) => {
                    read_errors.push(format!("{} (parse: {})", path, parse_err));
                }
            },
            Err(err) => {
                read_errors.push(format!("{} ({})", path, err));
            }
        }
    }

    // Fallback: if user already exported token in current process env, allow direct import.
    if creds.is_none() {
        if let Some(token) = read_claude_token_from_env() {
            creds = Some(ParsedClaudeCreds {
                access_token: token,
                refresh_token: String::new(),
                expires_at: None,
            });
        }
    }

    let creds = match creds {
        Some(v) => v,
        None => {
            return bad_request(json!({
                "error": "failed reading local Claude credentials",
                "detail": read_errors.join(" | "),
                "hint": "请先登录 Claude Code，或设置 CLAUDE_CODE_OAUTH_TOKEN / ANTHROPIC_API_KEY 环境变量，或使用 /v1/provider/connect/claude/auth-json 手动导入。"
            }));
        }
    };

    finish_connect(
        &state,
        user_id,
        "claude",
        account_label,
        creds.into(),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}


pub(crate) async fn connect_claude_auth_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectImportRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };
    let account_label = account_label_or_default(&payload.account_label, &user_id, "claude");

    let raw = match payload.auth_json {
        Some(v) if !v.trim().is_empty() => v,
        _ => return bad_request(json!({ "error": "auth_json cannot be empty" })),
    };

    let creds = match parse_claude_credentials_json(&raw) {
        Ok(v) => v,
        Err(e) => return bad_request(json!({ "error": e })),
    };

    finish_connect(
        &state,
        user_id,
        "claude",
        account_label,
        creds.into(),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}


pub(crate) async fn list_accounts(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
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

    refresh_rate_limits_from_usage(&state).await;

    let audit_records = read_audit_records(&state.audit_file).await;
    let usage_map = account_usage_map(&audit_records);
    let accounts = state.accounts.read().await;
    let rate_limits = state.rate_limits.read().await.clone();
    // Keep the dashboard's owner-usage view fresh (and warm the guard's cache
    // between background refreshes).
    // Lock order: `accounts` before `owner_usage` — the same order the proxy
    // selection path uses. Taking them in the opposite order anywhere risks a
    // deadlock.
    let owner_usage_map = compute_owner_usage_7d(&audit_records, &accounts);
    *state.owner_usage.write().await = owner_usage_map.clone();
    let now = Utc::now();
    // BTreeMap, not HashMap: serde serializes it in sorted key order, so the
    // provider groups (and thus the dashboard cards) keep a stable order across
    // refreshes instead of reshuffling with HashMap's randomized iteration.
    let mut grouped: std::collections::BTreeMap<String, Vec<AccountSummary>> =
        std::collections::BTreeMap::new();

    for account in accounts.iter() {
        if account.owner_user_id == user_id || account.share_enabled {
            grouped
                .entry(account.provider.clone())
                .or_default()
                .push(AccountSummary {
                    id: account.id.clone(),
                    account_id: if account.account_id.trim().is_empty() {
                        account.id.clone()
                    } else {
                        account.account_id.clone()
                    },
                    owner_user_id: account.owner_user_id.clone(),
                    account_label: account.account_label.clone(),
                    share_enabled: account.share_enabled,
                    share_limit_percent: account.share_limit_percent,
                    daily_token_limit: account.daily_token_limit,
                    created_at: account.created_at,
                    usage: usage_map.get(&account.id).cloned().unwrap_or_default(),
                    rate_limit: rate_limits.get(&account.id).cloned(),
                    health: account_health(account, now),
                    owner_usage: owner_usage_map.get(&account.id).cloned().unwrap_or_default(),
                    owner_protected: account.share_enabled
                        && owner_needs_protection(account, &rate_limits, &owner_usage_map),
                });
        }
    }

    // Stable within-group order too (defensive: the source Vec is already
    // created_at-sorted, but upserts append).
    for accounts in grouped.values_mut() {
        accounts.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    }

    (
        StatusCode::OK,
        Json(ListAccountsResponse {
            current_user_id: user_id,
            grouped_accounts: grouped,
        }),
    )
        .into_response()
}


/// Connects a Cursor account. The credential is a `WorkosCursorSessionToken`
/// captured from a logged-in Cursor session; it is stored as the access token
/// and presented to `api2.cursor.sh` as a Bearer credential on each request.
pub(crate) async fn connect_cursor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectCursorRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };

    let session_token = payload.session_token.trim().to_string();
    if session_token.is_empty() {
        return bad_request(json!({
            "error": "session_token 不能为空",
            "hint": "从浏览器 Cookie 复制 WorkosCursorSessionToken 的值"
        }));
    }
    let account_label = account_label_or_default(&payload.account_label, &user_id, "cursor");

    let expires_at = cursor_token_expiry_from_str(&session_token);
    finish_connect(
        &state,
        user_id,
        "cursor",
        account_label,
        ConnectCreds::token_only(session_token, expires_at),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}

/// Default location of Cursor's `state.vscdb` per OS.
fn default_cursor_db_path() -> String {
    if cfg!(target_os = "macos") {
        "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb".to_string()
    } else if cfg!(target_os = "windows") {
        // %APPDATA%\Cursor\User\globalStorage\state.vscdb
        match std::env::var("APPDATA") {
            Ok(appdata) => format!("{}\\Cursor\\User\\globalStorage\\state.vscdb", appdata),
            Err(_) => "~/AppData/Roaming/Cursor/User/globalStorage/state.vscdb".to_string(),
        }
    } else {
        "~/.config/Cursor/User/globalStorage/state.vscdb".to_string()
    }
}

/// Read a single `ItemTable` value from Cursor's SQLite store via the `sqlite3`
/// CLI. Opens with a busy timeout so a concurrent Cursor write doesn't error.
/// Only the tiny `ItemTable` is touched — never the multi-GB chat-history table.
async fn read_cursor_item(db_path: &str, key: &str) -> Result<String, String> {
    let query = format!(
        "SELECT value FROM ItemTable WHERE key='{}';",
        key.replace('\'', "''")
    );
    // `.timeout` is a dot-command (silent); a `PRAGMA busy_timeout` would print
    // its own value line and corrupt the result.
    let output = tokio::process::Command::new("sqlite3")
        .arg("-cmd")
        .arg(".timeout 3000")
        .arg(db_path)
        .arg(&query)
        .output()
        .await
        .map_err(|e| format!("无法调用 sqlite3: {} (请确认系统已安装 sqlite3)", e))?;
    if !output.status.success() {
        return Err(format!(
            "读取 Cursor 数据库失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Import the locally signed-in Cursor account by reading `cursorAuth/accessToken`
/// straight out of Cursor's `state.vscdb` — the one-click analogue to
/// `connect_codex_local` (which reads `~/.codex/auth.json`).
pub(crate) async fn connect_cursor_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ConnectCursorRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };

    // A caller-supplied path is confined to $HOME; the per-OS default is
    // operator-trusted (and may legitimately sit outside $HOME, e.g. %APPDATA%).
    let db_path = match payload.source_path {
        Some(p) => match crate::util::resolve_confined_home_path(&p) {
            Ok(v) => v,
            Err(e) => return bad_request(json!({ "error": format!("invalid source_path: {}", e) })),
        },
        None => expand_home(&default_cursor_db_path()),
    };
    if !std::path::Path::new(&db_path).exists() {
        return bad_request(json!({
            "error": format!("未找到 Cursor 数据库: {}", db_path),
            "hint": "请先在本机安装并登录 Cursor"
        }));
    }

    let token = match read_cursor_item(&db_path, "cursorAuth/accessToken").await {
        Ok(t) => t,
        Err(e) => return bad_request(json!({ "error": e })),
    };
    if token.is_empty() {
        return bad_request(json!({
            "error": "Cursor 未登录或未找到 accessToken",
            "hint": "请先在本机 Cursor 完成登录后再导入"
        }));
    }

    // Best-effort: use the cached email as a friendly label.
    let email = read_cursor_item(&db_path, "cursorAuth/cachedEmail")
        .await
        .unwrap_or_default();
    let account_label = if !payload.account_label.trim().is_empty() {
        payload.account_label.trim().to_string()
    } else if !email.is_empty() {
        email
    } else {
        format!("{}-cursor", user_id)
    };

    let expires_at = cursor_token_expiry_from_str(&token);
    finish_connect(
        &state,
        user_id,
        "cursor",
        account_label,
        ConnectCreds::token_only(token, expires_at),
        payload.share_enabled,
        payload.share_limit_percent,
        payload.daily_token_limit,
    )
    .await
}


pub(crate) async fn delete_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(u) => u,
        Err(e) => return unauthorized(e),
    };

    let removed_account = {
        let mut accounts = state.accounts.write().await;
        accounts
            .iter()
            .position(|a| a.id == id && a.owner_user_id == user_id)
            .map(|i| accounts.remove(i))
    };

    let Some(removed_account) = removed_account else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "账号不存在或不属于当前用户，无法删除" })),
        )
            .into_response();
    };

    // Persist before the auxiliary cleanup. If the write fails, roll the
    // account back into memory so the 500 the client sees matches reality —
    // otherwise the account stays deleted in this process but resurrects on
    // restart.
    if let Err(e) = persist_all_accounts(&state).await {
        state.accounts.write().await.push(removed_account);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response();
    }

    state.rate_limits.write().await.remove(&id);
    crate::capacity::forget_account(&state, &id).await;
    // Drop any sticky bindings pointing at the deleted account so future
    // sessions don't resolve to a ghost id.
    state
        .ws_session_bindings
        .write()
        .await
        .retain(|_, b| b.account_id != id);
    state
        .prompt_cache_bindings
        .write()
        .await
        .retain(|_, b| b.account_id != id);

    (StatusCode::OK, Json(json!({ "deleted": true, "id": id }))).into_response()
}


pub(crate) async fn toggle_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ToggleShareRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(u) => u,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response(),
    };

    let mut new_val: Option<(bool, Option<f64>, Option<u64>)> = None;
    {
        let mut accounts = state.accounts.write().await;
        for a in accounts.iter_mut() {
            if a.id == id && a.owner_user_id == user_id {
                // share_enabled omitted + a limit present means "just set the
                // cap" — don't flip the share switch as a side effect.
                let limit_only = body.share_limit_percent.is_some()
                    || body.clear_share_limit
                    || body.daily_token_limit.is_some()
                    || body.clear_daily_token_limit;
                let desired = body
                    .share_enabled
                    .unwrap_or(if limit_only { a.share_enabled } else { !a.share_enabled });
                a.share_enabled = desired;
                if body.clear_share_limit {
                    a.share_limit_percent = None;
                } else if body.share_limit_percent.is_some() {
                    a.share_limit_percent = normalize_share_limit(body.share_limit_percent);
                }
                if body.clear_daily_token_limit {
                    a.daily_token_limit = None;
                } else if body.daily_token_limit.is_some() {
                    a.daily_token_limit = body.daily_token_limit.filter(|v| *v > 0);
                }
                new_val = Some((desired, a.share_limit_percent, a.daily_token_limit));
                break;
            }
        }
    }

    let Some((share_enabled, share_limit_percent, daily_token_limit)) = new_val else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "账号不存在或不属于当前用户" })),
        )
            .into_response();
    };

    if let Err(e) = persist_all_accounts(&state).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "id": id,
            "share_enabled": share_enabled,
            "share_limit_percent": share_limit_percent,
            "daily_token_limit": daily_token_limit,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Request/response DTOs for the account-management endpoints.
// ---------------------------------------------------------------------------

/// Shared import-request shape for the Codex and Claude connect endpoints
/// (label + share policy + either an inline auth JSON or a local source path).
#[derive(Debug, Deserialize)]
pub(crate) struct ConnectImportRequest {
    pub(crate) account_label: String,
    pub(crate) share_enabled: bool,
    #[serde(default)]
    pub(crate) share_limit_percent: Option<f64>,
    pub(crate) daily_token_limit: Option<u64>,
    pub(crate) auth_json: Option<String>,
    pub(crate) source_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConnectCursorRequest {
    #[serde(default)]
    pub(crate) account_label: String,
    #[serde(default)]
    pub(crate) share_enabled: bool,
    #[serde(default)]
    pub(crate) share_limit_percent: Option<f64>,
    pub(crate) daily_token_limit: Option<u64>,
    /// The `WorkosCursorSessionToken` cookie value from a logged-in Cursor
    /// session (may be `user_xxx::<jwt>`; the gateway extracts the JWT).
    /// Optional for the `/local` flow, which reads the token from Cursor's DB.
    #[serde(default)]
    pub(crate) session_token: String,
    /// Override path to Cursor's `state.vscdb` (defaults to the per-OS location).
    pub(crate) source_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConnectAccountResponse {
    pub(crate) account_id: String,
    pub(crate) owner_user_id: String,
    pub(crate) provider: String,
    pub(crate) account_label: String,
    pub(crate) share_enabled: bool,
    pub(crate) share_limit_percent: Option<f64>,
    pub(crate) daily_token_limit: Option<u64>,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAccountsResponse {
    pub(crate) current_user_id: String,
    pub(crate) grouped_accounts: std::collections::BTreeMap<String, Vec<AccountSummary>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountSummary {
    /// Internal account id (used for delete / toggle-share endpoints).
    pub(crate) id: String,
    pub(crate) account_id: String,
    pub(crate) owner_user_id: String,
    pub(crate) account_label: String,
    pub(crate) share_enabled: bool,
    pub(crate) share_limit_percent: Option<f64>,
    pub(crate) daily_token_limit: Option<u64>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) usage: AccountUsage,
    pub(crate) rate_limit: Option<RateLimitSnapshot>,
    /// Live scheduling/health state (penalty, cooldown, dead/disabled).
    pub(crate) health: AccountHealth,
    /// Owner-vs-others billable-token split over the last 7 days.
    pub(crate) owner_usage: OwnerUsageStat,
    /// True when the owner-heavy-usage guard is currently steering other
    /// users away from this account.
    pub(crate) owner_protected: bool,
}

/// Account health/scheduling state surfaced to the dashboard. Derived from the
/// in-memory `AccountRuntime` (penalty/cooldown are runtime-only, not persisted).
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct AccountHealth {
    /// One of: "healthy" | "dead" | "disabled" | "cooldown" | "degraded".
    pub(crate) status: String,
    pub(crate) dead: bool,
    pub(crate) disabled: bool,
    pub(crate) penalty: f64,
    pub(crate) backoff_level: u32,
    /// Cooldown deadline if the account is currently rate-limited.
    pub(crate) rate_limit_until: Option<DateTime<Utc>>,
    /// OAuth token expiry, if known.
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) cyber_access: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToggleShareRequest {
    pub(crate) share_enabled: Option<bool>,
    /// New share cap in percent (0-100). Send 100 (or null with
    /// `clear_share_limit`) to lift the cap.
    #[serde(default)]
    pub(crate) share_limit_percent: Option<f64>,
    /// Explicitly remove the cap (since omitting the field means "no change").
    #[serde(default)]
    pub(crate) clear_share_limit: bool,
    /// New daily donation cap in billable tokens (non-owner traffic only).
    #[serde(default)]
    pub(crate) daily_token_limit: Option<u64>,
    /// Explicitly remove the daily cap (omitting the field means "no change").
    #[serde(default)]
    pub(crate) clear_daily_token_limit: bool,
}