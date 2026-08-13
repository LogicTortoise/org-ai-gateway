//! DeepSeek provider. An API-key endpoint provider — no OAuth, no token refresh.
//!
//! ## Shape — BOTH client protocols
//!
//! This provider wires two endpoints and serves two client protocols:
//!
//!   1. **Claude-format traffic** (`/v1/messages`): proxied near-natively to
//!      DeepSeek's Anthropic-compatible endpoint
//!      (`{base_url_anthropic}/v1/messages`). Request and response are already
//!      Anthropic-shaped, so the gateway buffers and returns them verbatim —
//!      **tool calls survive**. No Claude Code fingerprint is injected (DeepSeek
//!      is not Anthropic; their endpoint ignores `anthropic-version` anyway).
//!
//!   2. **Codex / OpenAI-format traffic** (`/v1/responses`): proxied as a
//!      **transparent pipe** to DeepSeek's native Responses API at
//!      `{base_url_openai}/v1/responses`. DeepSeek officially supports
//!      `wire_api = "responses"` from Codex CLI
//!      ([`api-docs.deepseek.com/.../quick_start/agent_integrations/codex`](https://api-docs.deepseek.com/zh-cn/quick_start/agent_integrations/codex));
//!      the payload matches DeepSeek's wire shape exactly and needs no
//!      translation. `stream: true` / `stream: false` are both forwarded as
//!      upstream `stream: true` (forced by `ensure_codex_payload_defaults`),
//!      with the gateway buffering SSE into a Responses JSON object for
//!      non-streaming clients (parity with the MiniMax passthrough).
//!
//! ### Model id handling
//!
//! The Claude-format (Anthropic) surface keeps its tier rewrite
//! (opus → pro, sonnet/haiku → pro, fable → flash, default → pro) —
//! Anthropic-facing ids are what that surface documents. The Codex-format
//! (Responses) surface receives whatever the client sent: `deepseek-chat` or
//! `deepseek-reasoner` land on DeepSeek's Responses catalog as-is; a foreign
//! name from the Claude chain (e.g. `claude-sonnet-4-5`) is rewritten to the
//! configured default slot, which is what the operator has set as the
//! "Codex-default" id (the operator picks the reasoner flavor by setting
//! `DEEPSEEK_DEFAULT_MODEL=deepseek-reasoner`).
//!
//! An "account" carries:
//!   * `base_url` — OpenAI-compatible prefix; defaults to `DEEPSEEK_BASE_URL`
//!     env, else `https://api.deepseek.com`. `/v1/responses` is appended.
//!     **base URL must be just the host** — not include `/v1` — or the gateway
//!     will form `/v1/v1/responses` and 404.
//!   * `base_url_alt` — Anthropic-compatible prefix; defaults to
//!     `DEEPSEEK_ANTHROPIC_BASE_URL` env, else `https://api.deepseek.com/anthropic`.
//!     `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the DeepSeek API key. `x-api-key` is the
//!     documented Anthropic header; `Authorization: Bearer` is what their
//!     Claude Code recipe (`ANTHROPIC_AUTH_TOKEN`) produces. The Anthropic
//!     path sends both; the Responses path sends only the bearer form.
//!
//! Token counts are REAL on both paths (both surfaces return usage objects)
//! — see `usage::tokens::parse_usage("deepseek", ...)`.
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
/// On the Responses surface this is also the fallback for any foreign name
/// from the Claude chain (e.g. `claude-sonnet-4-5`) — operators who want the
/// reasoner flavor set `DEEPSEEK_DEFAULT_MODEL=deepseek-reasoner`.
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "deepseek-chat";

/// The built-in model catalog. Static by design: DeepSeek's `GET /models` lists
/// the ids of their *OpenAI* surface (`deepseek-chat`, `deepseek-reasoner`),
/// which are not the ids this Anthropic surface documents — pulling it live
/// would offer models that then get silently remapped. Any id still works
/// directly via `deepseek/<id>`.
pub(crate) const BUILTIN_MODELS: &[&str] = &[
    "deepseek-chat",
    "deepseek-reasoner",
];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("deepseek").expect("deepseek model spec")
}

/// Built-in DeepSeek endpoints. Used when neither the account nor the env
/// overrides supply a base URL, so connecting only needs an api key.
const BUILTIN_OPENAI_BASE: &str = "https://api.deepseek.com";
const BUILTIN_ANTHROPIC_BASE: &str = "https://api.deepseek.com/anthropic";

