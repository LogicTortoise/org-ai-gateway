//! MiniMax provider (MiniMax 海螺 / MiniMax-M series). An API-key endpoint
//! provider — no OAuth, no token refresh.
//!
//! ## Shape — BOTH client protocols
//!
//! Like GLM / Kimi, this provider wires two endpoints and serves two client
//! protocols:
//!
//!   1. **Claude-format traffic** (`/v1/messages`): proxied near-natively to
//!      MiniMax's Anthropic-compatible endpoint (`{base_url_anthropic}/v1/messages`).
//!      Request and response are already Anthropic-shaped, so the gateway
//!      buffers and returns them verbatim — **tool calls survive**. No Claude
//!      Code fingerprint is injected (MiniMax is not Anthropic).
//!
//!   2. **Codex / OpenAI-format traffic** (`/v1/responses`): proxied straight
//!      through to MiniMax's native Responses API at
//!      `{base_url_openai}/v1/responses`. The Codex CLI is configured with
//!      `wire_api = "responses"` per the official MiniMax integration guide
//!      (`platform.minimaxi.com/docs/token-plan/codex`); the payload matches
//!      MiniMax's wire shape exactly and needs no translation. Both
//!      `stream: true` and `stream: false` are forwarded as-is.
//!
//! An "account" carries:
//!   * `base_url` — OpenAI-compatible prefix; defaults to `MINIMAX_BASE_URL` env,
//!     else `https://api.minimaxi.com`. `/v1/responses` is appended. The base
//!     URL must NOT include `/v1` — `MINIMAX_RESPONSES_PATH` already carries
//!     that prefix, so a base ending in `/v1` produces a doubled `/v1/v1/...`
//!     and a 404. Override to `https://api.minimax.io` for the international
//!     site.
//!   * `base_url_alt` — Anthropic-compatible prefix; defaults to
//!     `MINIMAX_ANTHROPIC_BASE_URL` env, else `https://api.minimaxi.com/anthropic`.
//!     `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the MiniMax API key. Both endpoints accept
//!     `Authorization: Bearer`.
//!
//! Token counts are REAL on the Anthropic path (miniMax returns
//! Anthropic-shaped usage). On the Responses path the upstream also returns
//! real `usage.input_tokens` / `usage.output_tokens` /
//! `usage.input_tokens_details.cached_tokens`.
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

/// The MiniMax OpenAI-compatible base prefix (mainland site). The base URL
/// MUST end at the host — it must NOT include `/v1` or any path segment.
/// The MiniMax Codex endpoint is `{base}/v1/responses`; appending `/v1` again
/// would produce a doubled `/v1/v1/responses` and a 404.
const BUILTIN_OPENAI_BASE: &str = "https://api.minimaxi.com";

/// The MiniMax OpenAI-compatible Codex endpoint path. MiniMax serves the
/// Responses API natively — the Codex client (`wire_api = "responses"` in
/// its config.toml) sends a vanilla Responses payload (`input[]` /
/// `instructions` / `tools`) and gets a vanilla Responses payload back. No
/// conversion is needed; this gateway is a transparent pipe. Hitting
/// `/v1/chat/completions` or `/v1/text/chatcompletion_v2` instead would
/// either 404 or return the cryptic
/// `{"base_resp":{"status_code":2013,"status_msg":"invalid params, chat
/// content is empty"}}` shape (because the Chat Completions adapter on top of
/// the Responses endpoint can't parse a Responses-shaped request).
const MINIMAX_RESPONSES_PATH: &str = "/v1/responses";

/// Built-in MiniMax Anthropic-compatible endpoint (mainland site). Used when
/// neither the account nor `MINIMAX_ANTHROPIC_BASE_URL` supplies one.
const BUILTIN_ANTHROPIC_BASE: &str = "https://api.minimaxi.com/anthropic";

