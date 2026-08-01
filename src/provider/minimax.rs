//! MiniMax provider (MiniMax 海螺 / MiniMax-M series). An API-key endpoint
//! provider — no OAuth, no token refresh — reached through MiniMax's
//! **Anthropic-compatible** Messages API (`{base}/v1/messages`).
//!
//! ## Shape
//!
//! Unlike GLM / Kimi (which ride BOTH an Anthropic-compatible and an
//! OpenAI-compatible endpoint), this provider deliberately wires only the
//! Anthropic surface: it exists to serve Claude Code, and on that path the
//! payload is buffered and returned verbatim so **tool calls survive**. MiniMax's
//! OpenAI-ish endpoint (`/v1/text/chatcompletion_v2`) would only reach the shared
//! text-only adapter, which is strictly worse. **MiniMax therefore serves the
//! Claude slot exclusively** — `ChainSlot::Codex` does not list it.
//!
//! No Claude Code fingerprint is injected: MiniMax is not Anthropic, so the
//! system-block injection / tool-name obfuscation must NOT be applied.
//!
//! An "account" carries:
//!   * `base_url` — Anthropic-compatible prefix; defaults to `MINIMAX_BASE_URL`
//!     env, else `https://api.minimaxi.com/anthropic`. `/v1/messages` is appended.
//!     Override to `https://api.minimax.io/anthropic` for the international site.
//!   * `api_key` / `access_token` — the MiniMax API key. MiniMax accepts either
//!     `Authorization: Bearer` or `x-api-key`, and documents that `Authorization`
//!     wins when both are present, so we send both.
//!
//! Token counts are REAL: the Anthropic-compatible endpoint returns Anthropic-
//! shaped `usage` — see `usage::tokens::parse_usage("minimax", ...)`.
use crate::prelude::*;
use crate::util::truncate_text;

/// Built-in default upstream model, used when neither the runtime override nor
/// `MINIMAX_DEFAULT_MODEL` supplies one. Also the built-in for all three Claude
/// Code tiers — MiniMax has only one model family in its catalog, so the
/// three slots collapse to the same value unless the operator overrides them.
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_OPUS_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_SONNET_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_FABLE_MODEL: &str = "MiniMax-M3";

/// The built-in model catalog, in MiniMax's own documented casing. There is no
/// live `/models` endpoint on the Anthropic surface, so this list is static and
/// can lag behind MiniMax's actual catalog — any id also works directly via
/// `minimax/<id>` regardless of whether it appears here.
pub(crate) const BUILTIN_MODELS: &[&str] = &[
    "MiniMax-M3",
    "MiniMax-M2.7",
    "MiniMax-M2.7-highspeed",
    "MiniMax-M2.5",
    "MiniMax-M2.5-highspeed",
    "MiniMax-M2.1",
    "MiniMax-M2.1-highspeed",
    "MiniMax-M2",
];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("minimax").expect("minimax model spec")
}

/// Built-in MiniMax endpoint (mainland site). Used when neither the account nor
/// `MINIMAX_BASE_URL` supplies one, so connecting only needs an api key.
const DEFAULT_BASE: &str = "https://api.minimaxi.com/anthropic";

/// Dedicated HTTP client for MiniMax. Short connect timeout (fail fast on the
/// fallback path) and a generous total timeout (long generations).
pub(crate) fn minimax_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("MINIMAX_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building minimax http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the MiniMax upstream: the explicit
/// `minimax/<model>` form, a bare `minimax` (→ default model), or a native
/// MiniMax id (which literally starts with `MiniMax-`).
pub(crate) fn is_minimax_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "minimax" || m.starts_with("minimax/") || m.starts_with("minimax-")
}

