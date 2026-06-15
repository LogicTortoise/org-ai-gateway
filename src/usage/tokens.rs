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
pub(crate) fn parse_usage(provider: &str, body: &str) -> TokenUsage {
    let events = sse_json_events(body);
    if !events.is_empty() {
        return if provider == "claude" {
            parse_claude_events(&events)
        } else {
            parse_codex_events(&events)
        };
    }
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return if provider == "claude" {
            parse_claude_json(&v)
        } else {
            parse_codex_json(&v)
        };
    }
    TokenUsage::default()
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
