//! DeepSeek provider. An API-key endpoint provider — no OAuth, no token refresh.
//!
//! ## Shape — BOTH client protocols
//!
//! Like GLM / Kimi, this provider wires two endpoints and serves two client
//! protocols:
//!
//!   1. **Claude-format traffic** (`/v1/messages`): proxied near-natively to
//!      DeepSeek's Anthropic-compatible endpoint
//!      (`{base_url_anthropic}/v1/messages`). Request and response are already
//!      Anthropic-shaped, so the gateway buffers and returns them verbatim —
//!      **tool calls survive**. No Claude Code fingerprint is injected (DeepSeek
//!      is not Anthropic; their endpoint ignores `anthropic-version` anyway).
//!
//!   2. **Codex / OpenAI-format traffic** (`/v1/responses`,
//!      `/v1/chat/completions`): proxied to DeepSeek's OpenAI-compatible
//!      endpoint (`{base_url_openai}/chat/completions`). The payload is
//!      rewritten from OpenAI Responses (`input` array of typed blocks +
//!      top-level `tools` + `instructions`) into OpenAI Chat Completions
//!      (`messages` + `tools`), and the response is rewritten back. **Function
//!      calling survives** (this is what distinguishes DeepSeek from the
//!      GLM/Kimi text-only adapter). Non-streaming only on the OpenAI path;
//!      streaming tool-call deltas are aggregated into a single
//!      `response.output_item.done` event, so streaming clients still see the
//!      tool call, just with first-token delay.
//!
//! ### OpenAI vs Anthropic model id mismatch
//!
//! The two surfaces publish DIFFERENT model ids:
//!   * Anthropic surface: `deepseek-v4-pro` / `deepseek-v4-pro[1m]` /
//!     `deepseek-v4-flash` (the ids DeepSeek's Claude Code recipe uses).
//!   * OpenAI surface:    `deepseek-chat` / `deepseek-reasoner`.
//!
//! There is no 1:1 mapping between them — `deepseek-v4-pro` and
//! `deepseek-v4-flash` are the same backend served with different pricing;
//! `deepseek-chat` is the same model from a different angle. The OpenAI path
//! therefore routes ALL traffic to a single configurable id, set via
//! `DEEPSEEK_OPENAI_MODEL` (default `deepseek-chat`). The Anthropic path keeps
//! the existing tier rewrite (opus → pro, sonnet/haiku → pro, fable → flash,
//! default → pro).
//!
//! An "account" carries:
//!   * `base_url` — OpenAI-compatible prefix; defaults to `DEEPSEEK_BASE_URL`
//!     env, else `https://api.deepseek.com/v1`. `/chat/completions` is appended.
//!   * `base_url_alt` — Anthropic-compatible prefix; defaults to
//!     `DEEPSEEK_ANTHROPIC_BASE_URL` env, else `https://api.deepseek.com/anthropic`.
//!     `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the DeepSeek API key. `x-api-key` is the
//!     documented Anthropic header; `Authorization: Bearer` is what their
//!     Claude Code recipe (`ANTHROPIC_AUTH_TOKEN`) produces. The Anthropic
//!     path sends both; the OpenAI path sends only the bearer form.
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

/// Built-in DeepSeek endpoints. Used when neither the account nor the env
/// overrides supply a base URL, so connecting only needs an api key.
const BUILTIN_OPENAI_BASE: &str = "https://api.deepseek.com/v1";
const BUILTIN_ANTHROPIC_BASE: &str = "https://api.deepseek.com/anthropic";

/// Built-in model id for the OpenAI surface. DeepSeek's OpenAI catalog has
/// just two ids (`deepseek-chat` and `deepseek-reasoner`); the gateway routes
/// all Codex-slot traffic to this single configurable id. Operators who want
/// the reasoner flavor set `DEEPSEEK_OPENAI_MODEL=deepseek-reasoner`.
pub(crate) const BUILTIN_OPENAI_MODEL: &str = "deepseek-chat";

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

/// The OpenAI-compatible base prefix for a DeepSeek account: its stored
/// `base_url`, else the `DEEPSEEK_BASE_URL` env (semantically flipped: now
/// defaults to the OpenAI endpoint), else the built-in OpenAI endpoint.
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

