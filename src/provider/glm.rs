//! GLM provider (Zhipu / z.ai). An API-key endpoint provider — no OAuth, no
//! token refresh — that can serve BOTH client protocols:
//!
//!   1. Claude-format traffic (`/v1/messages`): proxied near-natively to GLM's
//!      Anthropic-compatible endpoint (`{base_url_alt}/v1/messages`). The request
//!      and response are already Anthropic-shaped, so the gateway buffers and
//!      returns them verbatim — tool calls survive. No Claude Code fingerprint is
//!      injected (GLM is not Anthropic).
//!   2. Codex-format traffic (`/v1/responses`): GLM has no Responses API, so the
//!      request is normalized onto GLM's OpenAI-compatible
//!      `{base_url}/chat/completions` via a local Responses↔Chat Completions
//!      adapter (`convert_responses_to_chat_messages` + `convert_responses_tools`),
//!      then re-rendered in the client's format. The adapter preserves
//!      `function_call` / `function_call_output` blocks — tool calls round-trip,
//!      same as minimax / DeepSeek. Streaming is real per-token SSE translation
//!      through the shared `translate_openai_sse_to_responses` in
//!      `routes::proxy`.
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
use crate::util::truncate_text;

/// Built-in default GLM model, used when neither the runtime override nor
/// `GLM_DEFAULT_MODEL` supplies one. Selected by the bare `glm` slug, and the
/// shared built-in for all three Claude Code tier slots (GLM has only one
/// model family in its catalog, so the three slots collapse to the same value
/// unless the operator overrides them on the model-map panel).
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "glm-5.2";
pub(crate) const BUILTIN_OPUS_MODEL: &str = "glm-5.2";
pub(crate) const BUILTIN_SONNET_MODEL: &str = "glm-5.2";
pub(crate) const BUILTIN_FABLE_MODEL: &str = "glm-5.2";

/// STATIC FALLBACK model list, used only when the live `/models` fetch fails
/// (e.g. no GLM account connected yet). This list can lag behind GLM's actual
/// catalog — `get_glm_models` prefers the live list, and an override (runtime
/// edit or `GLM_MODELS`) pins the list outright. Any model id also works
/// directly via `glm/<id>` regardless of whether it appears here.
pub(crate) const BUILTIN_MODELS: &[&str] =
    &["glm-5.2", "glm-4.6", "glm-4.5", "glm-4.5-air", "glm-4.5-x", "glm-4-flash"];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("glm").expect("glm model spec")
}

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
/// `glm-4.6`; a native `glm-4.6` -> unchanged; a bare `glm` -> the configured
/// default. Claude Code traffic arrives as `claude-*` names, which are rewritten
/// via the standard tier rewrite: opus → opus slot, sonnet (with haiku folded
/// in) → sonnet slot, fable → fable slot, anything else → default slot.
pub(crate) fn glm_canonical_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_ascii_lowercase();
    if m.eq_ignore_ascii_case("glm") {
        return glm_default_model();
    }
    if let Some(rest) = m
        .strip_prefix("glm/")
        .or_else(|| m.strip_prefix("Glm/"))
        .or_else(|| m.strip_prefix("GLM/"))
    {
        return rest.to_string();
    }
    // Native `glm-<id>` ids (e.g. `glm-4.6`, `glm-4.5-air`) pass through to the
    // upstream unchanged — GLM's catalog uses its own ids and doesn't go
    // through the tier rewrite.
    if lower.starts_with("glm-") {
        return m.to_string();
    }
    if lower.contains("opus") {
        return glm_opus_model();
    }
    if lower.contains("haiku") || lower.contains("sonnet") {
        return glm_sonnet_model();
    }
    if lower.contains("fable") {
        return glm_fable_model();
    }
    glm_default_model()
}

fn glm_default_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Default)
}

fn glm_opus_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Opus)
}

fn glm_sonnet_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Sonnet)
}

fn glm_fable_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Fable)
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
    /// Parsed `tool_calls` from the upstream `choices[0].message.tool_calls`
    /// array (each entry's `id` / `function.name` / `function.arguments`).
    pub(crate) tool_calls: Vec<GlmToolCall>,
}

