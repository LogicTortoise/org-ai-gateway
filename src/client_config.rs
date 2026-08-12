use crate::prelude::*;

pub(crate) const GATEWAY_PROVIDER_KEY: &str = "org-ai-gateway";

/// Merge the gateway model-provider into an existing `config.toml`, preserving
/// every other key (projects, plugins, model, marketplaces, ...).
///
/// We deliberately only touch `model_provider` + `[model_providers.<key>]`. We
/// do NOT set `chatgpt_base_url`, so all other ChatGPT backend-api calls (usage,
/// account, token refresh) keep hitting the real server and the client stays
/// healthy. We never write `auth.json`, so the client keeps its real identity
/// and Codex's account-mismatch guard never fires.
///
/// `base_url` is where the local client should send `responses`/`models`
/// calls — the gateway's own `/v1`, derived from the request that asked for
/// this config rather than hardcoded, since the bind address/port varies by
/// deployment.
///
/// `experimental_bearer_token` is the only auth we set in TOML — we
/// deliberately do NOT set `env_key`. Codex 0.147+'s `ModelProviderInfo::api_key`
/// treats `Some(env_key)` as a hard requirement: if the named env var is unset,
/// it returns `Err(EnvVar)` and aborts the turn before reaching the
/// `experimental_bearer_token` fallback. So writing `env_key = "OAG_BEARER"`
/// would mean "user MUST `export OAG_BEARER=…` before Codex starts" — i.e.
/// the button would be effectively a no-op without a manual export. The
/// embedded `experimental_bearer_token` is enough by itself: Codex's
/// `bearer_auth_for_provider` reads `api_key()` (returns `None` when env_key
/// is unset), then falls through to `experimental_bearer_token`, and uses it
/// as the bearer for every request.
///
/// Trade-off: rotating the bearer requires either editing this TOML directly
/// or re-clicking the button (the apply handler regenerates the embedded
/// token). There is no env-var override path. That's acceptable because the
/// gateway issues per-user tokens at click time and the UI's "恢复" button is
/// the proper revert path. If a future Codex version restores the 0.144.x
/// fallback chain (`api_key() → experimental_bearer_token → …`), we can
/// re-add `env_key` alongside for env-var overrides.
pub(crate) fn merge_gateway_into_config(
    existing: &str,
    base_url: &str,
    bearer_token: &str,
) -> Result<String, String> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| format!("无法解析现有 config.toml: {}", e))?;

    doc["model_provider"] = value(GATEWAY_PROVIDER_KEY);

    if doc.get("model_providers").and_then(Item::as_table).is_none() {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let providers = doc["model_providers"]
        .as_table_mut()
        .ok_or_else(|| "config.toml 中的 model_providers 不是表".to_string())?;
    providers.set_implicit(true);

    let mut provider = Table::new();
    provider["name"] = value("Codex via org-ai-gateway");
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider["experimental_bearer_token"] = value(bearer_token);
    providers[GATEWAY_PROVIDER_KEY] = Item::Table(provider);

    Ok(doc.to_string())
}


/// `gateway_token` is the credential the local client should present to the
/// gateway — whatever bearer the caller authenticated with (the `user:<id>`
/// form). `base_url` is the gateway's own root (no `/v1` suffix — that's
/// appended by the Claude client itself), derived from the request rather
/// than hardcoded.
pub(crate) fn merge_gateway_into_claude_settings(
    existing: &str,
    gateway_token: &str,
    base_url: &str,
) -> Result<String, String> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing)
            .map_err(|e| format!("无法解析现有 Claude settings.json: {}", e))?
    };

    if !root.is_object() {
        return Err("Claude settings.json 顶层必须是对象".to_string());
    }

    let token = gateway_token.to_string();
    let obj = root
        .as_object_mut()
        .ok_or_else(|| "Claude settings.json 顶层必须是对象".to_string())?;
    let env = obj.entry("env".to_string()).or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env_obj = env
        .as_object_mut()
        .ok_or_else(|| "Claude settings.json 的 env 必须是对象".to_string())?;

    env_obj.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        Value::String(base_url.to_string()),
    );
    env_obj.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        Value::String(token),
    );
    // Remove conflicting auth vars to avoid provider mode confusion.
    env_obj.remove("ANTHROPIC_AUTH_TOKEN");
    env_obj.remove("ANTHROPIC_API_KEY");

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