/// Dedicated HTTP client for MiniMax. Short connect timeout (fail fast on the
/// fallback path) and a generous total timeout (long generations). Shared
/// between the Anthropic and OpenAI paths — MiniMax is one provider with two
/// endpoints, not two providers with different policies.
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

/// The OpenAI-compatible base prefix for a MiniMax account: its stored
/// `base_url`, else the `MINIMAX_BASE_URL` env, else the built-in OpenAI
/// endpoint. Trailing slash trimmed. Empty if unset (account lacks an OpenAI
/// endpoint AND env is empty — should never happen with the built-in default,
/// but kept consistent with the Anthropic helper).
pub(crate) fn minimax_openai_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("MINIMAX_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| BUILTIN_OPENAI_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

/// The Anthropic-compatible base prefix for a MiniMax account: its stored
/// `base_url_alt`, else the `MINIMAX_ANTHROPIC_BASE_URL` env, else the built-in
/// Anthropic endpoint. Trailing slash trimmed.
pub(crate) fn minimax_anthropic_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url_alt.trim().is_empty() {
        account.base_url_alt.trim().to_string()
    } else {
        std::env::var("MINIMAX_ANTHROPIC_BASE_URL")
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
    !minimax_openai_base(account).is_empty()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upstream call (Codex slot) — Responses-API passthrough
// ---------------------------------------------------------------------------
//
// The Codex slot (`wire_api = "responses"` in `config.toml`) sends OpenAI
// Responses payloads: a top-level `input` array of typed blocks
// (`message` / `function_call` / `function_call_output` / `reasoning` / …),
// a top-level `instructions` string, and a top-level `tools` array. Back
// it gets a Responses-shaped response (`output[]` of typed blocks plus
// `usage` with `input_tokens_details.cached_tokens` / `output_tokens`).
//
// MiniMax serves the **Responses API natively** at
// `{base_url}/v1/responses` — same shape in, same shape out, no conversion.
// The Codex CLI itself is configured to talk to MiniMax this way per the
// official integration guide (`platform.minimaxi.com/docs/token-plan/codex`).
//
// This is a deliberate departure from the previous design, which rewrote
// Responses → Chat Completions and posted to `/v1/text/chatcompletion_v2`.
// That conversion was unnecessary — and worse, MiniMax's
// `/v1/text/chatcompletion_v2` is a thin Chat Completions adapter layered
// over the Responses endpoint, so it returned the cryptic
// `{"base_resp":{"status_code":2013,"status_msg":"invalid params, chat
// content is empty"}}` shape when handed a Responses-shaped payload it
// couldn't parse. Forwarding the same payload to `/v1/responses` directly
// just works (verified live).
//
// The gateway is therefore a transparent pipe on this path: it rewrites
// only `model` (to a MiniMax catalog id) and `stream` (forces `true` on
// the streaming sibling), and forwards everything else byte-for-byte.
// Both senders return the raw `reqwest::Response` so the caller can
// either buffer it (non-streaming) or translate SSE events event-by-event
// (streaming).

/// Streaming caller for MiniMax's `/v1/responses`. The upstream is **always**
/// called with `stream: true` — `ensure_codex_payload_defaults` forces it on
/// every Codex payload before dispatch, so even non-streaming clients must
/// consume an SSE response. The gateway then either pipes the bytes through
/// (streaming client) or buffers the whole stream and aggregates it back into
/// a Responses JSON object via `sse::aggregate_codex_sse_to_response_json`
/// (non-streaming client).
pub(crate) async fn send_minimax_responses_streaming(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = minimax_openai_base(account);
    if base.is_empty() {
        return Err("minimax account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("minimax account has empty api key".to_string());
    }

    let url = format!("{}{}", base, MINIMAX_RESPONSES_PATH);
    let mut body = payload.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
        obj.insert("stream".to_string(), Value::Bool(true));
    }

    minimax_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("Accept", "text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("minimax streaming request failed ({}): {}", url, e))
}

/// Pull a human-readable error out of a MiniMax error body. Two shapes
/// seen in the wild:
///
///   * OpenAI-shape: `{"error":{"message":"..."}}` or bare `{"error":"..."}`
///     — auth / quota / rate-limit failures land here (and now also into
///     `/v1/responses` auth/quota rejections, which the OpenAI Responses
///     surface uses).
///   * MiniMax-shape: `{"base_resp":{"status_code":2013,
///     "status_msg":"..."}, ...}` — what the upstream returns when the
///     request itself is rejected by the `/v1/text/chatcompletion_v2`
///     adapter. We no longer call that surface for Codex, but the probe
///     still uses this parser to detect auth failures on either surface.
///
/// Surfacing the `status_code` lets the operator grep for the specific
/// failure mode (e.g. `2013` for context-window overflow).
pub(crate) fn parse_minimax_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    // MiniMax-shape: top-level `base_resp.status_msg` (+ status_code if present).
    if let Some(base) = v.get("base_resp").and_then(|b| b.as_object()) {
        let msg = base.get("status_msg").and_then(|m| m.as_str());
        let code = base.get("status_code").and_then(|c| c.as_i64());
        if let Some(m) = msg {
            return Some(match code {
                Some(c) => format!("minimax error {}: {}", c, m),
                None => m.to_string(),
            });
        }
    }
    // OpenAI-shape: `error` as object with `.message`, or a bare string.
    if let Some(err) = v.get("error") {
        if let Some(s) = err.as_str() {
            return Some(s.to_string());
        }
        if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
            return Some(m.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, used for Claude-format traffic)
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
    let base = minimax_anthropic_base(account);
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

/// Probe reachability of a MiniMax account at connect time. Dual-path:
/// prefers the OpenAI-compatible surface (the new default for Codex slot) and
/// falls back to the Anthropic-compatible surface — so an operator who hasn't
/// migrated yet (Anthropic URL still in `base_url` or `MINIMAX_BASE_URL`) still
/// gets a successful probe against the Anthropic path. A 401/403 on either
/// path is fatal (the key is wrong); other non-success codes (model/quota
/// complaint, 429) still count as "endpoint reachable + key accepted".
pub(crate) async fn probe_minimax(account: &UpstreamAccount) -> Result<(), String> {
    if account.bearer().is_empty() {
        return Err("MiniMax api key 不能为空".to_string());
    }
    let openai_base = minimax_openai_base(account);
    let anthropic_base = minimax_anthropic_base(account);

    // Migration safety: if the explicit OpenAI base URL points at the
    // Anthropic surface (an operator who hasn't migrated their `base_url` /
    // `MINIMAX_BASE_URL` yet), don't probe that as OpenAI — it'd 404. Skip
    // straight to Anthropic.
    let openai_usable = !openai_base.is_empty() && !openai_base.contains("/anthropic");

    if openai_usable {
        match probe_minimax_openai(account, &openai_base).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if anthropic_base.is_empty() {
                    return Err(e);
                }
                tracing::warn!(error = %e, "minimax openai probe failed, trying anthropic");
                return probe_minimax_anthropic(account, &anthropic_base).await;
            }
        }
    }
    if !anthropic_base.is_empty() {
        return probe_minimax_anthropic(account, &anthropic_base).await;
    }
    Err("MiniMax 缺少 base_url".to_string())
}

/// Probe the OpenAI-compatible `/v1/responses` surface with a minimal
/// Responses payload. A 200 OK / 4xx (other than 401/403) means the key works
/// and the endpoint is reachable; 401/403 means wrong key.
async fn probe_minimax_openai(account: &UpstreamAccount, base: &str) -> Result<(), String> {
    let url = format!("{}{}", base, MINIMAX_RESPONSES_PATH);
    let resp = minimax_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": minimax_canonical_model("minimax"),
            "input": [
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "ping" }] }
            ],
            "max_output_tokens": 1,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 MiniMax Responses ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "MiniMax 鉴权失败 ({}): {}",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    if let Some(msg) = parse_minimax_error_message(&body) {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("api key") || lower.contains("apikey") {
            return Err(format!("MiniMax 鉴权失败: {}", msg));
        }
    }
    Ok(())
}