/// Path appended to the OpenAI-compatible base. DeepSeek's Codex integration
/// is the native Responses API; the gateway is a transparent pipe here.
const DEEPSEEK_RESPONSES_PATH: &str = "/v1/responses";

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
/// `DEEPSEEK_DEFAULT_MODEL`, else the built-in chat tier.
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

/// The OpenAI-compatible base prefix for a DeepSeek account: its stored
/// `base_url`, else the `DEEPSEEK_BASE_URL` env, else the built-in OpenAI
/// endpoint (just the host — `/v1/responses` is appended at the call site).
/// Trailing slash trimmed.
pub(crate) fn deepseek_openai_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("DEEPSEEK_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| BUILTIN_OPENAI_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

/// The Anthropic-compatible base prefix for a DeepSeek account: its stored
/// `base_url_alt`, else the `DEEPSEEK_ANTHROPIC_BASE_URL` env, else the built-in
/// Anthropic endpoint. Trailing slash trimmed.
pub(crate) fn deepseek_anthropic_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url_alt.trim().is_empty() {
        account.base_url_alt.trim().to_string()
    } else {
        std::env::var("DEEPSEEK_ANTHROPIC_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| BUILTIN_ANTHROPIC_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

/// Whether this account can serve OpenAI-format traffic. Always true: the
/// built-in base URL defaults are populated, so an empty base only happens when
/// the operator has explicitly cleared both account `base_url` and the env.
pub(crate) fn supports_openai(account: &UpstreamAccount) -> bool {
    !deepseek_openai_base(account).is_empty()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upstream call (Codex slot) — transparent pipe
// ---------------------------------------------------------------------------
//
// The Codex slot sends OpenAI Responses payloads (top-level `input` array of
// typed blocks, `instructions`, `tools`). DeepSeek's OpenAI surface is a
// standard Responses API at `/v1/responses`, so we forward the payload
// verbatim. The upstream is always called with `stream: true`
// (`ensure_codex_payload_defaults`); the gateway then either pipes the bytes
// back (streaming client) or buffers + aggregates the SSE into a Responses
// JSON object (non-streaming client).
//
// Tool calls survive because the Responses wire shape carries `apply_patch`
// / `codex_app` / `image_gen` natively — no Chat-Completions rewrap, no
// `type:"custom"` / `type:"namespace"` translator.

/// Streaming caller for DeepSeek's `/v1/responses`. The upstream is **always**
/// called with `stream: true` — `ensure_codex_payload_defaults` forces it on
/// every Codex payload before dispatch, so even non-streaming clients must
/// consume an SSE response. The gateway then either pipes the bytes through
/// (streaming client) or buffers the whole stream and aggregates it back into
/// a Responses JSON object via `sse::aggregate_codex_sse_to_response_json`
/// (non-streaming client).
pub(crate) async fn send_deepseek_responses_streaming(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = deepseek_openai_base(account);
    if base.is_empty() {
        return Err("deepseek account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("deepseek account has empty api key".to_string());
    }

    let url = format!("{}{}", base, DEEPSEEK_RESPONSES_PATH);
    let mut body = payload.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
        obj.insert("stream".to_string(), Value::Bool(true));
    }

    deepseek_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("Accept", "text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("deepseek streaming request failed ({}): {}", url, e))
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, used for Claude-format traffic)
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
    let base = deepseek_anthropic_base(account);
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

/// Probe reachability of a DeepSeek account at connect time. Dual-path:
/// prefers the OpenAI-compatible surface (the new default for Codex slot) and
/// falls back to the Anthropic-compatible surface — so an operator who hasn't
/// migrated yet (Anthropic URL still in `base_url` or `DEEPSEEK_BASE_URL`) still
/// gets a successful probe against the Anthropic path. A 401/403 on either
/// path is fatal (the key is wrong); other non-success codes (model/quota
/// complaint, 429) still count as "endpoint reachable + key accepted".
pub(crate) async fn probe_deepseek(account: &UpstreamAccount) -> Result<(), String> {
    if account.bearer().is_empty() {
        return Err("DeepSeek api key 不能为空".to_string());
    }
    let openai_base = deepseek_openai_base(account);
    let anthropic_base = deepseek_anthropic_base(account);

    // Migration safety: an explicit OpenAI base pointing at the Anthropic
    // surface (e.g. an operator who hasn't migrated their `base_url` /
    // `DEEPSEEK_BASE_URL` yet) would 404 if probed as OpenAI. Skip straight to
    // Anthropic in that case.
    let openai_usable = !openai_base.is_empty() && !openai_base.contains("/anthropic");

    if openai_usable {
        match probe_deepseek_openai(account, &openai_base).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if anthropic_base.is_empty() {
                    return Err(e);
                }
                tracing::warn!(error = %e, "deepseek openai probe failed, trying anthropic");
                return probe_deepseek_anthropic(account, &anthropic_base).await;
            }
        }
    }
    if !anthropic_base.is_empty() {
        return probe_deepseek_anthropic(account, &anthropic_base).await;
    }
    Err("DeepSeek 缺少 base_url".to_string())
}

async fn probe_deepseek_openai(account: &UpstreamAccount, base: &str) -> Result<(), String> {
    let url = format!("{}{}", base, DEEPSEEK_RESPONSES_PATH);
    let resp = deepseek_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": deepseek_default_model(),
            "input": [{ "role": "user", "content": [
                { "type": "input_text", "text": "ping" }
            ]}],
            "max_output_tokens": 1,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 DeepSeek Responses ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "DeepSeek 鉴权失败 ({}): {} — 请确认 API Key 正确且账户余额充足",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    // Both Responses shape `{"error":{"message":"..."}}` and bare `{"error":"..."}`
    // carry auth/quota info — surface them so a wrong key is diagnosed early.
    if let Ok(v) = serde_json::from_str::<Value>(&body) {
        if let Some(err) = v.get("error") {
            let msg = if let Some(s) = err.as_str() {
                Some(s.to_string())
            } else {
                err.get("message").and_then(|m| m.as_str()).map(|s| s.to_string())
            };
            if let Some(m) = msg {
                let lower = m.to_ascii_lowercase();
                if lower.contains("auth") || lower.contains("api key") || lower.contains("apikey") {
                    return Err(format!("DeepSeek 鉴权失败: {}", m));
                }
            }
        }
    }
    Ok(())
}

async fn probe_deepseek_anthropic(account: &UpstreamAccount, base: &str) -> Result<(), String> {
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
        .map_err(|e| format!("无法连接 DeepSeek Anthropic ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "DeepSeek 鉴权失败 ({}): {} — 请确认 API Key 正确且账户余额充足",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
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
        // Native Anthropic-surface ids pass through.
        assert_eq!(deepseek_canonical_model("deepseek/deepseek-v4-pro[1m]"), "deepseek-v4-pro[1m]");
        assert_eq!(deepseek_canonical_model("deepseek-v4-flash"), "deepseek-v4-flash");
        // Bare `deepseek` lands on the configured default — for the Responses
        // surface that's the reasoner flavor once the operator sets
        // `DEEPSEEK_DEFAULT_MODEL=deepseek-reasoner`.
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
        assert!(cat.iter().any(|m| m.slug == "deepseek/deepseek-chat"));
        assert!(cat.iter().any(|m| m.slug == "deepseek/deepseek-reasoner"));
    }

    #[test]
    fn openai_base_defaults_and_normalizes() {
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
        // Built-in default is the bare host — `/v1/responses` is appended at
        // the call site, never embedded in the base URL.
        assert_eq!(deepseek_openai_base(&acc), "https://api.deepseek.com");
        // Explicit base wins, trailing slash stripped. Stripping `/v1` is the
        // operator's job — the gateway never re-strips it automatically.
        acc.base_url = "https://api.deepseek.com/".into();
        assert_eq!(deepseek_openai_base(&acc), "https://api.deepseek.com");
        // Env override (no account base) wins over the built-in default.
        std::env::set_var("DEEPSEEK_BASE_URL", "https://env.example/v1");
        acc.base_url.clear();
        assert_eq!(deepseek_openai_base(&acc), "https://env.example/v1");
        std::env::remove_var("DEEPSEEK_BASE_URL");
        assert!(supports_openai(&acc));
    }

    #[test]
    fn anthropic_base_defaults_and_normalizes() {
        std::env::remove_var("DEEPSEEK_ANTHROPIC_BASE_URL");
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
        assert_eq!(deepseek_anthropic_base(&acc), BUILTIN_ANTHROPIC_BASE);
        // Explicit alt wins, trailing slash stripped.
        acc.base_url_alt = "https://api.deepseek.com/anthropic/".into();
        assert_eq!(deepseek_anthropic_base(&acc), "https://api.deepseek.com/anthropic");
    }
}