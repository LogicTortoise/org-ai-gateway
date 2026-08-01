//! DeepSeek provider. An API-key endpoint provider — no OAuth, no token refresh —
//! reached through DeepSeek's **Anthropic-compatible** Messages API
//! (`https://api.deepseek.com/anthropic/v1/messages`), the same surface their docs
//! point Claude Code at via `ANTHROPIC_BASE_URL`.
//!
//! ## Shape
//!
//! Unlike GLM / Kimi (which ride BOTH an Anthropic-compatible and an
//! OpenAI-compatible endpoint), this provider deliberately wires only the
//! Anthropic surface: it exists to serve Claude Code, and on that path the
//! payload is buffered and returned verbatim so **tool calls survive**. Routing
//! Codex traffic through DeepSeek's `/v1/chat/completions` would only reach the
//! shared text-only adapter, which is strictly worse. **DeepSeek therefore serves
//! the Claude slot exclusively** — `ChainSlot::Codex` does not list it.
//!
//! No Claude Code fingerprint is injected: DeepSeek is not Anthropic, so the
//! system-block injection / tool-name obfuscation must NOT be applied. (Their
//! endpoint ignores `anthropic-version` / `anthropic-beta` anyway.)
//!
//! An "account" carries:
//!   * `base_url` — Anthropic-compatible prefix; defaults to `DEEPSEEK_BASE_URL`
//!     env, else `https://api.deepseek.com/anthropic`. `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the DeepSeek API key. Their docs list
//!     `x-api-key` as fully supported and their Claude Code recipe uses
//!     `ANTHROPIC_AUTH_TOKEN` (bearer), so we send both.
//!
//! Token counts are REAL: the Anthropic-compatible endpoint returns Anthropic-
//! shaped `usage` — see `usage::tokens::parse_usage("deepseek", ...)`.
use crate::prelude::*;
use crate::util::truncate_text;

/// Built-in upstream model for `claude-opus-*` traffic. The most expensive
/// tier — DeepSeek doesn't expose a stronger model than `deepseek-v4-pro`
/// today, so this slot reuses the pro tier by default.
pub(crate) const BUILTIN_OPUS_MODEL: &str = "deepseek-v4-pro";

/// Built-in upstream model for `claude-sonnet-*` AND `claude-haiku-*` traffic
/// (the two tiers share one upstream slot — most third-party providers only
/// expose a mid-tier, not a separate Sonnet variant, and DeepSeek's docs fold
/// haiku into the same `deepseek-v4-pro` recommendation).
pub(crate) const BUILTIN_SONNET_MODEL: &str = "deepseek-v4-pro";

/// Built-in upstream model for `claude-fable-*` traffic (Claude Code's
/// cheapest tier). Maps to DeepSeek's flash tier.
pub(crate) const BUILTIN_FABLE_MODEL: &str = "deepseek-v4-flash";

/// Built-in default upstream model for the bare `deepseek` slug, used when
/// neither the runtime override nor `DEEPSEEK_DEFAULT_MODEL` supplies one.
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "deepseek-v4-pro";

/// The built-in model catalog. Static by design: DeepSeek's `GET /models` lists
/// the ids of their *OpenAI* surface (`deepseek-chat`, `deepseek-reasoner`),
/// which are not the ids this Anthropic surface documents — pulling it live
/// would offer models that then get silently remapped. Any id still works
/// directly via `deepseek/<id>`.
pub(crate) const BUILTIN_MODELS: &[&str] = &[
    "deepseek-v4-pro",
    "deepseek-v4-pro[1m]",
    "deepseek-v4-flash",
];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("deepseek").expect("deepseek model spec")
}

/// Built-in DeepSeek endpoint. Used when neither the account nor
/// `DEEPSEEK_BASE_URL` supplies one, so connecting only needs an api key.
const DEFAULT_BASE: &str = "https://api.deepseek.com/anthropic";

/// Dedicated HTTP client for DeepSeek. Short connect timeout (fail fast on the
/// fallback path) and a generous total timeout (long generations).
pub(crate) fn deepseek_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("DEEPSEEK_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building deepseek http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the DeepSeek upstream: the explicit
/// `deepseek/<model>` form, a bare `deepseek` (→ default model), or a native
/// DeepSeek id (`deepseek-v4-pro`, `deepseek-chat`, ...).
pub(crate) fn is_deepseek_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "deepseek" || m.starts_with("deepseek/") || m.starts_with("deepseek-")
}