/// Resolve the upstream model id to send on the OpenAI surface. Single
/// configurable id via `DEEPSEEK_OPENAI_MODEL`, because the Anthropic tier
/// names (`deepseek-v4-pro` etc.) are NOT valid ids on the OpenAI surface
/// (`deepseek-chat` / `deepseek-reasoner`). The Anthropic path keeps the
/// `deepseek_canonical_model` tier rewrite intact; this helper exists
/// exclusively for the OpenAI path's needs.
pub(crate) fn deepseek_openai_canonical_model(_model: &str) -> String {
    std::env::var("DEEPSEEK_OPENAI_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| BUILTIN_OPENAI_MODEL.to_string())
}

// ---------------------------------------------------------------------------
// OpenAI-compatible upstream call (Codex slot)
// ---------------------------------------------------------------------------
//
// The Codex slot sends OpenAI Responses payloads (top-level `input` array of
// typed blocks, `instructions`, `tools`). DeepSeek's OpenAI surface is a
// standard Chat Completions API at `/chat/completions`, so we rewrite:
//   * `instructions` + `input` (message / function_call / function_call_output
//     blocks) -> `messages` array with `system`/`user`/`assistant`/`tool` roles
//   * `tools` -> Chat Completions `tools` (OpenAI standard, identical shape)
//   * upstream response (text + `tool_calls`) -> Responses API `output` array
//     (message + function_call blocks) for the non-streaming aggregation
// Streaming is supported but the gateway buffers the entire response anyway
// (account-swap retry needs the full body), so this is always non-streaming on
// the wire to DeepSeek even when the client asked for `stream: true`.
//
// DeepSeek supports `tools` / `tool_choice` / `tool_calls` per their OpenAI
// docs.

/// Outcome of a DeepSeek OpenAI-compatible call.
pub(crate) struct DeepseekResult {
    pub(crate) text: String,
    pub(crate) status: reqwest::StatusCode,
    pub(crate) error: Option<String>,
    /// Real token usage parsed from the response (`usage.prompt_tokens` /
    /// `usage.completion_tokens`); zero when the upstream omitted them.
    pub(crate) usage: TokenUsage,
    /// Parsed `tool_calls` from the upstream `choices[0].message.tool_calls`
    /// array.
    pub(crate) tool_calls: Vec<DeepseekToolCall>,
}

/// A single Chat-Completions-shaped tool call from DeepSeek.
#[derive(Debug, Clone)]
pub(crate) struct DeepseekToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    /// Raw `function.arguments` string from the upstream.
    pub(crate) arguments: String,
}

/// Build the Chat Completions `messages` array from a Responses API payload.
///
/// Identical conversion to `minimax::convert_responses_to_chat_messages`; the
/// two providers share the same OpenAI surface shape (standard
/// `/chat/completions`). Kept as a private helper here rather than imported so
/// the adapter surface stays local to each provider.
pub(crate) fn convert_responses_to_chat_messages(payload: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    let mut sys_buf = String::new();
    if let Some(s) = payload.get("instructions").and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            sys_buf.push_str(s);
        }
    }

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
                _ => {}
            }
        }
    }

    if !sys_buf.is_empty() {
        out.insert(0, json!({ "role": "system", "content": sys_buf }));
    }
    out
}

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

