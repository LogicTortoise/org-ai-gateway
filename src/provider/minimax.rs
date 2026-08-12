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
//!   2. **Codex / OpenAI-format traffic** (`/v1/responses`,
//!      `/v1/chat/completions`): proxied to MiniMax's OpenAI-compatible
//!      endpoint (`{base_url_openai}/v1/text/chatcompletion_v2`). The payload
//!      is rewritten from OpenAI Responses (`input` array of typed blocks +
//!      top-level `tools` + `instructions`) into OpenAI Chat Completions
//!      (`messages` + `tools`), and the response is rewritten back. **Function
//!      calling survives** (this is what distinguishes minimax from the GLM/
//!      Kimi text-only adapter). Non-streaming only on the OpenAI path —
//!      streaming tool-call deltas are aggregated into a single
//!      `response.output_item.done` event instead of incremental deltas, so
//!      streaming clients still see the tool call, just with first-token delay.
//!
//! An "account" carries:
//!   * `base_url` — OpenAI-compatible prefix; defaults to `MINIMAX_BASE_URL` env,
//!     else `https://api.minimaxi.com`. `/v1/text/chatcompletion_v2` is appended.
//!     Override to `https://api.minimax.io` for the international site. The
//!     base URL must NOT include `/v1` — `MINIMAX_OPENAI_PATH` already carries
//!     that prefix, so a base ending in `/v1` produces a doubled `/v1/v1/...`
//!     and a 404.
//!   * `base_url_alt` — Anthropic-compatible prefix; defaults to
//!     `MINIMAX_ANTHROPIC_BASE_URL` env, else `https://api.minimaxi.com/anthropic`.
//!     `/v1/messages` is appended.
//!   * `api_key` / `access_token` — the MiniMax API key. Both endpoints accept
//!     `Authorization: Bearer`.
//!
//! Token counts are REAL on both paths (Anthropic endpoint returns
//! Anthropic-shaped usage, OpenAI endpoint returns `prompt_tokens`/
//! `completion_tokens`). See `usage::tokens::parse_usage("minimax", ...)`.
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

/// Built-in MiniMax OpenAI-compatible endpoint (mainland site). Used when
/// neither the account nor `MINIMAX_BASE_URL` supplies one, so connecting only
/// needs an api key. Override to `https://api.minimax.io` for the
/// international site via `MINIMAX_BASE_URL` (or per-account `base_url`).
///
/// The base URL must end at the host (or its namespace prefix like
/// `/anthropic` for the Anthropic surface) — it must NOT include `/v1`.
/// `MINIMAX_OPENAI_PATH` carries the `/v1/...` segment, so a base ending in
/// `/v1` produces a doubled `/v1/v1/text/chatcompletion_v2` and a 404.
const BUILTIN_OPENAI_BASE: &str = "https://api.minimaxi.com";

/// Built-in MiniMax Anthropic-compatible endpoint (mainland site). Used when
/// neither the account nor `MINIMAX_ANTHROPIC_BASE_URL` supplies one.
const BUILTIN_ANTHROPIC_BASE: &str = "https://api.minimaxi.com/anthropic";

/// The MiniMax OpenAI-compatible path. This is the only `/v2` variant in this
/// codebase: MiniMax's OpenAI surface lives at `/v1/text/chatcompletion_v2`,
/// NOT the standard `/v1/chat/completions`. Hitting the standard path returns
/// 404.
const MINIMAX_OPENAI_PATH: &str = "/v1/text/chatcompletion_v2";

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
// OpenAI-compatible upstream call (Codex slot)
// ---------------------------------------------------------------------------
//
// The Codex slot sends OpenAI Responses payloads (top-level `input` array of
// typed blocks, `instructions`, `tools`). MiniMax's OpenAI surface is a
// Chat-Completions-shaped `/v1/text/chatcompletion_v2`, so we rewrite:
//   * `instructions` + `input` (message / function_call / function_call_output
//     blocks) -> `messages` array with `system`/`user`/`assistant`/`tool` roles
//   * `tools` -> Chat Completions `tools` (OpenAI standard, identical shape)
//   * upstream response (text + `tool_calls`) -> Responses API `output` array
//     (message + function_call blocks) for the non-streaming aggregation
// Streaming is supported but the gateway buffers the entire response anyway
// (account-swap retry needs the full body), so this is always non-streaming on
// the wire to MiniMax even when the client asked for `stream: true`.
//
// MiniMax supports `tools` / `tool_choice` / `tool_calls` per their v2 docs.