/// Maps a gateway model name to the upstream DeepSeek model id.
/// `deepseek/deepseek-v4-pro` -> `deepseek-v4-pro`; a native `deepseek-*` id ->
/// itself; a bare `deepseek` -> the configured default. A foreign name arriving
/// via the Claude chain follows Claude Code's tier naming: opus -> opus slot,
/// sonnet / haiku -> the shared sonnet slot, fable -> fable slot.
pub(crate) fn deepseek_canonical_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_ascii_lowercase();
    if lower == "deepseek" {
        return deepseek_default_model();
    }
    if lower.starts_with("deepseek/") {
        let rest = m["deepseek/".len()..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
        return deepseek_default_model();
    }
    if lower.starts_with("deepseek-") {
        return m.to_string();
    }
    // Matched anywhere, not just as a prefix: Claude Code's haiku ids come in both
    // the `claude-haiku-4-5-*` and the older `claude-3-5-haiku-*` shapes, and
    // neither contains the literal substring "sonnet" — both must route to the
    // sonnet slot.
    if lower.contains("opus") {
        return deepseek_opus_model();
    }
    if lower.contains("haiku") || lower.contains("sonnet") {
        return deepseek_sonnet_model();
    }
    if lower.contains("fable") {
        return deepseek_fable_model();
    }
    deepseek_default_model()
}

/// The configured default upstream model: runtime override, else
/// `DEEPSEEK_DEFAULT_MODEL`, else the built-in pro tier.
fn deepseek_default_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Default)
}

/// The configured model for opus-tier traffic.
fn deepseek_opus_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Opus)
}

/// The configured model for sonnet+haiku-tier traffic (shared upstream slot).
fn deepseek_sonnet_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Sonnet)
}

/// The configured model for fable-tier traffic (Claude Code's cheapest tier).
fn deepseek_fable_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Fable)
}