/// Pass-through `tools` array with `type: function` entries rewrapped to the
/// Chat Completions shape. Codex (Responses API) emits function tools flat
/// (`{type, name, description, parameters, strict}`); Chat Completions
/// expects them wrapped (`{type: "function", function: {name, description,
/// parameters, strict}}`). Forwarding the flat shape makes DeepSeek's
/// `/v1/chat/completions` reject the request as `invalid params, function is
/// empty`. Already-wrapped tools pass through unchanged.
pub(crate) fn convert_responses_tools(payload: &Value) -> Option<Vec<Value>> {
    let tools = payload.get("tools").and_then(|v| v.as_array())?;
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let Some(obj) = t.as_object() else { continue };
        let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("function");
        if ty != "function" {
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

/// Build the OpenAI Chat Completions request body for DeepSeek. Always
/// `stream: false` — the gateway buffers everything for safe account-swap
/// retry.
fn build_deepseek_openai_body(model: &str, payload: &Value) -> Value {
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

/// Send one chat request to DeepSeek's OpenAI-compatible `/chat/completions`
/// and return the assistant text + parsed tool_calls + real token usage.
/// Always non-streaming.
pub(crate) async fn send_deepseek_openai(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<DeepseekResult, String> {
    let base = deepseek_openai_base(account);
    if base.is_empty() {
        return Err("deepseek account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("deepseek account has empty api key".to_string());
    }

    let url = format!("{}/chat/completions", base);
    let body = build_deepseek_openai_body(model, payload);

    let resp = deepseek_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("deepseek upstream request failed ({}): {}", url, e))?;
    let status = resp.status();
    let text_body = resp
        .text()
        .await
        .map_err(|e| format!("reading deepseek upstream body failed: {}", e))?;

    if !status.is_success() {
        let detail = parse_deepseek_error_message(&text_body)
            .unwrap_or_else(|| format!("deepseek upstream returned {}", status));
        return Ok(DeepseekResult {
            text: String::new(),
            status,
            error: Some(detail),
            usage: TokenUsage::default(),
            tool_calls: Vec::new(),
        });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid deepseek response JSON: {}", e))?;
    if let Some(err) = parse_deepseek_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(DeepseekResult {
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
    let tool_calls = parse_deepseek_tool_calls(message);

    Ok(DeepseekResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("deepseek", &text_body),
        tool_calls,
    })
}

/// Streaming sibling of `send_deepseek_openai`: forces `stream: true` and
/// returns the upstream `reqwest::Response` for caller-driven SSE parsing.
pub(crate) async fn send_deepseek_openai_streaming(
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
    let url = format!("{}/chat/completions", base);
    let mut body = build_deepseek_openai_body(model, payload);
    body["stream"] = json!(true);
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

fn parse_deepseek_tool_calls(message: Option<&Value>) -> Vec<DeepseekToolCall> {
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
        out.push(DeepseekToolCall { id, name, arguments });
    }
    out
}

/// DeepSeek follows the OpenAI error shape (`{"error":{"message":"..."}}`)
/// but also tolerates a bare `{"error":"..."}`.
fn parse_deepseek_error_message(body: &str) -> Option<String> {
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
    let url = format!("{}/chat/completions", base);
    let resp = deepseek_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": deepseek_openai_canonical_model("deepseek"),
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 DeepSeek OpenAI ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "DeepSeek 鉴权失败 ({}): {} — 请确认 API Key 正确且账户余额充足",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    if let Some(msg) = parse_deepseek_error_message(&body) {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("api key") || lower.contains("apikey") {
            return Err(format!("DeepSeek 鉴权失败: {}", msg));
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
        assert_eq!(deepseek_openai_base(&acc), BUILTIN_OPENAI_BASE);
        // Explicit base wins, trailing slash stripped.
        acc.base_url = "https://api.deepseek.com/v1/".into();
        assert_eq!(deepseek_openai_base(&acc), "https://api.deepseek.com/v1");
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

    #[test]
    fn openai_canonical_model_resolves_via_env_or_builtin() {
        std::env::remove_var("DEEPSEEK_OPENAI_MODEL");
        assert_eq!(deepseek_openai_canonical_model("deepseek"), BUILTIN_OPENAI_MODEL);
        // The input model name is intentionally ignored on the OpenAI path —
        // both surfaces publish different ids, and forcing one onto the other
        // would just 400. The single configurable id wins for everything.
        assert_eq!(deepseek_openai_canonical_model("claude-opus-4-6"), BUILTIN_OPENAI_MODEL);
        std::env::set_var("DEEPSEEK_OPENAI_MODEL", "deepseek-reasoner");
        assert_eq!(deepseek_openai_canonical_model("deepseek"), "deepseek-reasoner");
        std::env::remove_var("DEEPSEEK_OPENAI_MODEL");
    }

    #[test]
    fn responses_to_chat_messages_preserves_tool_calls() {
        let payload = json!({
            "instructions": "You are a helpful assistant.",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "summarize this" }
                ]},
                { "type": "function_call", "call_id": "call_abc",
                  "name": "summarize", "arguments": "{\"text\":\"hello\"}" },
                { "type": "function_call_output", "call_id": "call_abc",
                  "output": "summary!" }
            ],
            "tools": [
                { "type": "function", "name": "summarize",
                  "description": "summarize",
                  "parameters": { "type": "object", "properties": { "text": { "type": "string" } } } }
            ]
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        // system + user + assistant(tool_calls) + tool = 4 entries
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        let tc = &msgs[2]["tool_calls"][0];
        assert_eq!(tc["id"], "call_abc");
        assert_eq!(tc["function"]["name"], "summarize");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_abc");
        let tools = convert_responses_tools(&payload).expect("non-empty tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "summarize");
    }
}