/// Maps a gateway model name to the upstream MiniMax model id.
/// `minimax/MiniMax-M3` -> `MiniMax-M3`; a bare `minimax` -> the configured
/// default; a native `MiniMax-*` id -> itself (with the documented casing
/// restored); anything else (e.g. a `claude-*` name arriving via the Claude
/// chain) -> the configured default, since MiniMax only resolves its own ids.
pub(crate) fn minimax_canonical_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_ascii_lowercase();
    if lower == "minimax" {
        return minimax_default_model();
    }
    if lower.starts_with("minimax/") {
        let rest = m["minimax/".len()..].trim();
        if !rest.is_empty() {
            return fix_case(rest);
        }
        return minimax_default_model();
    }
    if lower.starts_with("minimax-") {
        return fix_case(m);
    }
    // Claude Code's tier rewrite — opus / sonnet (with haiku folded in) /
    // fable each map to their own slot. Both `contains("haiku")` and
    // `contains("sonnet")` must reach the sonnet slot because the haiku id
    // shapes don't contain the literal "sonnet" substring.
    if lower.contains("opus") {
        return minimax_opus_model();
    }
    if lower.contains("haiku") || lower.contains("sonnet") {
        return minimax_sonnet_model();
    }
    if lower.contains("fable") {
        return minimax_fable_model();
    }
    minimax_default_model()
}

/// Restore MiniMax's documented casing for a known id. Their ids are mixed-case
/// (`MiniMax-M3`) and the API rejects an unknown id outright, so a user (or a
/// lowercasing client) typing `minimax-m3` would otherwise get a 400. Unknown ids
/// pass through verbatim — the catalog is static and may lag behind MiniMax.
fn fix_case(id: &str) -> String {
    catalog_names()
        .into_iter()
        .find(|known| known.eq_ignore_ascii_case(id))
        .unwrap_or_else(|| id.to_string())
}

/// The configured default upstream model: runtime override, else
/// `MINIMAX_DEFAULT_MODEL`, else the built-in. Used for a bare `minimax` and
/// as the fallback for any foreign model name degraded onto this provider.
fn minimax_default_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Default)
}

/// The configured upstream for `claude-opus-*` traffic.
fn minimax_opus_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Opus)
}

/// The configured upstream for `claude-sonnet-*` AND `claude-haiku-*` traffic.
fn minimax_sonnet_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Sonnet)
}

/// The configured upstream for `claude-fable-*` traffic.
fn minimax_fable_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Fable)
}

/// The Anthropic-compatible base prefix for a MiniMax account: its stored
/// `base_url`, else the `MINIMAX_BASE_URL` env, else the built-in endpoint.
/// Trailing slash trimmed.
pub(crate) fn minimax_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("MINIMAX_BASE_URL")
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

