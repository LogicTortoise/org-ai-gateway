use crate::prelude::*;

pub(crate) const GATEWAY_PROVIDER_KEY: &str = "org-ai-gateway";
/// Where the local client should send `responses`/`models` calls.
pub(crate) const GATEWAY_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// Merge the gateway model-provider into an existing `config.toml`, preserving
/// every other key (projects, plugins, model, marketplaces, ...).
///
/// We deliberately only touch `model_provider` + `[model_providers.<key>]`. We
/// do NOT set `chatgpt_base_url`, so all other ChatGPT backend-api calls (usage,
/// account, token refresh) keep hitting the real server and the client stays
/// healthy. Crucially we never write `auth.json`, so the client keeps its real
/// identity and Codex's account-mismatch guard never fires.
pub(crate) fn merge_gateway_into_config(existing: &str) -> Result<String, String> {
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
    provider["base_url"] = value(GATEWAY_BASE_URL);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    providers[GATEWAY_PROVIDER_KEY] = Item::Table(provider);

    Ok(doc.to_string())
}


/// `gateway_token` is the credential the local client should present to the
/// gateway — whatever bearer the caller authenticated with (the `user:<id>`
/// form).
pub(crate) fn merge_gateway_into_claude_settings(existing: &str, gateway_token: &str) -> Result<String, String> {
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
        Value::String("http://127.0.0.1:8080".to_string()),
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

