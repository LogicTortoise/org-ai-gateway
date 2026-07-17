//! Kimi provider (Moonshot AI). An API-key endpoint provider — no OAuth, no
//! token refresh — structurally identical to GLM: it can serve BOTH client
//! protocols:
//!
//!   1. Claude-format traffic (`/v1/messages`): proxied near-natively to Kimi's
//!      Anthropic-compatible endpoint (`{base_url_alt}/v1/messages`). The request
//!      and response are already Anthropic-shaped, so the gateway buffers and
//!      returns them verbatim — tool calls survive. No Claude Code fingerprint is
//!      injected (Kimi is not Anthropic). This is the path used when Kimi serves
//!      as a fallback for Claude Code.
//!   2. Codex-format traffic (`/v1/responses`): Kimi has no Responses API, so the
//!      request is normalized through the shared format adapter
//!      (`provider::cursor::{extract_request, build_*_body}`) onto Kimi's
//!      OpenAI-compatible `{base_url}/chat/completions`, then re-rendered in the
//!      client's format. Buffered text only (no tool round-trips), same tradeoff
//!      as the ollama path.
//!
//! Unlike GLM (whose base URLs vary by tenant — bigmodel.cn vs z.ai), Kimi's
//! endpoints are well-known and fixed, so the base URLs DEFAULT to Moonshot's
//! public endpoints — connecting only requires an API key. An "account" carries:
//!   * `base_url`     — OpenAI-compatible prefix; defaults to `KIMI_BASE_URL` env,
//!                      else `https://api.moonshot.cn/v1`. `/chat/completions` is appended.
//!   * `base_url_alt` — Anthropic-compatible prefix; defaults to
//!                      `KIMI_ANTHROPIC_BASE_URL` env, else `https://api.moonshot.cn/anthropic`.
//!                      `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the Kimi API key (bearer auth).
//!
//! Token counts are REAL here (both endpoints return usage objects), so audited
//! usage is exact — see `usage::tokens::parse_usage("kimi", ...)`.
use crate::prelude::*;
use crate::provider::cursor::ExtractedRequest;
use crate::util::truncate_text;

/// Default Kimi model when a request selects the bare `kimi` slug (override with
/// `KIMI_DEFAULT_MODEL`). This is what a `claude-*` request degraded onto the
/// Kimi fallback ends up running.
const FALLBACK_DEFAULT_MODEL: &str = "kimi-k2-0711-preview";

/// STATIC FALLBACK model list, used only when the live `/models` fetch fails
/// (e.g. no Kimi account connected yet). Can lag behind Moonshot's actual
/// catalog — `fetch_kimi_models` prefers the live list, and `KIMI_MODELS`
/// (comma-separated) overrides the whole thing. Any model id also works
/// directly via `kimi/<id>` regardless of whether it appears here.
const DEFAULT_KIMI_MODELS: &[&str] = &[
    "kimi-k2-0711-preview",
    "kimi-k2-turbo-preview",
    "kimi-latest",
    "moonshot-v1-8k",
    "moonshot-v1-32k",
    "moonshot-v1-128k",
];

/// Built-in Moonshot endpoints. Used when neither the account nor the env
/// override supplies a base URL, so connecting a Kimi account only needs an api
/// key. The `.cn` host serves mainland China; override to `https://api.moonshot.ai/...`
/// via `KIMI_BASE_URL` / `KIMI_ANTHROPIC_BASE_URL` (or per-account) for global.
const DEFAULT_OPENAI_BASE: &str = "https://api.moonshot.cn/v1";
const DEFAULT_ANTHROPIC_BASE: &str = "https://api.moonshot.cn/anthropic";

/// Dedicated HTTP client for Kimi. Short connect timeout (fail fast on the
/// fallback path) and a generous total timeout (long generations).
pub(crate) fn kimi_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("KIMI_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building kimi http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the Kimi upstream. Accepts the explicit
/// `kimi/<model>` form, a bare `kimi`/`moonshot` (→ default model), or any
/// native Moonshot model id (`kimi-*`, `moonshot-*`, `kimi/...`, `moonshot/...`).
pub(crate) fn is_kimi_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "kimi"
        || m == "moonshot"
        || m.starts_with("kimi/")
        || m.starts_with("kimi-")
        || m.starts_with("moonshot/")
        || m.starts_with("moonshot-")
}

