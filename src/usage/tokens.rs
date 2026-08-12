//! Token-usage parsing from buffered upstream responses, ported from the Go
//! `usage.go` + per-provider `ParseUsage`. Because the gateway buffers the
//! full response, we parse the complete body (SSE stream or single JSON) in one
//! pass rather than via a streaming callback.
//!
//! The token COUNTS are real (parsed from upstream usage objects), but
//! `billable_tokens` is an APPROXIMATION of cost, not a price: cache reads are
//! counted as zero (actual ~0.1x/0.25x) and Claude cache creation is ignored
//! (actual 1.25x). That's fine for its one consumer — the owner-vs-others
//! fairness split — but don't repurpose it for billing.
//!
//! Cursor is absent here on purpose: its protocol returns no usage data at all,
//! so cursor responses synthesize estimates (`cursor::estimate_text_tokens`)
//! and `audit_billable_tokens` falls back to char-length proxies.

use crate::prelude::*;
use crate::util::value_as_i64;

fn geti(node: &Value, key: &str) -> i64 {
    node.get(key).and_then(value_as_i64).unwrap_or(0)
}

fn clamp_non_negative(n: i64) -> i64 {
    n.max(0)
}

/// Collect the JSON payloads of every `data:` line in an SSE body.
fn sse_json_events(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(rest) {
            out.push(v);
        }
    }
    out
}

/// Parse token usage for a provider from a fully-buffered response body.
///
/// `trae` shares the CLAUDE parser: its sidecar exposes an Anthropic-compatible
/// surface only and never sees an OpenAI shape. `claude` is first-party
/// Anthropic. `minimax` / `deepseek` ride BOTH an Anthropic-compatible
/// endpoint AND an OpenAI-compatible endpoint (Codex slot uses the OpenAI one),
/// so they go through the GLM/Kimi dual-shape parser, which tries Anthropic
/// first and falls back to OpenAI when no anthropic-style counts appear.
pub(crate) fn parse_usage(provider: &str, body: &str) -> TokenUsage {
    let anthropic_only = matches!(provider, "claude" | "trae");
    let dual_shape = matches!(provider, "glm" | "kimi" | "minimax" | "deepseek");
    let events = sse_json_events(body);
    if !events.is_empty() {
        return if anthropic_only {
            parse_claude_events(&events)
        } else if dual_shape {
            parse_glm_events(&events)
        } else {
            parse_codex_events(&events)
        };
    }
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return if anthropic_only {
            parse_claude_json(&v)
        } else if dual_shape {
            parse_glm_json(&v)
        } else {
            parse_codex_json(&v)
        };
    }
    TokenUsage::default()
}

/// The model id the upstream says it actually ran, pulled out of a fully
/// buffered response body.
///
/// This is the ONLY authoritative answer to "which model served this request".
/// The `model` the client asked for routinely isn't it: the chain degrades a
/// `claude-sonnet-4-5` request onto DeepSeek (served by `deepseek-v4-pro`), a
/// bare `kimi` expands to a configured default, and Anthropic answers a floating
/// alias with the dated snapshot that ran (`claude-sonnet-4-5-20250929`). Only
/// the response knows.
///
/// One scan covers every shape the gateway buffers, because they all spell it
/// `model`, just at different depths:
///   * Anthropic SSE  — `message_start` → `/message/model`
///   * Codex SSE      — `response.*`    → `/response/model`
///   * OpenAI SSE     — each chunk      → `/model`
///   * any single JSON — the same three pointers
///
/// Returns `None` for unparseable bodies and error payloads (which carry no
/// `model`); callers fall back to `normalize_model_for_provider`.
pub(crate) fn parse_response_model(body: &str) -> Option<String> {
    fn from_value(v: &Value) -> Option<String> {
        for ptr in ["/message/model", "/response/model", "/model"] {
            if let Some(m) = v.pointer(ptr).and_then(|m| m.as_str()) {
                let m = m.trim();
                if !m.is_empty() {
                    return Some(m.to_string());
                }
            }
        }
        None
    }

    // SSE first: the events carry the model, the raw concatenated body isn't JSON.
    for ev in sse_json_events(body) {
        if let Some(m) = from_value(&ev) {
            return Some(m);
        }
    }
    from_value(&serde_json::from_str::<Value>(body).ok()?)
}