/// A single Chat-Completions-shaped tool call from GLM.
#[derive(Debug, Clone)]
pub(crate) struct GlmToolCall {
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
///                                 parts like `input_image` are skipped — GLM
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
                    // Assistant turn that called a tool. Chat Completions
                    // represents this as an assistant message with a
                    // `tool_calls` array (content can be null or empty).
                    let id = it
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| it.get("id").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();
                    let name = it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // Responses API `arguments` is already a JSON STRING
                    // (matches OpenAI Chat Completions convention).
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
                    // Tool result echoed back. Chat Completions uses a
                    // `role: tool` message keyed by `tool_call_id`.
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
                    // ...): skip silently. GLM is text + tools only; passing
                    // these through would either 400 or be ignored.
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

/// Convert a Responses API `tools` array into Chat Completions `tools`. Codex
/// (Responses API) emits function tools in the **flat** shape
/// (`{type, name, description, parameters, strict}`); Chat Completions
/// expects the **wrapped** shape
/// (`{type: "function", function: {name, description, parameters, strict}}`).
/// Forwarding the flat shape verbatim makes GLM / MiniMax / Kimi / DeepSeek
/// reject the request as `invalid params, function is empty` and return an
/// empty `choices:null` body. We rewrap. Anything already in wrapped shape
/// passes through (rare, but cheap to handle).
pub(crate) fn convert_responses_tools(payload: &Value) -> Option<Vec<Value>> {
    let tools = payload.get("tools").and_then(|v| v.as_array())?;
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let Some(obj) = t.as_object() else { continue };
        let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("function");
        if ty != "function" {
            // Non-function tools (web_search, file_search, ...) have no
            // Chat Completions equivalent on these surfaces — drop silently
            // rather than 400 the whole request.
            continue;
        }
        if obj.contains_key("function") {
            out.push(t.clone());
            continue;
        }
        let mut function = serde_json::Map::new();
        for k in ["name", "description", "parameters", "strict"] {
            if let Some(v) = obj.get(k) {
                function.insert(k.to_string(), v.clone());
            }
        }
        if function.get("name").and_then(|v| v.as_str()).map(str::is_empty).unwrap_or(true) {
            continue;
        }
        out.push(json!({
            "type": "function",
            "function": Value::Object(function),
        }));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Build the OpenAI Chat Completions request body for GLM from a Responses
/// API payload. Always `stream: false` — the buffered caller overrides to
/// `true` itself when streaming.
fn build_glm_openai_body(model: &str, payload: &Value) -> Value {
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

/// Send one chat request to GLM's OpenAI-compatible `/chat/completions` and
/// return the assistant text + parsed tool_calls + real token usage.
pub(crate) async fn send_glm_openai(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
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
    let body = build_glm_openai_body(model, payload);

    let resp = glm_http_client()
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
        let parsed = parse_glm_error_message(&text_body);
        let up = crate::util::format_upstream_error("glm", status, &text_body, parsed);
        warn!(
            "upstream_error_body provider=glm status={} parser_hit={} body={:?}",
            status.as_u16(),
            up.parser_hit,
            up.body_excerpt,
        );
        return Ok(GlmResult {
            text: String::new(),
            status,
            error: Some(up.detail),
            usage: TokenUsage::default(),
            tool_calls: Vec::new(),
        });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid glm response JSON: {}", e))?;
    if let Some(err) = parse_glm_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(GlmResult {
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
    let tool_calls = parse_glm_tool_calls(message);

    Ok(GlmResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("glm", &text_body),
        tool_calls,
    })
}

/// Streaming sibling of `send_glm_openai`: forces `stream: true` on the wire
/// and returns the upstream `reqwest::Response` so the caller can read the
/// SSE chunks and translate them event-by-event.
pub(crate) async fn send_glm_openai_streaming(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = glm_openai_base(account);
    if base.is_empty() {
        return Err("glm account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("glm account has empty api key".to_string());
    }
    let url = format!("{}/chat/completions", base);
    let mut body = build_glm_openai_body(model, payload);
    body["stream"] = json!(true);
    glm_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header("Accept", "text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("glm streaming request failed ({}): {}", url, e))
}

/// Extract `tool_calls` from a Chat Completions response's `choices[0].message`.
/// Each upstream entry carries `id` / `function.name` / `function.arguments`
/// (the arguments are a JSON string, kept verbatim).
fn parse_glm_tool_calls(message: Option<&Value>) -> Vec<GlmToolCall> {
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
        out.push(GlmToolCall { id, name, arguments });
    }
    out
}

/// GLM follows the OpenAI error shape (`{"error":{"message":"..."}}`) but also
/// tolerates a bare `{"error":"..."}`. `pub(crate)` so the proxy layer can
/// parse the same shape on the streaming path's non-success reads.
pub(crate) fn parse_glm_error_message(body: &str) -> Option<String> {
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

/// The STATIC fallback model catalog: the runtime override if set, else
/// `GLM_MODELS`, else the built-in list. Used when no live list is available.
pub(crate) fn glm_model_catalog() -> Vec<ModelInfo> {
    models_from_ids(spec().catalog())
}

/// Fetch the LIVE model list from GLM's OpenAI-compatible `GET {base}/models`.
/// Returns the upstream ids mapped to `glm/<id>` slugs. Errors (no OpenAI base,
/// bad key, endpoint absent) bubble up so the caller can fall back to the static
/// catalog. A pinned catalog (runtime override or `GLM_MODELS`) short-circuits
/// the network call — otherwise the live list would immediately overwrite it.
pub(crate) async fn fetch_glm_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    if spec().catalog_pinned() {
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
            if let Some(msg) = parse_glm_error_message(&body) {
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

    #[test]
    fn responses_to_chat_messages_preserves_tool_calls() {
        let payload = json!({
            "instructions": "You help with tools.",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "what's the weather in SF?" }
                ] },
                { "type": "function_call", "call_id": "call_1", "name": "get_weather",
                  "arguments": "{\"city\":\"SF\"}" },
                { "type": "function_call_output", "call_id": "call_1",
                  "output": "{\"temp_f\":62,\"sky\":\"foggy\"}" },
                { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "It's 62F and foggy." }
                ] },
            ],
            "tools": [
                { "type": "function", "name": "get_weather",
                  "parameters": { "type": "object" } }
            ],
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        // 5 entries: system + user + assistant(tool_calls) + tool + assistant(text).
        // The trailing `assistant` message is kept as its own entry — Chat
        // Completions clients accept consecutive assistant messages, and
        // merging would lose the textual reply that follows the tool call.
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"].as_str().unwrap().contains("tools"));
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"city\":\"SF\"}");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["role"], "assistant");
        assert!(msgs[4]["content"].as_str().unwrap().contains("62F"));

        let tools = convert_responses_tools(&payload).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn responses_to_chat_messages_drops_image_and_unknown_blocks() {
        let payload = json!({
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "describe this" },
                    { "type": "input_image", "image_url": "https://…" },
                ] },
                { "type": "reasoning", "summary": "thinking..." },
                { "type": "web_search_call", "query": "weather" },
            ],
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        // Only the user message survives (image dropped, unknown blocks skipped).
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "describe this");
    }

    #[test]
    fn parse_glm_tool_calls_extracts_function_entries() {
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call_a", "type": "function",
                          "function": { "name": "f1", "arguments": "{}" } },
                        { "id": "call_b", "type": "function",
                          "function": { "name": "f2", "arguments": "{\"x\":1}" } },
                    ]
                }
            }]
        }).to_string();
        let tcs = parse_glm_tool_calls(Some(&serde_json::from_str::<Value>(&body).unwrap()
            .pointer("/choices/0/message").unwrap()));
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].id, "call_a");
        assert_eq!(tcs[1].name, "f2");
        assert_eq!(tcs[1].arguments, "{\"x\":1}");
    }

    #[test]
    fn parse_glm_error_message_nested_and_flat() {
        assert_eq!(
            parse_glm_error_message(r#"{"error":{"message":"insufficient balance"}}"#).as_deref(),
            Some("insufficient balance")
        );
        assert_eq!(
            parse_glm_error_message(r#"{"error":"key invalid"}"#).as_deref(),
            Some("key invalid")
        );
        assert!(parse_glm_error_message("<html>500</html>").is_none());
    }
}
