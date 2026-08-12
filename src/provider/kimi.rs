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
//!   2. Codex-format traffic (`/v1/responses`): Kimi has no Responses API, so
//!      the request is normalized onto Kimi's OpenAI-compatible
//!      `{base_url}/chat/completions` via a local Responses↔Chat Completions
//!      adapter (`convert_responses_to_chat_messages` + `convert_responses_tools`),
//!      then re-rendered in the client's format. The adapter preserves
//!      `function_call` / `function_call_output` blocks — tool calls round-trip,
//!      same as minimax / DeepSeek / GLM. Streaming is real per-token SSE
//!      translation through the shared `translate_openai_sse_to_responses` in
//!      `routes::proxy`.
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
use crate::util::truncate_text;

/// Built-in default Kimi model, used when neither the runtime override nor
/// `KIMI_DEFAULT_MODEL` supplies one. Also the shared built-in for all three
/// Claude Code tier slots (Kimi has only one model family in its catalog, so
/// the three slots collapse to the same value unless the operator overrides
/// them on the model-map panel).
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "kimi-k2-0711-preview";
pub(crate) const BUILTIN_OPUS_MODEL: &str = "kimi-k2-0711-preview";
pub(crate) const BUILTIN_SONNET_MODEL: &str = "kimi-k2-0711-preview";
pub(crate) const BUILTIN_FABLE_MODEL: &str = "kimi-k2-0711-preview";

/// Built-in static fallback model list, used only when the live `/models`
/// fetch fails (e.g. no Kimi account connected yet). Can lag behind Moonshot's
/// actual catalog — `fetch_kimi_models` prefers the live list, and an override
/// (runtime edit or `KIMI_MODELS`) pins the list outright. Any model id also
/// works directly via `kimi/<id>` regardless of whether it appears here.
pub(crate) const BUILTIN_MODELS: &[&str] = &[
    "kimi-k2-0711-preview",
    "kimi-k2-turbo-preview",
    "kimi-latest",
    "moonshot-v1-8k",
    "moonshot-v1-32k",
    "moonshot-v1-128k",
];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("kimi").expect("kimi model spec")
}

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
/// `kimi-*` / `moonshot-*` id -> unchanged. Claude Code traffic arrives as
/// `claude-*` names, which are rewritten via the standard tier rewrite: opus →
/// opus slot, sonnet (with haiku folded in) → sonnet slot, fable → fable slot,
/// anything else → default slot.
pub(crate) fn kimi_canonical_model(model: &str) -> String {
    let m = model.trim();
    if m.eq_ignore_ascii_case("kimi") || m.eq_ignore_ascii_case("moonshot") {
        return kimi_default_model();
    }
    let lower = m.to_ascii_lowercase();
    if lower.starts_with("kimi/") {
        return m["kimi/".len()..].to_string();
    }
    if lower.starts_with("moonshot/") {
        return m["moonshot/".len()..].to_string();
    }
    if lower.starts_with("kimi-") || lower.starts_with("moonshot-") {
        return m.to_string();
    }
    if lower.contains("opus") {
        return kimi_opus_model();
    }
    if lower.contains("haiku") || lower.contains("sonnet") {
        return kimi_sonnet_model();
    }
    if lower.contains("fable") {
        return kimi_fable_model();
    }
    kimi_default_model()
}

fn kimi_default_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Default)
}

fn kimi_opus_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Opus)
}

fn kimi_sonnet_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Sonnet)
}

fn kimi_fable_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Fable)
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
    /// Parsed `tool_calls` from the upstream `choices[0].message.tool_calls`
    /// array (each entry's `id` / `function.name` / `function.arguments`).
    pub(crate) tool_calls: Vec<KimiToolCall>,
}

/// A single Chat-Completions-shaped tool call from Kimi.
#[derive(Debug, Clone)]
pub(crate) struct KimiToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The raw `function.arguments` string from the upstream. It's already
    /// JSON-shaped, so we pass it through to the client without re-parsing.
    pub(crate) arguments: String,
}