// ---- GLM (Zhipu / z.ai) + Kimi (Moonshot) ----
//
// GLM and Kimi both ride two endpoints with two usage shapes (they share these
// parsers):
//   * Anthropic-compatible `/v1/messages` → claude-shaped (`input_tokens` /
//     `output_tokens`, possibly streamed as message_start/message_delta events).
//   * OpenAI-compatible `/chat/completions` → openai-shaped
//     (`usage.prompt_tokens` / `usage.completion_tokens`).
// We don't know which endpoint produced a given body here, so try the Anthropic
// shape first (it carries the richer cache fields) and fall back to the OpenAI
// shape when no anthropic-style counts are present.

fn glm_openai_usage_obj(usage: &Value) -> TokenUsage {
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(value_as_i64)
        .unwrap_or(0);
    // DeepSeek R1 / OpenAI Chat Completions reasoning models (and any other
    // provider that follows the OpenAI shape) attach reasoning tokens under
    // `completion_tokens_details.reasoning_tokens`. Without this read,
    // reasoners' reasoning tokens are silently dropped from the billable
    // count and the audit row.
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(value_as_i64)
        .unwrap_or(0);
    TokenUsage {
        input_tokens: geti(usage, "prompt_tokens"),
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        output_tokens: geti(usage, "completion_tokens"),
        reasoning_tokens: reasoning,
        billable_tokens: 0,
    }
}

fn parse_glm_json(v: &Value) -> TokenUsage {
    // Anthropic-shaped usage (input_tokens/output_tokens) wins when present.
    let claude = parse_claude_json(v);
    if claude.input_tokens > 0 || claude.output_tokens > 0 {
        return claude;
    }
    // Otherwise OpenAI-shaped (prompt_tokens/completion_tokens).
    if let Some(usage) = v.get("usage") {
        let mut u = glm_openai_usage_obj(usage);
        u.billable_tokens = clamp_non_negative(u.input_tokens - u.cached_input_tokens + u.output_tokens);
        return u;
    }
    TokenUsage::default()
}

fn parse_glm_events(events: &[Value]) -> TokenUsage {
    // Anthropic-compatible streams use message_start/message_delta like Claude.
    let claude = parse_claude_events(events);
    if claude.input_tokens > 0 || claude.output_tokens > 0 {
        return claude;
    }
    // OpenAI-compatible streams attach `usage` on the final chunk.
    let mut u = TokenUsage::default();
    for ev in events {
        if let Some(usage) = ev.get("usage").filter(|u| !u.is_null()) {
            let parsed = glm_openai_usage_obj(usage);
            if parsed.input_tokens > 0 || parsed.output_tokens > 0 {
                u = parsed;
            }
        }
    }
    u.billable_tokens = clamp_non_negative(u.input_tokens - u.cached_input_tokens + u.output_tokens);
    u
}

// ---- Codex (`token_count` event / `response.usage`) ----

fn parse_codex_events(events: &[Value]) -> TokenUsage {
    let mut u = TokenUsage::default();
    for ev in events {
        if ev.get("type").and_then(|t| t.as_str()) == Some("token_count") {
            if let Some(ltu) = ev.pointer("/info/last_token_usage") {
                u.input_tokens = geti(ltu, "input_tokens");
                u.cached_input_tokens = geti(ltu, "cached_input_tokens");
                u.output_tokens = geti(ltu, "output_tokens");
                u.reasoning_tokens = geti(ltu, "reasoning_output_tokens");
            }
        }
        // `response.completed` carries a final usage object.
        if let Some(usage) = ev
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| ev.get("usage"))
        {
            let parsed = codex_usage_obj(usage);
            if parsed.input_tokens > 0 || parsed.output_tokens > 0 {
                u.input_tokens = parsed.input_tokens;
                u.cached_input_tokens = parsed.cached_input_tokens;
                u.output_tokens = parsed.output_tokens;
                u.reasoning_tokens = parsed.reasoning_tokens;
            }
        }
    }
    u.billable_tokens = clamp_non_negative(u.input_tokens - u.cached_input_tokens + u.output_tokens);
    u
}