/// Send an Anthropic-shaped payload to MiniMax's `/v1/messages` and return the
/// upstream response for the caller to buffer.
///
/// The payload is forwarded as-is except for `model`: MiniMax resolves ids
/// against its own catalog, so a foreign name (typically `claude-*`, since this
/// provider exists as a Claude fallback) is rewritten first.
pub(crate) async fn send_minimax_anthropic(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = minimax_base(account);
    if base.is_empty() {
        return Err("minimax account has no Anthropic-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("minimax account has empty api key".to_string());
    }

    let mut body = payload.clone();
    let requested = body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let upstream_model = minimax_canonical_model(&requested);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(upstream_model));
    }

    let url = format!("{}/v1/messages", base);
    minimax_http_client()
        .post(&url)
        // MiniMax accepts either header and documents that Authorization wins
        // when both are sent, so sending both is safe and covers either mode.
        .bearer_auth(api_key)
        .header("x-api-key", api_key)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("Accept", "text/event-stream, application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("failed to call minimax anthropic upstream ({}): {}", url, e))
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// The upstream ids the catalog is built from: runtime override, else
/// `MINIMAX_MODELS`, else the built-in list.
fn catalog_names() -> Vec<String> {
    spec().catalog()
}

/// The gateway-facing model catalog: a bare `minimax` default entry first, then
/// each id as `minimax/<id>` (the prefix is stripped before the upstream call).
///
/// Static by design — MiniMax's Anthropic-compatible surface exposes no
/// `/models` endpoint, so there is no live list to prefer.
pub(crate) fn minimax_model_catalog() -> Vec<ModelInfo> {
    let mut out = vec![ModelInfo {
        slug: "minimax".to_string(),
        display_name: "minimax (default)".to_string(),
    }];
    for id in catalog_names() {
        let id = id.trim().to_string();
        if !id.is_empty() {
            out.push(ModelInfo { slug: format!("minimax/{}", id), display_name: id });
        }
    }
    out
}

/// Probe reachability of a MiniMax account at connect time by issuing a minimal
/// `/v1/messages` call. There is no free listing endpoint on the Anthropic
/// surface, so this spends one token — the same tradeoff as the GLM / Kimi probes.
pub(crate) async fn probe_minimax(account: &UpstreamAccount) -> Result<(), String> {
    let base = minimax_base(account);
    if account.bearer().is_empty() {
        return Err("MiniMax api key 不能为空".to_string());
    }
    if base.is_empty() {
        return Err("MiniMax 缺少 Anthropic 兼容 base_url".to_string());
    }
    let url = format!("{}/v1/messages", base);
    let resp = minimax_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header("x-api-key", account.bearer())
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": minimax_canonical_model("minimax"),
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 MiniMax ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "MiniMax 鉴权失败 ({}): {}",
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
        assert!(is_minimax_model("minimax"));
        assert!(is_minimax_model("MiniMax-M3"));
        assert!(is_minimax_model("minimax/MiniMax-M2.7"));
        assert!(is_minimax_model("minimax-m3"));
        assert!(!is_minimax_model("claude-sonnet-4-5"));
        assert!(!is_minimax_model("deepseek-v4-pro"));
        assert!(!is_minimax_model("kimi-k2.5"));
        assert!(!is_minimax_model("gpt-5"));
    }

    #[test]
    fn canonicalization_strips_prefix_fixes_case_and_defaults_foreign_names() {
        std::env::remove_var("MINIMAX_DEFAULT_MODEL");
        std::env::remove_var("MINIMAX_MODELS");
        assert_eq!(minimax_canonical_model("minimax/MiniMax-M2.7"), "MiniMax-M2.7");
        assert_eq!(minimax_canonical_model("MiniMax-M3"), "MiniMax-M3");
        assert_eq!(minimax_canonical_model("minimax"), BUILTIN_DEFAULT_MODEL);
        // A lowercasing client must still hit a real id, not a 400.
        assert_eq!(minimax_canonical_model("minimax-m3"), "MiniMax-M3");
        assert_eq!(minimax_canonical_model("minimax/minimax-m2.5-highspeed"), "MiniMax-M2.5-highspeed");
        // An id MiniMax added after this catalog was written passes through.
        assert_eq!(minimax_canonical_model("minimax/MiniMax-M9"), "MiniMax-M9");
        // A Claude name degraded onto this provider must become a real MiniMax id.
        assert_eq!(minimax_canonical_model("claude-sonnet-4-5"), BUILTIN_DEFAULT_MODEL);
        assert_eq!(minimax_canonical_model(""), BUILTIN_DEFAULT_MODEL);
    }

    #[test]
    fn catalog_has_default_first() {
        std::env::remove_var("MINIMAX_MODELS");
        let cat = minimax_model_catalog();
        assert_eq!(cat[0].slug, "minimax");
        assert!(cat.iter().any(|m| m.slug == "minimax/MiniMax-M3"));
    }

    #[test]
    fn base_defaults_and_normalizes() {
        std::env::remove_var("MINIMAX_BASE_URL");
        let mut acc = UpstreamAccount {
            id: "m1".into(),
            owner_user_id: "alice".into(),
            provider: "minimax".into(),
            account_label: "mm".into(),
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
        assert_eq!(minimax_base(&acc), DEFAULT_BASE);
        // An explicit base wins, and the trailing slash is normalized away so
        // `format!("{}/v1/messages", base)` never produces a double slash.
        acc.base_url = "https://api.minimax.io/anthropic/".into();
        assert_eq!(minimax_base(&acc), "https://api.minimax.io/anthropic");
    }
}
