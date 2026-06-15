use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::auth::raw_bearer;
use crate::client_config::merge_gateway_into_claude_settings;
use crate::client_config::merge_gateway_into_config;
use crate::provider::codex::codex_bootstrap_payload;
use crate::util::expand_home;
use crate::util::path_exists;

pub(crate) async fn codex_bootstrap(headers: HeaderMap) -> impl IntoResponse {
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

    let payload = match codex_bootstrap_payload(&user_id) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    (StatusCode::OK, Json(payload)).into_response()
}


pub(crate) async fn codex_apply(headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "HOME is not set" })),
            )
                .into_response();
        }
    };
    let codex_dir = PathBuf::from(format!("{}/.codex", home));
    let config_path = codex_dir.join("config.toml");
    let backup_config_path = codex_dir.join("config.toml.gateway.bak");

    if let Err(e) = tokio::fs::create_dir_all(&codex_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed creating ~/.codex: {}", e) })),
        )
            .into_response();
    }

    // Read the existing config so we can MERGE the gateway provider in without
    // clobbering the user's own settings (projects, plugins, model, ...).
    let existing = tokio::fs::read_to_string(&config_path).await.unwrap_or_default();

    // Back the original config up exactly once (so "恢复" can undo the merge).
    let mut backup_created = false;
    if path_exists(&config_path).await && !path_exists(&backup_config_path).await {
        if let Err(e) = tokio::fs::copy(&config_path, &backup_config_path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed backing up config.toml: {}", e) })),
            )
                .into_response();
        }
        backup_created = true;
    }

    let merged = match merge_gateway_into_config(&existing) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    if let Err(e) = tokio::fs::write(&config_path, merged).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed writing config.toml: {}", e) })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(CodexApplyResponse {
            user_id,
            applied: true,
            auth_untouched: true,
            backup_created,
            config_path: config_path.display().to_string(),
            backup_config_path: backup_config_path.display().to_string(),
            note: "已把对话请求路由到网关；auth.json 未改动，本地仍是你的真实账号，客户端/终端都生效，无需退出重启。".to_string(),
        }),
    )
        .into_response()
}


pub(crate) async fn codex_restore(headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = extract_user_id(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
    }
    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "HOME is not set" })),
            )
                .into_response();
        }
    };
    let codex_dir = PathBuf::from(format!("{}/.codex", home));
    let config_path = codex_dir.join("config.toml");
    let backup_config_path = codex_dir.join("config.toml.gateway.bak");

    if !path_exists(&backup_config_path).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "backup file not found",
                "hint": "请先点“一键应用到本地 Codex”生成 config.toml 备份后再恢复"
            })),
        )
            .into_response();
    }

    if let Err(e) = tokio::fs::copy(&backup_config_path, &config_path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed restoring config.toml: {}", e) })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(CodexRestoreResponse {
            restored: true,
            config_path: config_path.display().to_string(),
            backup_config_path: backup_config_path.display().to_string(),
            note: "已恢复 config.toml".to_string(),
        }),
    )
        .into_response()
}