/// Maps a gateway model name to the upstream Kimi model id. `kimi/kimi-latest` ->
/// `kimi-latest`; a bare `kimi`/`moonshot` -> the configured default; a native
/// `kimi-*` / `moonshot-*` id -> unchanged.
pub(crate) fn kimi_canonical_model(model: &str) -> String {
    let m = model.trim();
    if m.eq_ignore_ascii_case("kimi") || m.eq_ignore_ascii_case("moonshot") {
        return std::env::var("KIMI_DEFAULT_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| FALLBACK_DEFAULT_MODEL.to_string());
    }
    let lower = m.to_ascii_lowercase();
    if lower.starts_with("kimi/") {
        return m["kimi/".len()..].to_string();
    }
    if lower.starts_with("moonshot/") {
        return m["moonshot/".len()..].to_string();
    }
    m.to_string()
}

/// The OpenAI-compatible base prefix for a Kimi account: its stored `base_url`,
/// else the `KIMI_BASE_URL` env, else the built-in Moonshot endpoint. Trailing
/// slash trimmed.
pub(crate) fn kimi_openai_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("KIMI_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

/// The Anthropic-compatible base prefix for a Kimi account: its stored
/// `base_url_alt`, else the `KIMI_ANTHROPIC_BASE_URL` env, else the built-in
/// Moonshot endpoint. Trailing slash trimmed.
pub(crate) fn kimi_anthropic_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url_alt.trim().is_empty() {
        account.base_url_alt.trim().to_string()
    } else {
        std::env::var("KIMI_ANTHROPIC_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

/// Whether this account can serve OpenAI-format traffic (the adapter path).
/// Always true for Kimi since the OpenAI base defaults to a built-in endpoint.
pub(crate) fn supports_openai(account: &UpstreamAccount) -> bool {
    !kimi_openai_base(account).is_empty()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upstream call (adapter path, used for Codex-format traffic)
// ---------------------------------------------------------------------------

/// Outcome of a Kimi OpenAI-compatible call.
pub(crate) struct KimiResult {
    pub(crate) text: String,
    pub(crate) status: reqwest::StatusCode,
    pub(crate) error: Option<String>,
    /// Real token usage parsed from the response (`usage.prompt_tokens` /
    /// `usage.completion_tokens`); zero when the upstream omitted them.
    pub(crate) usage: TokenUsage,
}

/// Build the `messages` array `/chat/completions` expects from the normalized
/// request: the (joined) system instruction first, then the conversation turns.
fn build_messages(req: &ExtractedRequest) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if !req.instruction.trim().is_empty() {
        out.push(json!({ "role": "system", "content": req.instruction }));
    }
    for turn in &req.turns {
        // ChatTurn.role: 1 = user, 2 = assistant (the shared extractor's encoding).
        let role = if turn.role == 2 { "assistant" } else { "user" };
        out.push(json!({ "role": role, "content": turn.content }));
    }
    out
}

/// Send one chat request to Kimi's OpenAI-compatible `/chat/completions` and
/// return the assistant text plus real token usage. Always non-streaming — the
/// gateway buffers the whole reply and re-renders it in the client's format.
pub(crate) async fn send_kimi_openai(
    client: &reqwest::Client,
    account: &UpstreamAccount,
    model: &str,
    req: &ExtractedRequest,
) -> Result<KimiResult, String> {
    let base = kimi_openai_base(account);
    if base.is_empty() {
        return Err("kimi account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("kimi account has empty api key".to_string());
    }
    let url = format!("{}/chat/completions", base);
    let body = json!({
        "model": model,
        "messages": build_messages(req),
        "stream": false,
    });

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("kimi upstream request failed ({}): {}", url, e))?;
    let status = resp.status();
    let text_body = resp
        .text()
        .await
        .map_err(|e| format!("reading kimi upstream body failed: {}", e))?;

    if !status.is_success() {
        let detail = parse_error_message(&text_body)
            .unwrap_or_else(|| format!("kimi upstream returned {}", status));
        return Ok(KimiResult { text: String::new(), status, error: Some(detail), usage: TokenUsage::default() });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid kimi response JSON: {}", e))?;
    if let Some(err) = parse_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(KimiResult { text: String::new(), status, error: Some(err), usage: TokenUsage::default() });
        }
    }

    let content = value
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(KimiResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("kimi", &text_body),
    })
}

/// Pull a human-readable error out of a Kimi error body. Moonshot follows the
/// OpenAI shape (`{"error":{"message":"..."}}`) but also tolerates a bare
/// `{"error":"..."}`.
fn parse_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_string());
    }
    err.get("message").and_then(|m| m.as_str()).map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, used for Claude-format traffic)
// ---------------------------------------------------------------------------

