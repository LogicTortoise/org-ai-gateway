//! Ollama provider: a LOCAL model server speaking the Ollama HTTP API
//! (`/api/chat`, `/api/tags`). It is the odd one out among the providers — there
//! is no OAuth, no token refresh, no rate-limit windows, and no per-account
//! capacity model. An "account" is simply a base URL pointing at a running
//! ollama (default `http://127.0.0.1:11434`); it is free, always-on, and treated
//! by the pool as a non-metered upstream.
//!
//! Two roles (both wired up in `routes::proxy`):
//!   1. Directly selectable: a client request with `model=ollama/<name>` (or a
//!      bare `ollama`) routes straight here, peer to Codex/Claude/Cursor.
//!   2. Whole-pool fallback: when the originally-routed paid provider has no
//!      usable account left (exhausted / rate-limited / errored), the proxy
//!      falls back to a local ollama instead of returning a hard failure.
//!
//! Token counts ARE real here (ollama returns `prompt_eval_count` /
//! `eval_count`), so audited usage is exact rather than estimated — unlike
//! Cursor, whose protocol carries no usage and synthesizes char-based estimates.
//!
//! The inbound request normalization (OpenAI / Anthropic / Responses ->
//! messages) and the response rendering (back into the client's format) are the
//! format-agnostic adapters that already live in `provider::cursor`
//! (`extract_request`, `build_buffered_body`, `build_sse_body`). They are reused
//! verbatim here; the `Cursor*` names are historical — the logic is provider-neutral.
use crate::prelude::*;
use crate::provider::cursor::{ChatTurn, ExtractedRequest};
use crate::util::truncate_text;

/// Default local ollama endpoint when an account stores no explicit base URL
/// and `OLLAMA_BASE_URL` is unset.
pub(crate) const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// Default model when a request selects the bare `ollama` slug (override with
/// `OLLAMA_DEFAULT_MODEL`).
const FALLBACK_DEFAULT_MODEL: &str = "llama3";

/// Dedicated HTTP client for ollama. Local-first: a short connect timeout so an
/// absent/unreachable ollama fails fast (important on the fallback path, where a
/// hung connect would stall a request that should degrade quickly), but a
/// generous total timeout because local generation on CPU can be slow.
pub(crate) fn ollama_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building ollama http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the ollama upstream. Models use the
/// `ollama/<upstream-model>` form (the prefix is stripped before the upstream
/// call); a bare `ollama` selects the configured default model.
pub(crate) fn is_ollama_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "ollama" || m.starts_with("ollama/")
}