pub(crate) async fn claude_apply(headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
        }
    };

    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "HOME is not set" })),
            )
                .into_response();
        }
    };

    let claude_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.claude", home)));
    let settings_path = claude_dir.join("settings.json");
    let backup_settings_path = claude_dir.join("settings.json.gateway.bak");

    if let Err(e) = tokio::fs::create_dir_all(&claude_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed creating claude config dir: {}", e) })),
        )
            .into_response();
    }

    let existing = tokio::fs::read_to_string(&settings_path).await.unwrap_or_default();

    let mut backup_created = false;
    if path_exists(&settings_path).await && !path_exists(&backup_settings_path).await {
        if let Err(e) = tokio::fs::copy(&settings_path, &backup_settings_path).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed backing up settings.json: {}", e) })),
            )
                .into_response();
        }
        backup_created = true;
    }

    // Write whatever the caller authenticated with (their gateway API key) into
    // the client config, so the local Claude points at the gateway with a real
    // owner-bound credential rather than a forgeable `user:<id>`.
    let gateway_token = raw_bearer(&headers).unwrap_or_else(|| format!("user:{}", user_id));
    let merged = match merge_gateway_into_claude_settings(&existing, &gateway_token) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    if let Err(e) = tokio::fs::write(&settings_path, merged).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed writing settings.json: {}", e) })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(ClaudeApplyResponse {
            user_id,
            applied: true,
            backup_created,
            settings_path: settings_path.display().to_string(),
            backup_settings_path: backup_settings_path.display().to_string(),
            note: "已把 Claude Code 默认流量路由到网关（ANTHROPIC_BASE_URL + CLAUDE_CODE_OAUTH_TOKEN）。".to_string(),
        }),
    )
        .into_response()
}


