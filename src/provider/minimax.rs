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
use base64::Engine;
use std::io::Cursor;

/// Built-in default upstream model, used when neither the runtime override nor
/// `MINIMAX_DEFAULT_MODEL` supplies one. Also the built-in for all three Claude
/// Code tiers — MiniMax has only one model family in its catalog, so the
/// three slots collapse to the same value unless the operator overrides them.
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_OPUS_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_SONNET_MODEL: &str = "MiniMax-M3";
pub(crate) const BUILTIN_FABLE_MODEL: &str = "MiniMax-M3";

/// Built-in reasoning-effort tiers for MiniMax's `/v1/responses`. MiniMax only
/// distinguishes `none` (think-off) and `high` (Deep thinking), so every tier
/// maps to `high` — the operator can point a tier at `none` if they want to
/// switch lightweight requests to think-off.
pub(crate) const MINIMAX_DEFAULT_EFFORT: &str = "high";
pub(crate) const MINIMAX_EFFORT_LOW: &str = "high";
pub(crate) const MINIMAX_EFFORT_MEDIUM: &str = "high";
pub(crate) const MINIMAX_EFFORT_HIGH: &str = "high";
pub(crate) const MINIMAX_EFFORT_XHIGH: &str = "high";

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

/// MiniMax image generation is not OpenAI-wire-compatible. The gateway's
/// `/v1/images/generations` route translates to this endpoint and normalizes
/// the result back into an OpenAI Images response.
const MINIMAX_IMAGE_PATH: &str = "/v1/image_generation";
const MINIMAX_IMAGE_MODEL: &str = "image-01";
const MAX_MINIMAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

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

/// Convert an OpenAI Images generation payload into MiniMax's image-01 shape.
/// Unsupported semantics return an error so the route can fall through to the
/// native Codex/OpenAI image provider without silently changing the request.
pub(crate) fn build_minimax_image_request(payload: &Value) -> Result<Value, String> {
    let obj = payload
        .as_object()
        .ok_or_else(|| "image request body must be a JSON object".to_string())?;
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "image request is missing `prompt`".to_string())?;
    if prompt.chars().count() > 1500 {
        return Err("MiniMax image prompts are limited to 1500 characters".to_string());
    }
    if obj.get("background").and_then(Value::as_str) == Some("transparent") {
        return Err("MiniMax image-01 does not support transparent backgrounds".to_string());
    }

    let requested_model = obj.get("model").and_then(Value::as_str).unwrap_or("");
    let image_model = if requested_model.eq_ignore_ascii_case("image-01-live")
        || requested_model.eq_ignore_ascii_case("minimax/image-01-live")
    {
        "image-01-live"
    } else {
        MINIMAX_IMAGE_MODEL
    };
    let n = obj.get("n").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=9).contains(&n) {
        return Err("MiniMax image count must be between 1 and 9".to_string());
    }

    let mut body = json!({
        "model": image_model,
        "prompt": prompt,
        "response_format": "base64",
        "n": n,
        "prompt_optimizer": false,
        "aigc_watermark": false,
    });
    let body_obj = body.as_object_mut().expect("image body is an object");
    match obj.get("size").and_then(Value::as_str).unwrap_or("auto") {
        "auto" => {
            body_obj.insert("aspect_ratio".to_string(), Value::String("1:1".to_string()));
        }
        raw => {
            let (width, height) = parse_minimax_image_size(raw)?;
            body_obj.insert("width".to_string(), json!(width));
            body_obj.insert("height".to_string(), json!(height));
        }
    }
    if let Some(seed) = obj.get("seed").and_then(Value::as_i64) {
        body_obj.insert("seed".to_string(), json!(seed));
    }
    Ok(body)
}

fn parse_minimax_image_size(raw: &str) -> Result<(u32, u32), String> {
    let (width, height) = raw
        .split_once('x')
        .ok_or_else(|| format!("unsupported MiniMax image size `{}`", raw))?;
    let width = width
        .parse::<u32>()
        .map_err(|_| format!("unsupported MiniMax image size `{}`", raw))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| format!("unsupported MiniMax image size `{}`", raw))?;
    if !(512..=2048).contains(&width)
        || !(512..=2048).contains(&height)
        || width % 8 != 0
        || height % 8 != 0
    {
        return Err(format!("unsupported MiniMax image size `{}`", raw));
    }
    Ok((width, height))
}