/// Outcome of a MiniMax OpenAI-compatible call. Mirrors `KimiResult` / `GlmResult`
/// but adds the parsed `tool_calls` so the renderer can rebuild the Responses
/// `output` array correctly.
pub(crate) struct MinimaxResult {
    pub(crate) text: String,
    pub(crate) status: reqwest::StatusCode,
    pub(crate) error: Option<String>,
    /// Real token usage parsed from the response (`usage.prompt_tokens` /
    /// `usage.completion_tokens`); zero when the upstream omitted them.
    pub(crate) usage: TokenUsage,
    /// Parsed `tool_calls` from the upstream `choices[0].message.tool_calls`
    /// array (each entry's `id` / `function.name` / `function.arguments`).
    pub(crate) tool_calls: Vec<MinimaxToolCall>,
}

/// A single Chat-Completions-shaped tool call from MiniMax.
#[derive(Debug, Clone)]
pub(crate) struct MinimaxToolCall {
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
///                                 parts like `input_image` are skipped — MiniMax
///                                 is text-only).
///   * `type: "function_call"`  -> `role: assistant` with a synthetic
///                                 `tool_calls` entry (id / name / arguments).
///   * `type: "function_call_output` (or
///     `type: "function_call_output"`) -> `role: tool` with `tool_call_id` set
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
                    // ...): skip silently. MiniMax is text + tools only; passing
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
/// non-text parts (images, etc.). Matches `cursor::text_from_parts` but is
/// private to this provider to keep the adapter surface local.
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
/// (`type: "function"`), so this is a passthrough with the `type` field
/// normalized — Responses allows omitting `type`, Chat Completions documents it
/// but accepts its absence on most servers.
pub(crate) fn convert_responses_tools(payload: &Value) -> Option<Vec<Value>> {
    let tools = payload.get("tools").and_then(|v| v.as_array())?;
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let Some(obj) = t.as_object() else { continue };
        // Pass `function` shaped tools through verbatim. Codex only emits
        // `type: "function"` tools, so we don't need to translate other
        // shapes.
        if obj.get("type").and_then(|v| v.as_str()).unwrap_or("function") == "function" {
            out.push(t.clone());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Build the OpenAI Chat Completions request body for MiniMax from a Responses
/// API payload. Always `stream: false` — the gateway buffers everything for
/// safe account-swap retry.
fn build_minimax_openai_body(model: &str, payload: &Value) -> Value {
    let mut body = json!({
        "model": model,
        "messages": convert_responses_to_chat_messages(payload),
        "stream": false,
    });
    if let Some(tools) = convert_responses_tools(payload) {
        body["tools"] = json!(tools);
        // Honor the client's `tool_choice` if it set one. Default `auto` would
        // be set by the upstream on its own.
        if let Some(tc) = payload.get("tool_choice") {
            body["tool_choice"] = tc.clone();
        }
    }
    body
}

/// Send one chat request to MiniMax's OpenAI-compatible
/// `/v1/text/chatcompletion_v2` and return the assistant text + parsed
/// tool_calls + real token usage. Always non-streaming.
pub(crate) async fn send_minimax_openai(
    account: &UpstreamAccount,
    model: &str,
    payload: &Value,
) -> Result<MinimaxResult, String> {
    let base = minimax_openai_base(account);
    if base.is_empty() {
        return Err("minimax account has no OpenAI-compatible base_url".to_string());
    }
    let api_key = account.bearer();
    if api_key.is_empty() {
        return Err("minimax account has empty api key".to_string());
    }

    let url = format!("{}{}", base, MINIMAX_OPENAI_PATH);
    let body = build_minimax_openai_body(model, payload);

    let resp = minimax_http_client()
        .post(&url)
        .bearer_auth(api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("minimax upstream request failed ({}): {}", url, e))?;
    let status = resp.status();
    let text_body = resp
        .text()
        .await
        .map_err(|e| format!("reading minimax upstream body failed: {}", e))?;

    if !status.is_success() {
        let detail = parse_minimax_error_message(&text_body)
            .unwrap_or_else(|| format!("minimax upstream returned {}", status));
        return Ok(MinimaxResult {
            text: String::new(),
            status,
            error: Some(detail),
            usage: TokenUsage::default(),
            tool_calls: Vec::new(),
        });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid minimax response JSON: {}", e))?;
    if let Some(err) = parse_minimax_error_message(&text_body) {
        if value.pointer("/choices/0/message").is_none() {
            return Ok(MinimaxResult {
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
    let tool_calls = parse_minimax_tool_calls(message);

    Ok(MinimaxResult {
        text: content,
        status,
        error: None,
        usage: crate::usage::tokens::parse_usage("minimax", &text_body),
        tool_calls,
    })
}

/// Streaming sibling of `send_minimax_openai`: forces `stream: true` on the
/// wire and returns the upstream `reqwest::Response` so the caller can read
/// the SSE chunks and translate them event-by-event. The caller is
/// responsible for parsing the chunk stream — this just opens the pipe.
pub(crate) async fn send_minimax_openai_streaming(
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
    let url = format!("{}{}", base, MINIMAX_OPENAI_PATH);
    let mut body = build_minimax_openai_body(model, payload);
    body["stream"] = json!(true);
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

/// Extract `tool_calls` from a Chat Completions response's `choices[0].message`.
/// Each upstream entry carries `id` / `function.name` / `function.arguments`
/// (the arguments are a JSON string, kept verbatim).
fn parse_minimax_tool_calls(message: Option<&Value>) -> Vec<MinimaxToolCall> {
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
        out.push(MinimaxToolCall { id, name, arguments });
    }
    out
}

/// Pull a human-readable error out of a MiniMax error body. MiniMax follows
/// the OpenAI shape (`{"error":{"message":"..."}}`) but also tolerates a bare
/// `{"error":"..."}`.
fn parse_minimax_error_message(body: &str) -> Option<String> {
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

async fn probe_minimax_openai(account: &UpstreamAccount, base: &str) -> Result<(), String> {
    let url = format!("{}{}", base, MINIMAX_OPENAI_PATH);
    let resp = minimax_http_client()
        .post(&url)
        .bearer_auth(account.bearer())
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "model": minimax_canonical_model("minimax"),
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("无法连接 MiniMax OpenAI ({}): {}", url, e))?;
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
    fn responses_to_chat_messages_preserves_tool_calls() {
        // Codex-format payload: instructions + an input array that walks
        // through user text -> assistant tool call -> tool result. The
        // conversion has to keep the `tool_calls` entry on the assistant
        // message and emit a `role: tool` message for the result.
        let payload = json!({
            "instructions": "You are a helpful assistant.",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "what's the weather in SF?" }
                ]},
                { "type": "function_call", "call_id": "call_abc",
                  "name": "get_weather", "arguments": "{\"city\":\"SF\"}" },
                { "type": "function_call_output", "call_id": "call_abc",
                  "output": "{\"temp\":68}" }
            ],
            "tools": [
                { "type": "function", "name": "get_weather",
                  "description": "get weather",
                  "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } }
            ]
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        // system + user + assistant(tool_calls) + tool = 4 entries
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"].as_str().unwrap().contains("helpful assistant"));
        assert_eq!(msgs[1]["role"], "user");
        assert!(msgs[1]["content"].as_str().unwrap().contains("weather in SF"));
        assert_eq!(msgs[2]["role"], "assistant");
        let tc = &msgs[2]["tool_calls"][0];
        assert_eq!(tc["id"], "call_abc");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], "{\"city\":\"SF\"}");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_abc");
        assert!(msgs[3]["content"].as_str().unwrap().contains("temp"));
        // And the tool definitions ride through untouched.
        let tools = convert_responses_tools(&payload).expect("non-empty tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
    }

    #[test]
    fn responses_to_chat_messages_drops_image_and_unknown_blocks() {
        // Images aren't supported by MiniMax's OpenAI surface; unknown
        // block types (`reasoning`, etc.) shouldn't blow up the conversion.
        let payload = json!({
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "look" },
                    { "type": "input_image", "image_url": "https://x/y.png" }
                ]},
                { "type": "reasoning", "id": "r_1", "summary": [{"type":"summary_text","text":"thinking"}] }
            ]
        });
        let msgs = convert_responses_to_chat_messages(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        // image dropped, text kept.
        assert_eq!(msgs[0]["content"].as_str().unwrap(), "look");
    }
}