/// The Anthropic-compatible base prefix for a DeepSeek account: its stored
/// `base_url`, else the `DEEPSEEK_BASE_URL` env, else the built-in endpoint.
/// Trailing slash trimmed.
pub(crate) fn deepseek_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("DEEPSEEK_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, the only serving path)
// ---------------------------------------------------------------------------

/// Send an Anthropic-shaped payload to DeepSeek's `/v1/messages` and return the
/// upstream response for the caller to buffer.
///
/// The payload is forwarded as-is except for `model`, which is resolved to a real
/// DeepSeek id first. DeepSeek's backend would remap an unknown name to the flash
/// tier on its own, but doing it here keeps the audit ledger honest about which
/// model actually ran.
pub(crate) async fn send_deepseek_anthropic(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = deepseek_base(account);
    if base.is_empty() {
        return Err("deepseek account has no Anthropic-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("deepseek account has empty api key".to_string());
    }

    let mut body = payload.clone();
    let requested = body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let upstream_model = deepseek_canonical_model(&requested);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(upstream_model));
    }

    let url = format!("{}/v1/messages", base);
    deepseek_http_client()
        .post(&url)
        // `x-api-key` is the documented header; the bearer form is what their
        // Claude Code recipe (`ANTHROPIC_AUTH_TOKEN`) produces. Send both.
        .bearer_auth(api_key)
        .header("x-api-key", api_key)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("Accept", "text/event-stream, application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("failed to call deepseek anthropic upstream ({}): {}", url, e))
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// The gateway-facing model catalog: a bare `deepseek` default entry first, then
/// each id as `deepseek/<id>` (the prefix is stripped before the upstream call).
/// The id list comes from the runtime override, else `DEEPSEEK_MODELS`, else
/// `BUILTIN_MODELS` — see there for why it is never fetched live.
pub(crate) fn deepseek_model_catalog() -> Vec<ModelInfo> {
    let names: Vec<String> = spec().catalog();
    let mut out = vec![ModelInfo {
        slug: "deepseek".to_string(),
        display_name: "deepseek (default)".to_string(),
    }];
    for id in names {
        let id = id.trim().to_string();
        if !id.is_empty() {
            out.push(ModelInfo { slug: format!("deepseek/{}", id), display_name: id });
        }
    }
    out
}

/// Probe reachability of a DeepSeek account at connect time by issuing a minimal
/// `/v1/messages` call — the exact path traffic will take, so a base URL that is
/// wrong only for the Anthropic surface is caught here. Spends one token.
pub(crate) async fn probe_deepseek(account: &UpstreamAccount) -> Result<(), String> {
    let base = deepseek_base(account);
    if account.bearer().is_empty() {
        return Err("DeepSeek api key 不能为空".to_string());
    }
    if base.is_empty() {
        return Err("DeepSeek 缺少 Anthropic 兼容 base_url".to_string());
    }
    let url = format!("{}/v1/messages", base);
    let resp = deepseek_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header("x-api-key", account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": deepseek_canonical_model("deepseek"),
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 DeepSeek ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "DeepSeek 鉴权失败 ({}): {} — 请确认 API Key 正确且账户余额充足",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    // Anything else (a model/quota complaint, a 429) still proves the endpoint and
    // key are usable, so it must not block connecting.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_detection() {
        assert!(is_deepseek_model("deepseek"));
        assert!(is_deepseek_model("deepseek-v4-pro"));
        assert!(is_deepseek_model("deepseek/deepseek-v4-flash"));
        assert!(is_deepseek_model("DeepSeek-V4-Pro"));
        assert!(!is_deepseek_model("claude-sonnet-4-5"));
        assert!(!is_deepseek_model("MiniMax-M3"));
        assert!(!is_deepseek_model("kimi-k2.5"));
        assert!(!is_deepseek_model("gpt-5"));
    }

    #[test]
    fn canonicalization_strips_prefix_and_maps_claude_tiers() {
        std::env::remove_var("DEEPSEEK_DEFAULT_MODEL");
        std::env::remove_var("DEEPSEEK_OPUS_MODEL");
        std::env::remove_var("DEEPSEEK_SONNET_MODEL");
        std::env::remove_var("DEEPSEEK_FABLE_MODEL");
        assert_eq!(deepseek_canonical_model("deepseek/deepseek-v4-pro[1m]"), "deepseek-v4-pro[1m]");
        assert_eq!(deepseek_canonical_model("deepseek-v4-flash"), "deepseek-v4-flash");
        assert_eq!(deepseek_canonical_model("deepseek"), BUILTIN_DEFAULT_MODEL);
        // Claude Code's tier names map onto the three upstream slots. haiku
        // folds into sonnet because Claude Code's haiku ids don't contain the
        // literal "sonnet" substring (e.g. `claude-haiku-4-5-*`). Both
        // `contains("haiku")` and `contains("sonnet")` must therefore land in
        // the same slot.
        assert_eq!(deepseek_canonical_model("claude-opus-4-6"), BUILTIN_OPUS_MODEL);
        assert_eq!(deepseek_canonical_model("claude-sonnet-4-5"), BUILTIN_SONNET_MODEL);
        assert_eq!(deepseek_canonical_model("claude-haiku-4-5-20251001"), BUILTIN_SONNET_MODEL);
        assert_eq!(deepseek_canonical_model("claude-3-5-haiku-20241022"), BUILTIN_SONNET_MODEL);
        assert_eq!(deepseek_canonical_model("claude-fable-4-0"), BUILTIN_FABLE_MODEL);
        assert_eq!(deepseek_canonical_model(""), BUILTIN_DEFAULT_MODEL);
    }

    #[test]
    fn catalog_has_default_first() {
        std::env::remove_var("DEEPSEEK_MODELS");
        let cat = deepseek_model_catalog();
        assert_eq!(cat[0].slug, "deepseek");
        assert!(cat.iter().any(|m| m.slug == "deepseek/deepseek-v4-pro"));
        assert!(cat.iter().any(|m| m.slug == "deepseek/deepseek-v4-flash"));
    }

    #[test]
    fn base_defaults_and_normalizes() {
        std::env::remove_var("DEEPSEEK_BASE_URL");
        let mut acc = UpstreamAccount {
            id: "d1".into(),
            owner_user_id: "alice".into(),
            provider: "deepseek".into(),
            account_label: "ds".into(),
            access_token: String::new(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            api_key: "sk-test".into(),
            base_url: String::new(),
            base_url_alt: String::new(),
            share_enabled: true,
            share_limit_percent: None,
            daily_token_limit: None,
            created_at: Utc::now(),
            runtime: AccountRuntime::default(),
        };
        assert_eq!(deepseek_base(&acc), DEFAULT_BASE);
        acc.base_url = "https://api.deepseek.com/anthropic/".into();
        assert_eq!(deepseek_base(&acc), "https://api.deepseek.com/anthropic");
    }
}
