//! GLM provider (Zhipu / z.ai). An API-key endpoint provider — no OAuth, no
//! token refresh — that can serve BOTH client protocols:
//!
//!   1. Claude-format traffic (`/v1/messages`): proxied near-natively to GLM's
//!      Anthropic-compatible endpoint (`{base_url_alt}/v1/messages`). The request
//!      and response are already Anthropic-shaped, so the gateway buffers and
//!      returns them verbatim — tool calls survive. No Claude Code fingerprint is
//!      injected (GLM is not Anthropic).
//!   2. Codex-format traffic (`/v1/responses`): GLM has no Responses API, so the
//!      request is normalized through the shared format adapter
//!      (`provider::cursor::{extract_request, build_*_body}`) onto GLM's
//!      OpenAI-compatible `{base_url}/chat/completions`, then re-rendered in the
//!      client's format. This path is buffered text only (no tool round-trips),
//!      the same tradeoff as the ollama path.
//!
//! An "account" carries:
//!   * `base_url`     — the OpenAI-compatible prefix (e.g. `https://open.bigmodel.cn/api/paas/v4`
//!                      or `https://api.z.ai/api/paas/v4`); `/chat/completions` is appended.
//!   * `base_url_alt` — the Anthropic-compatible prefix (e.g. `https://open.bigmodel.cn/api/anthropic`
//!                      or `https://api.z.ai/api/anthropic`); `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the GLM API key (bearer auth).
//!
//! Token counts are REAL here (both endpoints return usage objects), so audited
//! usage is exact — see `usage::tokens::parse_usage("glm", ...)`.
use crate::prelude::*;
use crate::provider::cursor::ExtractedRequest;
use crate::util::truncate_text;

/// Default GLM model when a request selects the bare `glm` slug (override with
/// `GLM_DEFAULT_MODEL`).
const FALLBACK_DEFAULT_MODEL: &str = "glm-5.2";

/// STATIC FALLBACK model list, used only when the live `/models` fetch fails
/// (e.g. no GLM account connected yet). This list can lag behind GLM's actual
/// catalog — `get_glm_models` prefers the live list, and `GLM_MODELS`
/// (comma-separated) overrides the whole thing. Any model id also works
/// directly via `glm/<id>` regardless of whether it appears here.
const DEFAULT_GLM_MODELS: &[&str] =
    &["glm-5.2", "glm-4.6", "glm-4.5", "glm-4.5-air", "glm-4.5-x", "glm-4-flash"];

/// Dedicated HTTP client for GLM. A short connect timeout (fail fast on the
/// fallback path) and a generous total timeout (long generations).
pub(crate) fn glm_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("GLM_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building glm http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the GLM upstream. Accepts the explicit
/// `glm/<model>` form, a bare `glm` (→ default model), or any native `glm-*`
/// model id (e.g. `glm-4.6`).
pub(crate) fn is_glm_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "glm" || m.starts_with("glm/") || m.starts_with("glm-")
}

/// Maps a gateway model name to the upstream GLM model id. `glm/glm-4.6` ->
/// `glm-4.6`; a bare `glm` -> the configured default; `glm-4.6` -> unchanged.
pub(crate) fn glm_canonical_model(model: &str) -> String {
    let m = model.trim();
    if m.eq_ignore_ascii_case("glm") {
        return std::env::var("GLM_DEFAULT_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| FALLBACK_DEFAULT_MODEL.to_string());
    }
    if let Some(rest) = m
        .strip_prefix("glm/")
        .or_else(|| m.strip_prefix("Glm/"))
        .or_else(|| m.strip_prefix("GLM/"))
    {
        return rest.to_string();
    }
    m.to_string()
}

/// The OpenAI-compatible base prefix for a GLM account: its stored `base_url`,
/// else the `GLM_BASE_URL` env. Trailing slash trimmed. Empty if unset.
pub(crate) fn glm_openai_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("GLM_BASE_URL").ok().map(|v| v.trim().to_string()).unwrap_or_default()
    };
    raw.trim_end_matches('/').to_string()
}