/// Build the Chat Completions `messages` array from a Responses API payload.
///
/// Walks the `input` array, converting each block:
///
///   * `type: "message"`        -> `role: user|assistant|system|developer` with
///                                 text content (concatenated from all
///                                 `input_text`/`output_text` parts; non-text
///                                 parts like `input_image` are skipped — Kimi
///                                 is text-only on most models).
///   * `type: "function_call"`  -> `role: assistant` with a synthetic
///                                 `tool_calls` entry (id / name / arguments).
///   * `type: "function_call_output"` -> `role: tool` with `tool_call_id` set
///                                 and `content` carrying the tool output.
///
/// `instructions` (top-level) and any `system` / `developer` messages inside
/// `input` become a leading `role: system` message.
pub(crate) fn convert_responses_to_chat_messages(payload: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    // 1. Top-level `instructions` (Responses API) -> system message.
    let mut sys_buf = String::new();
    if let Some(s) = payload.get("instructions").and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            sys_buf.push_str(s);
        }
    }

    // 2. Walk `input`.
    if let Some(items) = payload.get("input").and_then(|v| v.as_array()) {
        for item in items {
            let Some(it) = item.as_object() else { continue };
            let t = it.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match t {
                "message" => {
                    let role = it.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    let content = match it.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(parts)) => collect_text_parts(parts),
                        _ => String::new(),
                    };
                    match role {
                        "system" | "developer" => {
                            if !sys_buf.is_empty() {
                                sys_buf.push('\n');
                            }
                            sys_buf.push_str(&content);
                        }
                        "assistant" => {
                            out.push(json!({ "role": "assistant", "content": content }));
                        }
                        _ => {
                            // Default + "user" + unknown: treat as user. Codex
                            // doesn't emit "tool" messages via `input` — those
                            // come through as `function_call_output`.
                            out.push(json!({ "role": "user", "content": content }));
                        }
                    }
                }
                "function_call" => {
                    let id = it
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| it.get("id").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();
                    let name = it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let arguments = match it.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    out.push(json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments }
                        }]
                    }));
                }
                "function_call_output" => {
                    let call_id = it.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let output = it.get("output").map(|v| {
                        if let Value::String(s) = v {
                            s.clone()
                        } else {
                            v.to_string()
                        }
                    }).unwrap_or_default();
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output
                    }));
                }
                _ => {
                    // Unknown / unsupported block types (`reasoning`,
                    // `web_search_call`, `file_search_call`, `computer_call`,
                    // ...): skip silently.
                }
            }
        }
    }

    if !sys_buf.is_empty() {
        out.insert(0, json!({ "role": "system", "content": sys_buf }));
    }
    out
}

/// Concatenate text from an OpenAI / Responses content-part array, ignoring
/// non-text parts (images, etc.).
fn collect_text_parts(parts: &[Value]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for p in parts {
        let Some(part) = p.as_object() else { continue };
        if let Some(t) = part.get("type").and_then(|v| v.as_str()) {
            if !matches!(t, "text" | "input_text" | "output_text") {
                continue;
            }
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(text);
            }
        }
    }
    out.join("\n")
}

/// Convert a Responses API `tools` array into Chat Completions `tools`. The two
/// shapes are identical for the only tool type the gateway forwards
/// (`type: "function"`), so this is a passthrough.
pub(crate) fn convert_responses_tools(payload: &Value) -> Option<Vec<Value>> {
    let tools = payload.get("tools").and_then(|v| v.as_array())?;
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let Some(obj) = t.as_object() else { continue };
        if obj.get("type").and_then(|v| v.as_str()).unwrap_or("function") == "function" {
            out.push(t.clone());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Build the OpenAI Chat Completions request body for Kimi from a Responses
/// API payload. Always `stream: false` — the buffered caller overrides to
/// `true` itself when streaming.
fn build_kimi_openai_body(model: &str, payload: &Value) -> Value {
    let mut body = json!({
        "model": model,
        "messages": convert_responses_to_chat_messages(payload),
        "stream": false,
    });
    if let Some(tools) = convert_responses_tools(payload) {
        body["tools"] = json!(tools);
        if let Some(tc) = payload.get("tool_choice") {
            body["tool_choice"] = tc.clone();
        }
    }
    body
}

/// Send one chat request to Kimi's OpenAI-compatible `/chat/completions` and
/// return the assistant text + parsed tool_calls + real token usage.
pub(crate) async fn send_kimi_openai(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
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
    let body = build_kimi_openai_body(model, payload);

    let resp = kimi_http_client()
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
        let detail = parse_kimi_error_message(&text_body)
            .unwrap_or_else(|| format!("kimi upstream returned {}", status));
        return Ok(KimiResult {
            text: String::new(),
            status,
            error: Some(detail),
            usage: TokenUsage::default(),
            tool_calls: Vec::new(),
        });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid kimi response JSON: {}", e))?;
    if let Some(err) = parse_kimi_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(KimiResult {
                text: String::new(),
                status,
                error: Some(err),
                usage: TokenUsage::default(),
                tool_calls: Vec::new(),
            });
        }
    }

    let message = value.pointer("/choices/0/message");
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool_calls = parse_kimi_tool_calls(message);

    Ok(KimiResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("kimi", &text_body),
        tool_calls,
    })
}