pub(crate) async fn send_minimax_image_generation(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = minimax_openai_base(account);
    if base.is_empty() {
        return Err("minimax account has no image-generation base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("minimax account has empty api key".to_string());
    }
    let url = format!("{}{}", base, MINIMAX_IMAGE_PATH);
    minimax_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("minimax image request failed ({}): {}", url, e))
}

/// Normalize MiniMax's `{data.image_base64[]}` response into the OpenAI Images
/// `{data:[{b64_json}]}` contract. MiniMax currently returns JPEG bytes; Codex
/// saves built-in image results as `.png`, so transcode every result to a real
/// PNG instead of emitting a misleading extension.
pub(crate) fn normalize_minimax_image_response(body: &[u8]) -> Result<Value, String> {
    if body.len() > MAX_MINIMAX_IMAGE_BYTES {
        return Err("MiniMax image response exceeded the 32 MiB limit".to_string());
    }
    let parsed: Value = serde_json::from_slice(body)
        .map_err(|e| format!("invalid MiniMax image response: {}", e))?;
    let status_code = parsed
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if status_code != 0 {
        let message = parsed
            .pointer("/base_resp/status_msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown MiniMax image error");
        return Err(format!("MiniMax image error {}: {}", status_code, message));
    }
    let images = parsed
        .pointer("/data/image_base64")
        .and_then(Value::as_array)
        .ok_or_else(|| "MiniMax image response is missing data.image_base64".to_string())?;
    if images.is_empty() {
        return Err("MiniMax image response contained no images".to_string());
    }

    let mut data = Vec::with_capacity(images.len());
    let mut response_size: Option<String> = None;
    for encoded in images {
        let encoded = encoded
            .as_str()
            .ok_or_else(|| "MiniMax image response contained a non-string image".to_string())?;
        let encoded = encoded.rsplit_once(',').map(|(_, data)| data).unwrap_or(encoded);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("invalid MiniMax image base64: {}", e))?;
        let decoded = image::load_from_memory(&bytes)
            .map_err(|e| format!("invalid MiniMax image bytes: {}", e))?;
        response_size.get_or_insert_with(|| format!("{}x{}", decoded.width(), decoded.height()));
        let mut png = Cursor::new(Vec::new());
        decoded
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| format!("failed to encode MiniMax image as PNG: {}", e))?;
        data.push(json!({
            "b64_json": base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        }));
    }
    Ok(json!({
        "created": Utc::now().timestamp(),
        "data": data,
        "output_format": "png",
        "size": response_size.unwrap_or_default(),
    }))
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

/// Whether an upstream error message is MiniMax's "input image flagged as
/// sensitive" rejection. These land as an HTTP 4xx *before* any model work:
/// `input new_sensitive, messages[N]'s content[K] image is sensitive, please
/// check your input (1026)`. The account is fine — only the payload trips the
/// content filter — so the gateway strips the offending image and retries
/// instead of surfacing a fatal input error the Codex client can't recover from.
pub(crate) fn is_sensitive_image_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("image is sensitive") || lower.contains("new_sensitive")
}

/// Whether MiniMax rejected a legacy Codex Responses input shape.
pub(crate) fn is_strict_input_error(msg: &str) -> bool {
    msg.contains("input is neither string nor array of items")
}

/// Rewrite a Responses payload so every `input_image` content block inside an
/// `input` array's `message` items is replaced by an `input_text` placeholder.
/// MiniMax's content filter rejects the whole request when any image is
/// sensitive, so dropping the image and telling the model it was omitted lets
/// the turn complete (the model can proceed without it) rather than wedging the
/// Codex message queue. Returns a new value; the input is not mutated.
pub(crate) fn strip_sensitive_images(payload: &Value) -> Value {
    let mut out = payload.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    let Some(input) = obj.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return out;
    };
    for item in input.iter_mut() {
        let Some(item_obj) = item.as_object_mut() else {
            continue;
        };
        if item_obj.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let Some(content) = item_obj.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for block in content.iter_mut() {
            let Some(block_obj) = block.as_object_mut() else {
                continue;
            };
            if block_obj.get("type").and_then(|t| t.as_str()) == Some("input_image") {
                *block = json!({
                    "type": "input_text",
                    "text": "[image omitted: upstream content filter flagged it as sensitive; proceed without it]",
                });
            }
        }
    }
    out
}