/// The Anthropic-compatible base prefix for a GLM account: its stored
/// `base_url_alt`, else the `GLM_ANTHROPIC_BASE_URL` env. Empty if unset.
pub(crate) fn glm_anthropic_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url_alt.trim().is_empty() {
        account.base_url_alt.trim().to_string()
    } else {
        std::env::var("GLM_ANTHROPIC_BASE_URL").ok().map(|v| v.trim().to_string()).unwrap_or_default()
    };
    raw.trim_end_matches('/').to_string()
}

/// Whether this account can serve OpenAI-format traffic (the adapter path).
pub(crate) fn supports_openai(account: &UpstreamAccount) -> bool {
    !glm_openai_base(account).is_empty()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upstream call (adapter path, used for Codex-format traffic)
// ---------------------------------------------------------------------------

/// Outcome of a GLM OpenAI-compatible call.
pub(crate) struct GlmResult {
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

/// Send one chat request to GLM's OpenAI-compatible `/chat/completions` and
/// return the assistant text plus real token usage. Always non-streaming — the
/// gateway buffers the whole reply and re-renders it in the client's format.
pub(crate) async fn send_glm_openai(
    client: &reqwest::Client,
    account: &UpstreamAccount,
    model: &str,
    req: &ExtractedRequest,
) -> Result<GlmResult, String> {
    let base = glm_openai_base(account);
    if base.is_empty() {
        return Err("glm account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("glm account has empty api key".to_string());
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
        .map_err(|e| format!("glm upstream request failed ({}): {}", url, e))?;
    let status = resp.status();
    let text_body = resp
        .text()
        .await
        .map_err(|e| format!("reading glm upstream body failed: {}", e))?;

    if !status.is_success() {
        let detail = parse_error_message(&text_body)
            .unwrap_or_else(|| format!("glm upstream returned {}", status));
        return Ok(GlmResult { text: String::new(), status, error: Some(detail), usage: TokenUsage::default() });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid glm response JSON: {}", e))?;
    if let Some(err) = parse_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(GlmResult { text: String::new(), status, error: Some(err), usage: TokenUsage::default() });
        }
    }

    let content = value
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(GlmResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("glm", &text_body),
    })
}

/// Pull a human-readable error out of a GLM error body. GLM follows the OpenAI
/// shape (`{"error":{"message":"..."}}`) but also tolerates a bare
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

/// Send a raw Anthropic-shaped payload to GLM's Anthropic-compatible
/// `/v1/messages` and return the upstream response for the caller to buffer.
/// No fingerprint injection: GLM is not Anthropic, so the Claude Code system
/// blocks / tool obfuscation must NOT be applied.
pub(crate) async fn send_glm_anthropic(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = glm_anthropic_base(account);
    if base.is_empty() {
        return Err("glm account has no Anthropic-compatible base_url_alt".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("glm account has empty api key".to_string());
    }
    let url = format!("{}/v1/messages", base);
    glm_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("Accept", "text/event-stream, application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("failed to call glm anthropic upstream ({}): {}", url, e))
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// Build the gateway-facing model list from a set of upstream model ids: a bare
/// `glm` default entry first, then each id as `glm/<id>` (the prefix is stripped
/// before the upstream call).
fn models_from_ids(ids: impl IntoIterator<Item = String>) -> Vec<ModelInfo> {
    let mut out = vec![ModelInfo {
        slug: "glm".to_string(),
        display_name: "glm (default)".to_string(),
    }];
    for id in ids {
        let id = id.trim().to_string();
        if !id.is_empty() {
            out.push(ModelInfo { slug: format!("glm/{}", id), display_name: id });
        }
    }
    out
}

/// The STATIC fallback model catalog: `GLM_MODELS` override if set, else the
/// built-in `DEFAULT_GLM_MODELS`. Used when no live list is available.
pub(crate) fn glm_model_catalog() -> Vec<ModelInfo> {
    let names: Vec<String> = match std::env::var("GLM_MODELS") {
        Ok(v) if !v.trim().is_empty() => {
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        }
        _ => DEFAULT_GLM_MODELS.iter().map(|s| s.to_string()).collect(),
    };
    models_from_ids(names)
}

/// Fetch the LIVE model list from GLM's OpenAI-compatible `GET {base}/models`.
/// Returns the upstream ids mapped to `glm/<id>` slugs. Errors (no OpenAI base,
/// bad key, endpoint absent) bubble up so the caller can fall back to the static
/// catalog. An explicit `GLM_MODELS` override short-circuits the network call.
pub(crate) async fn fetch_glm_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    if std::env::var("GLM_MODELS").map(|v| !v.trim().is_empty()).unwrap_or(false) {
        return Ok(glm_model_catalog());
    }
    let base = glm_openai_base(account);
    if base.is_empty() {
        return Err("glm account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("glm account has empty api key".to_string());
    }
    let url = format!("{}/models", base);
    let resp = glm_http_client()
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("failed to reach glm models api ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("reading glm models body failed: {}", e))?;
    if !status.is_success() {
        return Err(format!("glm models api error {}: {}", status.as_u16(), truncate_text(&body, 200)));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid glm models response: {}", e))?;
    // OpenAI shape: {"object":"list","data":[{"id":"glm-4.6",...}, ...]}.
    let arr = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "glm models response missing `data` array".to_string())?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    if ids.is_empty() {
        return Err("glm models response had no model ids".to_string());
    }
    Ok(models_from_ids(ids))
}

/// Probe reachability of a GLM account by issuing a tiny OpenAI-compatible
/// request. Used at connect time to validate base_url + api key before storing.
pub(crate) async fn probe_glm(account: &UpstreamAccount) -> Result<(), String> {
    let base = glm_openai_base(account);
    let anthropic = glm_anthropic_base(account);
    if base.is_empty() && anthropic.is_empty() {
        return Err("至少要填一个 base_url（OpenAI 兼容）或 base_url_alt（Anthropic 兼容）".to_string());
    }
    if account.bearer().is_empty() {
        return Err("GLM api key 不能为空".to_string());
    }
    // Prefer the OpenAI-compat endpoint for the probe (cheap minimal request).
    if !base.is_empty() {
        let model = glm_canonical_model("glm");
        let url = format!("{}/chat/completions", base);
        let resp = glm_http_client()
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
            .map_err(|e| format!("无法连接 GLM ({}): {}", url, e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // 401/403 => bad key; other non-2xx with a parseable error => surface it.
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!("GLM 鉴权失败 ({}): {}", status.as_u16(), truncate_text(&body, 200)));
        }
        if !status.is_success() {
            if let Some(msg) = parse_error_message(&body) {
                // A model/quota error still proves the endpoint+key are valid.
                let lower = msg.to_ascii_lowercase();
                if lower.contains("auth") || lower.contains("api key") || lower.contains("apikey") {
                    return Err(format!("GLM 鉴权失败: {}", msg));
                }
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
        assert!(is_glm_model("glm"));
        assert!(is_glm_model("glm/glm-4.6"));
        assert!(is_glm_model("glm-4.5-air"));
        assert!(is_glm_model("GLM-4.6"));
        assert!(!is_glm_model("gpt-5"));
        assert!(!is_glm_model("claude-sonnet-4"));
        assert!(!is_glm_model("ollama/llama3"));
        assert_eq!(glm_canonical_model("glm/glm-4.6"), "glm-4.6");
        assert_eq!(glm_canonical_model("glm-4.5"), "glm-4.5");
    }

    #[test]
    fn catalog_has_default_first() {
        std::env::remove_var("GLM_MODELS");
        let cat = glm_model_catalog();
        assert_eq!(cat[0].slug, "glm");
        assert!(cat.iter().any(|m| m.slug == "glm/glm-4.6"));
    }
}