/// Streaming sibling of `send_kimi_openai`: forces `stream: true` on the wire
/// and returns the upstream `reqwest::Response` so the caller can read the
/// SSE chunks and translate them event-by-event.
pub(crate) async fn send_kimi_openai_streaming(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = kimi_openai_base(account);
    if base.is_empty() {
        return Err("kimi account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("kimi account has empty api key".to_string());
    }
    let url = format!("{}/chat/completions", base);
    let mut body = build_kimi_openai_body(model, payload);
    body["stream"] = json!(true);
    kimi_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("Accept", "text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("kimi streaming request failed ({}): {}", url, e))
}

/// Extract `tool_calls` from a Chat Completions response's `choices[0].message`.
/// Each upstream entry carries `id` / `function.name` / `function.arguments`
/// (the arguments are a JSON string, kept verbatim).
fn parse_kimi_tool_calls(message: Option<&Value>) -> Vec<KimiToolCall> {
    let Some(arr) = message.and_then(|m| m.get("tool_calls")).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for tc in arr {
        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = tc
            .pointer("/function/name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments = tc
            .pointer("/function/arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() && name.is_empty() && arguments.is_empty() {
            continue;
        }
        out.push(KimiToolCall { id, name, arguments });
    }
    out
}

/// Moonshot follows the OpenAI error shape (`{"error":{"message":"..."}}`) but
/// also tolerates a bare `{"error":"..."}`. `pub(crate)` so the proxy layer
/// can parse the same shape on the streaming path's non-success reads.
pub(crate) fn parse_kimi_error_message(body: &str) -> Option<String> {
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

/// The STATIC fallback model catalog: runtime override, else `KIMI_MODELS`,
/// else the built-in list. Used when no live list is available.
pub(crate) fn kimi_model_catalog() -> Vec<ModelInfo> {
    models_from_ids(spec().catalog())
}

/// Fetch the LIVE model list from Kimi's OpenAI-compatible `GET {base}/models`.
/// Returns the upstream ids mapped to `kimi/<id>` slugs. Errors (bad key,
/// endpoint absent) bubble up so the caller can fall back to the static catalog.
/// A pinned catalog (runtime override or `KIMI_MODELS`) short-circuits the
/// network call — otherwise the live list would immediately overwrite it.
pub(crate) async fn fetch_kimi_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    if spec().catalog_pinned() {
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
        if let Some(msg) = parse_kimi_error_message(&body) {
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

    #[test]
    fn responses_to_chat_messages_preserves_tool_calls() {
        let payload = json!({
            "instructions": "Help with tools.",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "look up foo" }
                ] },
                { "type": "function_call", "call_id": "call_2", "name": "lookup",
                  "arguments": "{\"q\":\"foo\"}" },
                { "type": "function_call_output", "call_id": "call_2",
                  "output": "bar" },
                { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "found: bar" }
                ] },
            ],
            "tools": [
                { "type": "function", "name": "lookup",
                  "parameters": { "type": "object" } }
            ],
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        // 5 entries: system + user + assistant(tool_calls) + tool + assistant(text).
        // Mirrors the GLM-side test — the trailing `assistant` message is
        // kept as its own entry rather than merged into the prior
        // assistant(tool_calls) message.
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_2");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "lookup");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_2");
        assert_eq!(msgs[4]["role"], "assistant");
        assert!(msgs[4]["content"].as_str().unwrap().contains("found"));

        let tools = convert_responses_tools(&payload).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "lookup");
    }

    #[test]
    fn responses_to_chat_messages_drops_image_and_unknown_blocks() {
        let payload = json!({
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "describe" },
                    { "type": "input_image", "image_url": "https://..." },
                ] },
                { "type": "reasoning", "summary": "thinking" },
            ],
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "describe");
    }

    #[test]
    fn parse_kimi_tool_calls_extracts_function_entries() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call_a", "type": "function",
                          "function": { "name": "f1", "arguments": "{}" } },
                    ]
                }
            }]
        });
        let msg = body.pointer("/choices/0/message").unwrap();
        let tcs = parse_kimi_tool_calls(Some(msg));
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_a");
        assert_eq!(tcs[0].name, "f1");
    }

    #[test]
    fn parse_kimi_error_message_nested_and_flat() {
        assert_eq!(
            parse_kimi_error_message(r#"{"error":{"message":"insufficient balance"}}"#).as_deref(),
            Some("insufficient balance")
        );
        assert_eq!(
            parse_kimi_error_message(r#"{"error":"key invalid"}"#).as_deref(),
            Some("key invalid")
        );
        assert!(parse_kimi_error_message("<html>500</html>").is_none());
    }
}