/// Parse a single already-decoded Codex realtime event (one WebSocket frame, or
/// one SSE `data:` payload) into token usage, if it carries any. Returns `None`
/// for the many frames that don't (text deltas, item.added, etc.). The WS relay
/// sees one JSON event per frame, so it can't reuse `parse_usage`, which expects
/// a whole buffered SSE/JSON body.
pub(crate) fn parse_codex_event_usage(ev: &Value) -> Option<TokenUsage> {
    let mut u = TokenUsage::default();
    let mut found = false;
    if ev.get("type").and_then(|t| t.as_str()) == Some("token_count") {
        if let Some(ltu) = ev.pointer("/info/last_token_usage") {
            let input = geti(ltu, "input_tokens");
            let output = geti(ltu, "output_tokens");
            // An early `token_count` can be all-zero; ignore it so it doesn't
            // clobber a real usage frame the caller already recorded.
            if input > 0 || output > 0 {
                u.input_tokens = input;
                u.cached_input_tokens = geti(ltu, "cached_input_tokens");
                u.output_tokens = output;
                u.reasoning_tokens = geti(ltu, "reasoning_output_tokens");
                found = true;
            }
        }
    }
    if let Some(usage) = ev
        .get("response")
        .and_then(|r| r.get("usage"))
        .or_else(|| ev.get("usage"))
    {
        let parsed = codex_usage_obj(usage);
        if parsed.input_tokens > 0 || parsed.output_tokens > 0 {
            u = parsed;
            found = true;
        }
    }
    if !found {
        return None;
    }
    u.billable_tokens = clamp_non_negative(u.input_tokens - u.cached_input_tokens + u.output_tokens);
    Some(u)
}

fn parse_codex_json(v: &Value) -> TokenUsage {
    let usage = v
        .get("usage")
        .or_else(|| v.get("response").and_then(|r| r.get("usage")));
    let mut u = usage.map(codex_usage_obj).unwrap_or_default();
    u.billable_tokens = clamp_non_negative(u.input_tokens - u.cached_input_tokens + u.output_tokens);
    u
}

fn codex_usage_obj(usage: &Value) -> TokenUsage {
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(value_as_i64)
        .unwrap_or_else(|| geti(usage, "cache_read_input_tokens"));
    let reasoning = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(value_as_i64)
        .unwrap_or_else(|| geti(usage, "reasoning_output_tokens"));
    TokenUsage {
        input_tokens: geti(usage, "input_tokens"),
        cached_input_tokens: cached,
        cache_creation_tokens: 0,
        output_tokens: geti(usage, "output_tokens"),
        reasoning_tokens: reasoning,
        billable_tokens: 0,
    }
}

// ---- Claude (message_start + message_delta / single JSON usage) ----