pub(crate) async fn claude_restore(headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = extract_user_id(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
    }
    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "HOME is not set" })),
            )
                .into_response();
        }
    };

    let claude_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.claude", home)));
    let settings_path = claude_dir.join("settings.json");
    let backup_settings_path = claude_dir.join("settings.json.gateway.bak");

    if !path_exists(&backup_settings_path).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "backup file not found",
                "hint": "请先点“一键应用到本地 Claude”生成 settings.json 备份后再恢复"
            })),
        )
            .into_response();
    }

    if let Err(e) = tokio::fs::copy(&backup_settings_path, &settings_path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed restoring settings.json: {}", e) })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(ClaudeRestoreResponse {
            restored: true,
            settings_path: settings_path.display().to_string(),
            backup_settings_path: backup_settings_path.display().to_string(),
            note: "已恢复 Claude settings.json 备份。".to_string(),
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Cursor consumer-side config: point the local Cursor at the gateway's
// OpenAI-compatible endpoint so it draws from the shared pool. Unlike Codex
// (config.toml) and Claude (settings.json), Cursor stores this in its SQLite
// `state.vscdb`, so we edit it via the `sqlite3` CLI.
// ---------------------------------------------------------------------------

/// ItemTable key holding the big reactive-storage JSON blob with `openAIBaseUrl`
/// and `useOpenAIKey`.
const CURSOR_REACTIVE_KEY: &str =
    "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
/// ItemTable key holding the custom OpenAI API key.
const CURSOR_OPENAI_KEY: &str = "cursorAuth/openAIKey";
/// Base URL the gateway exposes its OpenAI-compatible endpoint under.
const GATEWAY_OPENAI_BASE: &str = "http://127.0.0.1:8080/v1";

fn cursor_db_path() -> String {
    if cfg!(target_os = "windows") {
        match std::env::var("APPDATA") {
            Ok(a) => format!("{}\\Cursor\\User\\globalStorage\\state.vscdb", a),
            Err(_) => expand_home("~/AppData/Roaming/Cursor/User/globalStorage/state.vscdb"),
        }
    } else if cfg!(target_os = "macos") {
        expand_home("~/Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else {
        expand_home("~/.config/Cursor/User/globalStorage/state.vscdb")
    }
}

fn cursor_backup_path() -> String {
    let db = cursor_db_path();
    match db.rfind('/') {
        Some(i) => format!("{}/cursor.gateway.bak.json", &db[..i]),
        None => "cursor.gateway.bak.json".to_string(),
    }
}

async fn sqlite_query(db: &str, sql: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("sqlite3")
        .arg("-cmd")
        .arg(".timeout 4000")
        .arg(db)
        .arg(sql)
        .output()
        .await
        .map_err(|e| format!("无法调用 sqlite3: {} (请确认已安装 sqlite3)", e))?;
    if !out.status.success() {
        return Err(format!("sqlite3 失败: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Write `value` into ItemTable[`key`] via a temp file + `readfile()`, which
/// sidesteps SQL escaping for large/JSON values.
async fn sqlite_set_item(db: &str, key: &str, value: &str) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("oag-cursor-{}.tmp", Uuid::new_v4()));
    tokio::fs::write(&tmp, value.as_bytes())
        .await
        .map_err(|e| format!("写临时文件失败: {}", e))?;
    let sql = format!(
        "INSERT OR REPLACE INTO ItemTable(key,value) VALUES('{}', readfile('{}'));",
        key.replace('\'', "''"),
        tmp.display().to_string().replace('\'', "''")
    );
    let res = sqlite_query(db, &sql).await.map(|_| ());
    let _ = tokio::fs::remove_file(&tmp).await;
    res
}

pub(crate) async fn cursor_apply(headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response(),
    };

    let db = cursor_db_path();
    if !path_exists(&PathBuf::from(&db)).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未找到 Cursor 数据库: {}", db), "hint": "请先安装并登录 Cursor" })),
        )
            .into_response();
    }

    // Read + parse the reactive-storage blob.
    let blob_raw = match sqlite_query(&db, &format!(
        "SELECT value FROM ItemTable WHERE key='{}';", CURSOR_REACTIVE_KEY
    )).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };
    if blob_raw.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Cursor 配置尚未初始化，请先正常打开一次 Cursor" }))).into_response();
    }
    let mut blob: Value = match serde_json::from_str(&blob_raw) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("解析 Cursor 配置失败: {}", e) }))).into_response(),
    };

    let old_base = blob.get("openAIBaseUrl").cloned().unwrap_or(Value::Null);
    let old_use = blob.get("useOpenAIKey").cloned().unwrap_or(Value::Bool(false));
    let old_key = sqlite_query(&db, &format!(
        "SELECT value FROM ItemTable WHERE key='{}';", CURSOR_OPENAI_KEY
    )).await.unwrap_or_default();

    // Back up the original values exactly once.
    let backup_path = cursor_backup_path();
    let mut backup_created = false;
    if !path_exists(&PathBuf::from(&backup_path)).await {
        let backup = json!({
            "openAIBaseUrl": old_base,
            "useOpenAIKey": old_use,
            "cursorAuthOpenAIKey": old_key,
        });
        // A failed serialization must not produce an empty backup file with
        // `backup_created=true` — a later restore would then wipe the config.
        let backup_bytes = match serde_json::to_vec_pretty(&backup) {
            Ok(v) => v,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("序列化备份失败: {}", e) }))).into_response(),
        };
        if let Err(e) = tokio::fs::write(&backup_path, backup_bytes).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("写备份失败: {}", e) }))).into_response();
        }
        backup_created = true;
    }

    // Inject gateway routing.
    if let Value::Object(map) = &mut blob {
        map.insert("openAIBaseUrl".into(), Value::String(GATEWAY_OPENAI_BASE.into()));
        map.insert("useOpenAIKey".into(), Value::Bool(true));
    }
    let new_blob = match serde_json::to_string(&blob) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("序列化 Cursor 配置失败: {}", e) }))).into_response(),
    };

    if let Err(e) = sqlite_set_item(&db, CURSOR_REACTIVE_KEY, &new_blob).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response();
    }
    let gateway_token = raw_bearer(&headers).unwrap_or_else(|| format!("user:{}", user_id));
    if let Err(e) = sqlite_set_item(&db, CURSOR_OPENAI_KEY, &gateway_token).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "applied": true,
            "user_id": user_id,
            "backup_created": backup_created,
            "db_path": db,
            "backup_path": backup_path,
            "base_url": GATEWAY_OPENAI_BASE,
            "note": "已把本机 Cursor 的 OpenAI Base URL 指向网关并启用自定义 Key。请完全退出并重新打开 Cursor 后生效。",
            "caveat": "注意：Cursor 仅在 chat/plan 面板使用自定义 Base URL，主力 Agent/Tab 仍走 Cursor 自有后端（Cursor 的设计限制，非网关问题）。"
        })),
    )
        .into_response()
}