/// Maps a gateway model name to the upstream ollama model tag. `ollama/llama3`
/// -> `llama3`; a bare `ollama` -> the configured default model.
pub(crate) fn ollama_canonical_model(model: &str) -> String {
    let m = model.trim();
    if m.eq_ignore_ascii_case("ollama") {
        return std::env::var("OLLAMA_DEFAULT_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| FALLBACK_DEFAULT_MODEL.to_string());
    }
    if let Some(rest) = m
        .strip_prefix("ollama/")
        .or_else(|| m.strip_prefix("Ollama/"))
    {
        return rest.to_string();
    }
    m.to_string()
}

/// The base URL for an ollama account: its stored `base_url`, else the
/// `OLLAMA_BASE_URL` env, else the local default. The trailing slash is trimmed
/// so callers can append `/api/...` paths uniformly.
pub(crate) fn ollama_base_url(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("OLLAMA_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// Upstream call
// ---------------------------------------------------------------------------

/// Outcome of an ollama upstream call.
pub(crate) struct OllamaResult {
    pub(crate) text: String,
    pub(crate) status: reqwest::StatusCode,
    /// Error detail from an HTTP error or a JSON error body, if any.
    pub(crate) error: Option<String>,
    /// Real token usage parsed from the response (`prompt_eval_count` /
    /// `eval_count`); zero when the upstream omitted them.
    pub(crate) usage: TokenUsage,
}

/// Build the `messages` array ollama's `/api/chat` expects from the normalized
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

/// Send one chat request to an ollama upstream and return the assistant text
/// plus real token usage. Mirrors `send_cursor_upstream`: build the body, POST,
/// surface status + body. Always non-streaming (`stream:false`) — the gateway
/// buffers the whole reply and re-renders it in the client's format.
pub(crate) async fn send_ollama_upstream(
    client: &reqwest::Client,
    account: &UpstreamAccount,
    model: &str,
    req: &ExtractedRequest,
) -> Result<OllamaResult, String> {
    let base = ollama_base_url(account);
    let url = format!("{}/api/chat", base);
    let body = json!({
        "model": model,
        "messages": build_messages(req),
        "stream": false,
    });

    let mut request = client.post(&url).json(&body);
    // Optional bearer for an ollama placed behind an auth proxy: stored as the
    // account's access_token / api_key (local ollama needs none).
    let bearer = account.bearer();
    if !bearer.is_empty() {
        request = request.bearer_auth(bearer);
    }

    let resp = request
        .send()
        .await
        .map_err(|e| format!("ollama upstream request failed ({}): {}", url, e))?;
    let status = resp.status();
    let text_body = resp
        .text()
        .await
        .map_err(|e| format!("reading ollama upstream body failed: {}", e))?;

    if !status.is_success() {
        let detail = parse_error_message(&text_body)
            .unwrap_or_else(|| format!("ollama upstream returned {}", status));
        return Ok(OllamaResult {
            text: String::new(),
            status,
            error: Some(detail),
            usage: TokenUsage::default(),
        });
    }

    let value: Value = serde_json::from_str(&text_body)
        .map_err(|e| format!("invalid ollama response JSON: {}", e))?;
    // ollama may return a 200 with an `{"error": "..."}` body (e.g. model not pulled).
    if let Some(err) = parse_error_message(&text_body) {
        if value.get("message").is_none() {
            return Ok(OllamaResult {
                text: String::new(),
                status,
                error: Some(err),
                usage: TokenUsage::default(),
            });
        }
    }

    let content = value
        .pointer("/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(OllamaResult {
        text: content,
        status,
        error: None,
        usage: parse_usage(&value),
    })
}

/// Pull a human-readable error out of an ollama error body (`{"error":"..."}`).
fn parse_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

/// Parse real token usage from an ollama `/api/chat` response. `prompt_eval_count`
/// is the input tokens, `eval_count` the generated tokens; ollama has no prompt
/// cache, so billable = input + output (consistent with `usage::tokens`).
fn parse_usage(value: &Value) -> TokenUsage {
    let geti = |key: &str| value.get(key).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let input = geti("prompt_eval_count");
    let output = geti("eval_count");
    TokenUsage {
        input_tokens: input,
        cached_input_tokens: 0,
        cache_creation_tokens: 0,
        output_tokens: output,
        reasoning_tokens: 0,
        billable_tokens: input + output,
    }
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// Fetch an ollama account's installed models via `GET /api/tags`. Each model
/// `m` is exposed to clients as `ollama/<m>` (the prefix is stripped before the
/// upstream call), plus a bare `ollama` default entry first.
pub(crate) async fn fetch_ollama_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    let base = ollama_base_url(account);
    let url = format!("{}/api/tags", base);
    let client = ollama_http_client();
    let mut request = client.get(&url);
    let bearer = account.bearer();
    if !bearer.is_empty() {
        request = request.bearer_auth(bearer);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("failed to reach ollama at {}: {}", url, e))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed reading ollama models response: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "ollama models api error {}: {}",
            status.as_u16(),
            truncate_text(&body, 300)
        ));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid ollama models response: {}", e))?;
    let arr = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "ollama models response missing `models` field".to_string())?;

    let mut out = vec![ModelInfo {
        slug: "ollama".to_string(),
        display_name: "ollama (default)".to_string(),
    }];
    for item in arr {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        out.push(ModelInfo {
            slug: format!("ollama/{}", name),
            display_name: name.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_with(base: &str) -> UpstreamAccount {
        UpstreamAccount {
            id: "o1".into(),
            owner_user_id: "alice".into(),
            provider: "ollama".into(),
            account_label: "local".into(),
            access_token: String::new(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            api_key: String::new(),
            base_url: base.into(),
            base_url_alt: String::new(),
            share_enabled: true,
            share_limit_percent: None,
            daily_token_limit: None,
            created_at: Utc::now(),
            runtime: AccountRuntime::default(),
        }
    }

    #[test]
    fn model_detection_and_canonicalization() {
        assert!(is_ollama_model("ollama"));
        assert!(is_ollama_model("ollama/qwen3"));
        assert!(is_ollama_model("Ollama/llama3.1:8b"));
        assert!(!is_ollama_model("gpt-5"));
        assert!(!is_ollama_model("claude-sonnet-4"));
        assert_eq!(ollama_canonical_model("ollama/qwen3"), "qwen3");
        assert_eq!(ollama_canonical_model("ollama/llama3.1:8b"), "llama3.1:8b");
    }

    #[test]
    fn base_url_prefers_account_then_default() {
        std::env::remove_var("OLLAMA_BASE_URL");
        assert_eq!(ollama_base_url(&account_with("")), DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(
            ollama_base_url(&account_with("http://10.0.0.2:11434/")),
            "http://10.0.0.2:11434"
        );
    }

    #[test]
    fn parses_real_token_usage() {
        let v = json!({
            "message": { "role": "assistant", "content": "hi" },
            "prompt_eval_count": 26,
            "eval_count": 290,
            "done": true
        });
        let u = parse_usage(&v);
        assert_eq!(u.input_tokens, 26);
        assert_eq!(u.output_tokens, 290);
        assert_eq!(u.billable_tokens, 316);
    }

    #[test]
    fn build_messages_prepends_system() {
        let req = ExtractedRequest {
            instruction: "Be terse.".into(),
            turns: vec![
                ChatTurn { role: 1, content: "Hi".into() },
                ChatTurn { role: 2, content: "Hello".into() },
                ChatTurn { role: 1, content: "Bye".into() },
            ],
            stream: false,
        };
        let msgs = build_messages(&req);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "Bye");
    }
}