async fn probe_minimax_anthropic(account: &UpstreamAccount, base: &str) -> Result<(), String> {
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
        .map_err(|e| format!("无法连接 MiniMax Anthropic ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "MiniMax 鉴权失败 ({}): {}",
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
    fn openai_base_defaults_and_normalizes() {
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
        assert_eq!(minimax_openai_base(&acc), BUILTIN_OPENAI_BASE);
        // Explicit base wins, trailing slash stripped.
        acc.base_url = "https://api.minimax.io/".into();
        assert_eq!(minimax_openai_base(&acc), "https://api.minimax.io");
        // Env override (no account base) wins over the built-in default.
        std::env::set_var("MINIMAX_BASE_URL", "https://env.example");
        acc.base_url.clear();
        assert_eq!(minimax_openai_base(&acc), "https://env.example");
        std::env::remove_var("MINIMAX_BASE_URL");
        assert!(supports_openai(&acc));
    }

    #[test]
    fn anthropic_base_defaults_and_normalizes() {
        std::env::remove_var("MINIMAX_ANTHROPIC_BASE_URL");
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
        assert_eq!(minimax_anthropic_base(&acc), BUILTIN_ANTHROPIC_BASE);
        // Explicit alt wins, trailing slash stripped.
        acc.base_url_alt = "https://api.minimax.io/anthropic/".into();
        assert_eq!(minimax_anthropic_base(&acc), "https://api.minimax.io/anthropic");
    }

    #[test]
    fn parse_minimax_error_message_recognizes_base_resp_shape() {
        // 1. MiniMax-shape: the actual upstream error format on
        //    context-window overflow — `choices:null, usage:null` and only
        //    `base_resp.status_msg` carries the message. Without this branch
        //    the parser returned None and the caller reported
        //    `minimax_empty_response` (silent failure).
        let body = r#"{"choices":null,"usage":null,"base_resp":{"status_code":2013,"status_msg":"invalid params, chat content is empty"}}"#;
        let msg = parse_minimax_error_message(body).expect("base_resp branch must fire");
        assert!(msg.contains("2013"), "status_code must be surfaced: {}", msg);
        assert!(msg.contains("invalid params, chat content is empty"));

        // 2. base_resp without status_msg falls through to None (rather than
        //    producing a half-baked "minimax error None: ").
        let no_msg = r#"{"base_resp":{"status_code":2013}}"#;
        assert!(parse_minimax_error_message(no_msg).is_none());

        // 3. OpenAI-shape still works (auth/quota errors use this form).
        let oai = r#"{"error":{"message":"insufficient balance","type":"balance"}}"#;
        assert_eq!(
            parse_minimax_error_message(oai).as_deref(),
            Some("insufficient balance")
        );

        // 4. Bare OpenAI-style `error` string still works.
        let bare = r#"{"error":"rate limited"}"#;
        assert_eq!(
            parse_minimax_error_message(bare).as_deref(),
            Some("rate limited")
        );

        // 5. base_resp without status_msg but with a recognizable OpenAI
        //    error field — OpenAI branch should still fire (base_resp
        //    doesn't shadow it).
        let both = r#"{"base_resp":{"status_code":401},"error":{"message":"bad key"}}"#;
        assert_eq!(
            parse_minimax_error_message(both).as_deref(),
            Some("bad key"),
            "base_resp without status_msg must not shadow the OpenAI branch"
        );

        // 6. Not JSON at all → None.
        assert!(parse_minimax_error_message("not json").is_none());
    }
}