/// Send a raw Anthropic-shaped payload to Kimi's Anthropic-compatible
/// `/v1/messages` and return the upstream response for the caller to buffer.
/// No fingerprint injection: Kimi is not Anthropic, so the Claude Code system
/// blocks / tool obfuscation must NOT be applied.
pub(crate) async fn send_kimi_anthropic(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = kimi_anthropic_base(account);
    if base.is_empty() {
        return Err("kimi account has no Anthropic-compatible base_url_alt".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("kimi account has empty api key".to_string());
    }
    let url = format!("{}/v1/messages", base);
    kimi_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("Accept", "text/event-stream, application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("failed to call kimi anthropic upstream ({}): {}", url, e))
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// Build the gateway-facing model list from a set of upstream model ids: a bare
/// `kimi` default entry first, then each id as `kimi/<id>` (the prefix is
/// stripped before the upstream call).
fn models_from_ids(ids: impl IntoIterator<Item = String>) -> Vec<ModelInfo> {
    let mut out = vec![ModelInfo {
        slug: "kimi".to_string(),
        display_name: "kimi (default)".to_string(),
    }];
    for id in ids {
        let id = id.trim().to_string();
        if !id.is_empty() {
            out.push(ModelInfo { slug: format!("kimi/{}", id), display_name: id });
        }
    }
    out
}

/// The STATIC fallback model catalog: `KIMI_MODELS` override if set, else the
/// built-in `DEFAULT_KIMI_MODELS`. Used when no live list is available.
pub(crate) fn kimi_model_catalog() -> Vec<ModelInfo> {
    let names: Vec<String> = match std::env::var("KIMI_MODELS") {
        Ok(v) if !v.trim().is_empty() => {
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        }
        _ => DEFAULT_KIMI_MODELS.iter().map(|s| s.to_string()).collect(),
    };
    models_from_ids(names)
}

/// Fetch the LIVE model list from Kimi's OpenAI-compatible `GET {base}/models`.
/// Returns the upstream ids mapped to `kimi/<id>` slugs. Errors (bad key,
/// endpoint absent) bubble up so the caller can fall back to the static catalog.
/// An explicit `KIMI_MODELS` override short-circuits the network call.
pub(crate) async fn fetch_kimi_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    if std::env::var("KIMI_MODELS").map(|v| !v.trim().is_empty()).unwrap_or(false) {
        return Ok(kimi_model_catalog());
    }
    let base = kimi_openai_base(account);
    if base.is_empty() {
        return Err("kimi account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("kimi account has empty api key".to_string());
    }
    let url = format!("{}/models", base);
    let resp = kimi_http_client()
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("failed to reach kimi models api ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("reading kimi models body failed: {}", e))?;
    if !status.is_success() {
        return Err(format!("kimi models api error {}: {}", status.as_u16(), truncate_text(&body, 200)));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid kimi models response: {}", e))?;
    // OpenAI shape: {"object":"list","data":[{"id":"kimi-k2-0711-preview",...}, ...]}.
    let arr = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "kimi models response missing `data` array".to_string())?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    if ids.is_empty() {
        return Err("kimi models response had no model ids".to_string());
    }
    Ok(models_from_ids(ids))
}

/// Probe reachability of a Kimi account by issuing a tiny OpenAI-compatible
/// request. Used at connect time to validate the api key before storing.
pub(crate) async fn probe_kimi(account: &UpstreamAccount) -> Result<(), String> {
    let base = kimi_openai_base(account);
    if account.bearer().is_empty() {
        return Err("Kimi api key 不能为空".to_string());
    }
    if base.is_empty() {
        return Err("Kimi 缺少 OpenAI 兼容 base_url".to_string());
    }
    let model = kimi_canonical_model("kimi");
    let url = format!("{}/chat/completions", base);
    let resp = kimi_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": model,
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 Kimi ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // 401/403 => bad key.
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!("Kimi 鉴权失败 ({}): {}", status.as_u16(), truncate_text(&body, 200)));
    }
    if !status.is_success() {
        if let Some(msg) = parse_error_message(&body) {
            // A model/quota error still proves the endpoint+key are valid.
            let lower = msg.to_ascii_lowercase();
            if lower.contains("auth") || lower.contains("api key") || lower.contains("apikey") {
                return Err(format!("Kimi 鉴权失败: {}", msg));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_detection_and_canonicalization() {
        assert!(is_kimi_model("kimi"));
        assert!(is_kimi_model("moonshot"));
        assert!(is_kimi_model("kimi/kimi-latest"));
        assert!(is_kimi_model("kimi-k2-0711-preview"));
        assert!(is_kimi_model("moonshot-v1-32k"));
        assert!(is_kimi_model("KIMI-K2-0711-PREVIEW"));
        assert!(!is_kimi_model("gpt-5"));
        assert!(!is_kimi_model("claude-sonnet-4"));
        assert!(!is_kimi_model("glm-4.6"));
        assert_eq!(kimi_canonical_model("kimi/kimi-latest"), "kimi-latest");
        assert_eq!(kimi_canonical_model("moonshot/moonshot-v1-8k"), "moonshot-v1-8k");
        assert_eq!(kimi_canonical_model("kimi-k2-0711-preview"), "kimi-k2-0711-preview");
    }

    #[test]
    fn catalog_has_default_first() {
        std::env::remove_var("KIMI_MODELS");
        let cat = kimi_model_catalog();
        assert_eq!(cat[0].slug, "kimi");
        assert!(cat.iter().any(|m| m.slug == "kimi/kimi-k2-0711-preview"));
    }
}