/// Normalize legacy message items for MiniMax's strict Responses schema.
pub(crate) fn normalize_codex_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    let Some(input) = obj.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return out;
    };
    for item in input.iter_mut() {
        let Some(item_obj) = item.as_object_mut() else {
            continue;
        };
        let item_type = item_obj.get("type").and_then(|t| t.as_str());
        if !matches!(item_type, Some("message") | None) {
            continue;
        }
        if item_obj.get("type").is_none() {
            item_obj.insert("type".to_string(), Value::String("message".to_string()));
        }
        if let Some(content) = item_obj.get_mut("content") {
            if let Some(text) = content.as_str() {
                *content = json!([{
                    "type": "input_text",
                    "text": text,
                }]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, used for Claude-format traffic)
// ---------------------------------------------------------------------------

/// Pure selection helper: decide what model id to forward to MiniMax's
/// Anthropic-compatible endpoint.
///
/// `resolved_model` is the literal upstream id selected by model routing. When
/// present, `minimax_canonical_model` is bypassed. Otherwise we read `payload`'s
/// `model` and let the canonical mapping produce the right MiniMax id (this is
/// the legacy global-chain path; MiniMax resolves ids against its own catalog
/// and rejects foreign `claude-*` names, so the canonical step is mandatory
/// for non-MiniMax client input).
pub(crate) fn pick_minimax_model(resolved_model: Option<&str>, payload: &serde_json::Map<String, Value>) -> String {
    match resolved_model {
        Some(m) => m.to_string(),
        None => {
            let requested = payload
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            minimax_canonical_model(&requested)
        }
    }
}

/// Send an Anthropic-shaped payload to MiniMax's `/v1/messages` and return the
/// upstream response for the caller to buffer.
///
/// The payload is forwarded as-is except for `model`. A matched model route is
/// sent literally; without one, canonical rewriting still runs so a Claude
/// Code `claude-*` name degrades onto the right MiniMax slot. MiniMax resolves
/// ids against its own catalog, so a foreign name (typically `claude-*`, since
/// this provider exists as a Claude fallback) is rewritten first.
pub(crate) async fn send_minimax_anthropic(
    account: &UpstreamAccount,
    resolved_model: Option<&str>,
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
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "model".to_string(),
            Value::String(pick_minimax_model(resolved_model, obj)),
        );
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
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};

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
    fn openai_image_payload_maps_to_minimax_image_01() {
        let body = build_minimax_image_request(&json!({
            "model": "gpt-image-2",
            "prompt": "draw a small orange robot",
            "size": "1024x1536",
            "n": 2,
            "quality": "high",
        }))
        .unwrap();
        assert_eq!(body["model"], "image-01");
        assert_eq!(body["response_format"], "base64");
        assert_eq!(body["width"], 1024);
        assert_eq!(body["height"], 1536);
        assert_eq!(body["n"], 2);
        assert_eq!(body["aigc_watermark"], false);
        assert!(body.get("quality").is_none());
    }

    #[test]
    fn unsupported_minimax_image_semantics_fall_through() {
        assert!(build_minimax_image_request(&json!({
            "prompt": "transparent icon",
            "background": "transparent",
        }))
        .is_err());
        assert!(build_minimax_image_request(&json!({
            "prompt": "oversized",
            "size": "3840x2160",
        }))
        .is_err());
        assert!(build_minimax_image_request(&json!({
            "prompt": "too many",
            "n": 10,
        }))
        .is_err());
    }

    #[test]
    fn minimax_jpeg_response_becomes_openai_png_response() {
        let source = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(8, 8, Rgb([255, 128, 0])));
        let mut jpeg = Cursor::new(Vec::new());
        source.write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg.into_inner());
        let upstream = serde_json::to_vec(&json!({
            "data": {"image_base64": [encoded]},
            "base_resp": {"status_code": 0, "status_msg": "success"},
        }))
        .unwrap();
        let normalized = normalize_minimax_image_response(&upstream).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(normalized["data"][0]["b64_json"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(normalized["output_format"], "png");
        assert_eq!(normalized["size"], "8x8");
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

    #[test]
    fn sensitive_image_error_detection() {
        assert!(is_sensitive_image_error(
            "input new_sensitive, messages[112]'s content[0] image is sensitive, please check your input (1026)"
        ));
        assert!(is_sensitive_image_error("image is sensitive"));
        assert!(is_sensitive_image_error("something new_sensitive happened"));
        // Unrelated errors must not trip the strip-and-retry path.
        assert!(!is_sensitive_image_error("insufficient balance"));
        assert!(!is_sensitive_image_error("rate limited"));
        assert!(!is_sensitive_image_error("invalid params, chat content is empty"));
    }

    #[test]
    fn strict_input_error_detection() {
        assert!(is_strict_input_error(
            "Invalid request: input is neither string nor array of items: Mismatch type string with value..."
        ));
        assert!(is_strict_input_error(
            "upstream says: input is neither string nor array of items"
        ));
        assert!(!is_strict_input_error(""));
        assert!(!is_strict_input_error("rate limited"));
        assert!(!is_strict_input_error("input must be an array"));
    }

    #[test]
    fn strip_sensitive_images_replaces_input_image_blocks() {
        let payload = json!({
            "model": "MiniMax-M3",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe this"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                    ],
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "unrelated",
                },
            ],
        });
        let stripped = strip_sensitive_images(&payload);
        let content = stripped
            .pointer("/input/0/content")
            .and_then(|c| c.as_array())
            .unwrap();
        assert_eq!(content.len(), 2, "input_image must become input_text, not be dropped");
        assert_eq!(content[0]["type"], "input_text", "text block untouched");
        assert_eq!(content[1]["type"], "input_text", "image block rewritten");
        assert!(content[1]["text"].as_str().unwrap().contains("omitted"));
        // Non-message items are untouched.
        assert_eq!(stripped.pointer("/input/1/type").unwrap(), "function_call_output");
        // Original payload is not mutated.
        assert_eq!(payload.pointer("/input/0/content/1/type").unwrap(), "input_image");
    }

    #[test]
    fn strip_sensitive_images_ignores_missing_input_or_non_message() {
        let no_input = json!({"model": "MiniMax-M3"});
        assert_eq!(strip_sensitive_images(&no_input), no_input);

        // A message with string content (not an array) is left alone.
        let string_content = json!({
            "input": [{"type": "message", "role": "user", "content": "plain text"}],
        });
        assert_eq!(strip_sensitive_images(&string_content), string_content);
    }

    #[test]
    fn normalize_codex_payload_wraps_string_content_and_adds_type() {
        let payload = json!({
            "input": [
                {"role": "user", "content": "plain legacy string"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            ],
        });
        let normalized = normalize_codex_payload(&payload);
        assert_eq!(normalized.pointer("/input/0/type").unwrap(), "message");
        assert_eq!(
            normalized.pointer("/input/0/content").unwrap(),
            &json!([{"type": "input_text", "text": "plain legacy string"}])
        );
        assert_eq!(
            normalized.pointer("/input/1/type").unwrap(),
            "function_call"
        );
        assert!(payload.pointer("/input/0/content").unwrap().is_string());
    }

    #[test]
    fn normalize_codex_payload_leaves_valid_or_unrelated_shapes_alone() {
        let valid = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "already valid"}]
            }]
        });
        assert_eq!(normalize_codex_payload(&valid), valid);

        let no_input = json!({"model": "MiniMax-M3"});
        assert_eq!(normalize_codex_payload(&no_input), no_input);

        let string_input = json!({"input": "raw text"});
        assert_eq!(normalize_codex_payload(&string_input), string_input);
    }

    #[test]
    fn pick_minimax_model_returns_resolved_verbatim_without_synthetic_prefix() {
        // A model route must be forwarded exactly as-is.
        let mut obj = serde_json::Map::new();
        obj.insert("model".into(), json!("claude-sonnet-4-5"));
        assert_eq!(pick_minimax_model(Some("MiniMax-M3"), &obj), "MiniMax-M3");
        assert_eq!(pick_minimax_model(Some("minimax/MiniMax-M3"), &obj), "minimax/MiniMax-M3");
        assert_eq!(pick_minimax_model(Some("minimax-test"), &obj), "minimax-test");
    }

    #[test]
    fn pick_minimax_model_falls_back_to_canonical_when_unresolved() {
        let mut obj = serde_json::Map::new();
        obj.insert("model".into(), json!("claude-sonnet-4-5"));
        assert_eq!(
            pick_minimax_model(None, &obj),
            minimax_canonical_model("claude-sonnet-4-5")
        );
        let empty = serde_json::Map::new();
        assert_eq!(pick_minimax_model(None, &empty), minimax_canonical_model(""));
    }
}