fn parse_claude_events(events: &[Value]) -> TokenUsage {
    let mut u = TokenUsage::default();
    for ev in events {
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(usage) = ev.pointer("/message/usage") {
                    u.input_tokens = geti(usage, "input_tokens");
                    u.cached_input_tokens = geti(usage, "cache_read_input_tokens");
                    u.cache_creation_tokens = geti(usage, "cache_creation_input_tokens");
                }
            }
            Some("message_delta") => {
                if let Some(usage) = ev.get("usage") {
                    u.output_tokens = geti(usage, "output_tokens");
                }
            }
            _ => {}
        }
    }
    // Anthropic's `input_tokens` already excludes cache read/creation tokens
    // (the three counts are disjoint), so uncached input == input_tokens — do
    // not subtract the cache counts again.
    u.billable_tokens = clamp_non_negative(u.input_tokens) + u.output_tokens;
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_token_count_event() {
        let body = "data: {\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":120,\"cached_input_tokens\":20,\"output_tokens\":50,\"reasoning_output_tokens\":10}}}\n\n";
        let u = parse_usage("codex", body);
        assert_eq!(u.input_tokens, 120);
        assert_eq!(u.cached_input_tokens, 20);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.reasoning_tokens, 10);
        assert_eq!(u.billable_tokens, 150); // 120 - 20 + 50
    }

    #[test]
    fn claude_message_events() {
        let body = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":200,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":10}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":80}}\n\n";
        let u = parse_usage("claude", body);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.cached_input_tokens, 40);
        assert_eq!(u.cache_creation_tokens, 10);
        assert_eq!(u.output_tokens, 80);
        // input_tokens already excludes cache read/creation, so billable = 200 + 80.
        assert_eq!(u.billable_tokens, 280);
    }

    #[test]
    fn codex_ws_event_usage() {
        // A single realtime frame (token_count) yields per-frame usage.
        let tc: Value = serde_json::from_str(
            "{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":300,\"cached_input_tokens\":250,\"output_tokens\":40,\"reasoning_output_tokens\":5}}}",
        )
        .unwrap();
        let u = parse_codex_event_usage(&tc).expect("token_count carries usage");
        assert_eq!(u.input_tokens, 300);
        assert_eq!(u.cached_input_tokens, 250);
        assert_eq!(u.output_tokens, 40);
        assert_eq!(u.billable_tokens, 90); // 300 - 250 + 40

        // response.completed carries the terminal usage too.
        let done: Value = serde_json::from_str(
            "{\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":300,\"input_tokens_details\":{\"cached_tokens\":250},\"output_tokens\":40}}}",
        )
        .unwrap();
        let u = parse_codex_event_usage(&done).expect("completed carries usage");
        assert_eq!(u.cached_input_tokens, 250);
        assert_eq!(u.billable_tokens, 90);

        // A plain delta frame has no usage.
        let delta: Value =
            serde_json::from_str("{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}").unwrap();
        assert!(parse_codex_event_usage(&delta).is_none());
    }

    #[test]
    fn claude_single_json() {
        let body = "{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}";
        let u = parse_usage("claude", body);
        assert_eq!(u.billable_tokens, 15);
    }

    #[test]
    fn response_model_anthropic_message_start() {
        // Real Anthropic shape: model lives at message_start.message.model.
        let body = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":10}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n";
        assert_eq!(
            parse_response_model(body).as_deref(),
            Some("claude-sonnet-4-5-20250929"),
        );
    }

    #[test]
    fn response_model_anthropic_single_json() {
        let body = "{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}";
        assert_eq!(
            parse_response_model(body).as_deref(),
            Some("claude-sonnet-4-5"),
        );
    }

    #[test]
    fn response_model_codex_response_completed() {
        // Codex: model lives under response.* on the terminal event.
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n";
        assert_eq!(parse_response_model(body).as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn response_model_openai_chunk() {
        // OpenAI-compat stream: model is at the chunk root.
        let body = "data: {\"id\":\"cmpl-1\",\"model\":\"glm-4.6\",\"choices\":[]}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_response_model(body).as_deref(), Some("glm-4.6"));
    }

    #[test]
    fn response_model_none_when_body_has_no_model() {
        // Error payloads carry no model.
        assert!(parse_response_model("{\"error\":\"boom\"}").is_none());
        // And of course garbage bytes.
        assert!(parse_response_model("not json").is_none());
    }

    #[test]
    fn glm_openai_usage_picks_up_reasoning_tokens() {
        // DeepSeek R1 / other OpenAI Chat Completions reasoning models
        // attach reasoning tokens under `completion_tokens_details`.
        // Before the fix this was hardcoded to 0 and the token count
        // silently dropped from the audit row.
        let body = serde_json::json!({
            "id": "cmpl-1",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 20},
                "completion_tokens_details": {"reasoning_tokens": 30},
            }
        });
        let u = parse_glm_json(&body);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cached_input_tokens, 20);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.reasoning_tokens, 30);
        // billable = input_uncached + output = 100 - 20 + 50 = 130
        assert_eq!(u.billable_tokens, 130);
    }
}

fn parse_claude_json(v: &Value) -> TokenUsage {
    let mut u = TokenUsage::default();
    if let Some(usage) = v.get("usage") {
        u.input_tokens = geti(usage, "input_tokens");
        u.cached_input_tokens = geti(usage, "cache_read_input_tokens");
        u.cache_creation_tokens = geti(usage, "cache_creation_input_tokens");
        u.output_tokens = geti(usage, "output_tokens");
    }
    // See `parse_claude_events`: Claude's input_tokens already excludes cache.
    u.billable_tokens = clamp_non_negative(u.input_tokens) + u.output_tokens;
    u
}