pub(crate) async fn cursor_restore(headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = extract_user_id(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response();
    }
    let db = cursor_db_path();
    let backup_path = cursor_backup_path();
    if !path_exists(&PathBuf::from(&backup_path)).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "未找到备份", "hint": "请先点“应用配置”生成备份后再恢复" })),
        )
            .into_response();
    }
    let backup_raw = match tokio::fs::read_to_string(&backup_path).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("读备份失败: {}", e) }))).into_response(),
    };
    let backup: Value = match serde_json::from_str(&backup_raw) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("备份文件损坏，已中止恢复: {}", e) }))).into_response(),
    };

    let blob_raw = match sqlite_query(&db, &format!(
        "SELECT value FROM ItemTable WHERE key='{}';", CURSOR_REACTIVE_KEY
    )).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };
    // Refuse to proceed on a missing/corrupt blob: blindly continuing would
    // serialize `Value::Null` and overwrite Cursor's config with the literal
    // string "null".
    let mut blob: Value = match serde_json::from_str::<Value>(&blob_raw) {
        Ok(v) if v.is_object() => v,
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Cursor 配置读取失败或已损坏，已中止恢复以免覆盖" }))).into_response(),
    };
    if let Value::Object(map) = &mut blob {
        map.insert("openAIBaseUrl".into(), backup.get("openAIBaseUrl").cloned().unwrap_or(Value::Null));
        map.insert("useOpenAIKey".into(), backup.get("useOpenAIKey").cloned().unwrap_or(Value::Bool(false)));
    }
    let new_blob = match serde_json::to_string(&blob) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("序列化 Cursor 配置失败: {}", e) }))).into_response(),
    };
    if let Err(e) = sqlite_set_item(&db, CURSOR_REACTIVE_KEY, &new_blob).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response();
    }

    // Restore the API key row (delete it if there was none originally). A
    // failure here must surface — claiming `restored: true` while the gateway
    // key is still installed would leave Cursor silently routed at the gateway.
    let old_key = backup.get("cursorAuthOpenAIKey").and_then(|v| v.as_str()).unwrap_or("");
    let key_result = if old_key.is_empty() {
        sqlite_query(&db, &format!("DELETE FROM ItemTable WHERE key='{}';", CURSOR_OPENAI_KEY))
            .await
            .map(|_| ())
    } else {
        sqlite_set_item(&db, CURSOR_OPENAI_KEY, old_key).await
    };
    if let Err(e) = key_result {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("恢复 OpenAI Key 失败: {}", e) }))).into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "restored": true,
            "db_path": db,
            "note": "已恢复 Cursor 原配置（Base URL / Key）。请完全退出并重新打开 Cursor 后生效。"
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Response DTOs for the client-config apply/restore endpoints.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct CodexApplyResponse {
    pub(crate) user_id: String,
    pub(crate) applied: bool,
    /// We never rewrite auth.json, so the local client keeps its real identity
    /// and Codex's account-mismatch guard never trips.
    pub(crate) auth_untouched: bool,
    pub(crate) backup_created: bool,
    pub(crate) config_path: String,
    pub(crate) backup_config_path: String,
    pub(crate) note: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodexRestoreResponse {
    pub(crate) restored: bool,
    pub(crate) config_path: String,
    pub(crate) backup_config_path: String,
    pub(crate) note: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClaudeApplyResponse {
    pub(crate) user_id: String,
    pub(crate) applied: bool,
    pub(crate) backup_created: bool,
    pub(crate) settings_path: String,
    pub(crate) backup_settings_path: String,
    pub(crate) note: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClaudeRestoreResponse {
    pub(crate) restored: bool,
    pub(crate) settings_path: String,
    pub(crate) backup_settings_path: String,
    pub(crate) note: String,
}