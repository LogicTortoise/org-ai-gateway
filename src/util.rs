use crate::prelude::*;

/// Derive the gateway's externally-visible base URL (`scheme://host[:port]`,
/// no trailing slash) from the inbound request's own headers, instead of a
/// hardcoded host/port. `GATEWAY_BIND_ADDR` can be (and on some deployments
/// is) different from the well-known default, and behind a reverse proxy the
/// bind address isn't the public one either — the request the client itself
/// just sent already carries the address that worked, so honor that.
/// Honors `X-Forwarded-Proto`/`X-Forwarded-Host` so it produces the public
/// URL when behind a trusted edge/reverse proxy.
pub(crate) fn request_base_url(headers: &HeaderMap) -> String {
    let header_str = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let proto = header_str("x-forwarded-proto")
        .map(|p| p.split(',').next().unwrap_or("http").trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = header_str("x-forwarded-host")
        .or_else(|| header_str("host"))
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    format!("{}://{}", proto, host)
}

pub(crate) fn truncate_text(input: &str, max_chars: usize) -> String {
    let mut out: String = input.chars().take(max_chars).collect();
    if input.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}


/// Expand a leading `~`/`~/` to `$HOME`. Unix-style paths only — Windows
/// callers (e.g. the Cursor `state.vscdb` lookup) resolve `%APPDATA%`
/// themselves before getting here.
pub(crate) fn expand_home(input: &str) -> String {
    if input == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}/{}", home, rest);
        }
    }
    input.to_string()
}


pub(crate) async fn path_exists(path: &PathBuf) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// Resolve a CALLER-SUPPLIED config path, confined to the gateway operator's
/// home directory. The `connect/*/local` endpoints let a request name where to
/// read credentials from; without this an attacker could point `source_path` at
/// `/etc/passwd`, `/proc/self/environ`, etc. and probe/read arbitrary host
/// files. Absolute paths outside `$HOME` and any `..` traversal are rejected.
/// Operator-controlled DEFAULT paths bypass this (they are not attacker input).
pub(crate) fn resolve_confined_home_path(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let home = home.trim_end_matches('/').to_string();
    let candidate = if trimmed == "~" {
        home.clone()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        format!("{}/{}", home, rest)
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("{}/{}", home, trimmed)
    };
    if candidate.split('/').any(|seg| seg == "..") {
        return Err("path must not contain `..`".to_string());
    }
    let home_prefix = format!("{}/", home);
    if candidate != home && !candidate.starts_with(&home_prefix) {
        return Err("path must be inside the home directory".to_string());
    }
    Ok(candidate)
}

/// Lowercase hex encoding. Shared by checksum/hash helpers across the crate —
/// don't hand-roll per-byte `format!` loops at call sites.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Process-wide HTTP client: connection pooling (keep-alive, one TLS handshake
/// per host) plus a total request timeout so a hung upstream stream can't hold
/// a buffered gateway request forever. Override via GATEWAY_HTTP_TIMEOUT_SECS.
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("GATEWAY_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building shared http client")
    })
}

/// Dedicated HTTP client for the Codex/OpenAI upstream: same pooling and timeouts
/// as `http_client`, but with a pinned rustls TLS fingerprint (see
/// `fingerprint::rustls_tls`) and an optional outbound proxy via `CODEX_PROXY_URL`
/// (`http://user:pass@host:port`; HTTP CONNECT + Basic auth handled by reqwest).
/// This mirrors the Go hybrid transport that forged TLS for OpenAI hosts only —
/// Claude and other providers keep the standard `http_client`.
pub(crate) fn codex_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("GATEWAY_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .use_preconfigured_tls(crate::fingerprint::rustls_tls::codex_client_config());
        if let Some(proxy_url) = std::env::var("CODEX_PROXY_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            match reqwest::Proxy::all(&proxy_url) {
                Ok(p) => builder = builder.proxy(p),
                Err(e) => warn!("invalid CODEX_PROXY_URL {:?}, ignoring: {}", proxy_url, e),
            }
        }
        builder.build().expect("failed building codex http client")
    })
}

/// Default ceiling on a buffered upstream response body. The gateway inflates
/// gzip upstream bodies and holds the whole thing in memory, so without a cap a
/// malicious/compromised upstream could return a small gzip stream that expands
/// to gigabytes (decompression bomb) and OOM the process. Real AI responses are
/// a few MB at most; 256 MiB leaves enormous headroom while still stopping a
/// bomb. Override with `GATEWAY_MAX_RESPONSE_BYTES`.
pub(crate) fn max_response_bytes() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("GATEWAY_MAX_RESPONSE_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(256 * 1024 * 1024)
    })
}

/// Buffer an upstream response body, aborting if it exceeds `max_bytes`. Streams
/// chunk-by-chunk so an oversized (or bomb) body is rejected without first being
/// fully materialized in memory.
pub(crate) async fn read_body_capped(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<axum::body::Bytes, String> {
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("error reading upstream body: {}", e))?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!(
                "upstream response exceeded the {}-byte cap (possible decompression bomb)",
                max_bytes
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(axum::body::Bytes::from(buf))
}

// ---------------------------------------------------------------------------
// Upstream-error logging helpers.
//
// Provider accounts in the pool are shared, so an upstream error body may echo
// back auth material (API keys, Bearer tokens, JWTs). Before any byte of an
// upstream body lands in `data/gateway.out.log` we redact it. Detail strings
// that reach the client via `last_error` MUST stay parser-derived or use the
// generic `<provider> upstream returned <status>` fallback — never raw body.
// ---------------------------------------------------------------------------

/// Cap used by `redact_secrets` so a 10 MiB CDN-bounce HTML body isn't walked
/// byte-by-byte. 4 KiB is plenty to capture a meaningful error JSON (real
/// upstream errors are well under 2 KiB).
const REDACT_WORK_CAP: usize = 4 * 1024;

/// Final excerpt length for `format_upstream_error`. Matches the existing
/// 500-char convention used elsewhere when truncating upstream bodies
/// (`proxy.rs:1251`, `1693`).
const UPSTREAM_BODY_EXCERPT_CHARS: usize = 500;

/// Mask credential-shaped tokens in a body before it reaches the log.
///
/// Patterns handled:
///   - `sk-<20+ alnum/_/->`  (covers `sk-…`, `sk-ant-…`, `sk-proj-…`)
///   - `Bearer <20+>`        (case-insensitive keyword + adjacent token)
///   - JWT                   (3 dot-separated base64url segments, first two
///                            each ≥20 chars)
///   - JSON values keyed by `api_key`/`apikey`/`access_token`/`refresh_token`
///     /`authorization`/`token`/`secret` (case-insensitive key match)
///
/// Bodies are capped at `REDACT_WORK_CAP` bytes before scanning so an
/// upstream that returns a 10 MiB HTML page doesn't cost an O(n) walk.
pub(crate) fn redact_secrets(input: &str) -> String {
    let work: &str = if input.len() > REDACT_WORK_CAP {
        let mut cut = REDACT_WORK_CAP;
        while cut > 0 && !input.is_char_boundary(cut) {
            cut -= 1;
        }
        &input[..cut]
    } else {
        input
    };

    // Pass 1: tokenize on JSON / HTTP delimiters, redact per token.
    let mut out = String::with_capacity(work.len());
    let mut token = String::new();
    let mut pending_bearer = false;

    for ch in work.chars() {
        if is_token_delimiter(ch) {
            flush_token(&mut out, &mut token, &mut pending_bearer);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush_token(&mut out, &mut token, &mut pending_bearer);

    // Pass 2: if what remains is valid JSON, mask values whose key is in the
    // sensitive set. Non-JSON bodies pass through unchanged from pass 1.
    redact_json_sensitive_values(&out)
}

fn is_token_delimiter(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | '"' | ',' | '{' | '}' | '[' | ']' | '(' | ')' | '<' | '>' | ';' | ':' | '='
    )
}

/// Returns the replacement for `token` given whether the immediately
/// preceding emitted token was the literal `Bearer`. The replacement is empty
/// when the token should be emitted verbatim; the second return value tells
/// the caller whether `token` itself IS the bearer keyword (so the NEXT token
/// should be treated as the bearer payload).
fn classify_token(token: &str) -> (String, bool) {
    if let Some(rest) = token.strip_prefix("sk-") {
        if rest.len() >= 20 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return ("<redacted:sk>".to_string(), false);
        }
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() == 3
        && parts[0].len() >= 20
        && parts[1].len() >= 20
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(is_base64url))
    {
        return ("<redacted:jwt>".to_string(), false);
    }
    if token.eq_ignore_ascii_case("bearer") {
        return (String::new(), true);
    }
    (String::new(), false)
}

fn flush_token(out: &mut String, token: &mut String, pending_bearer: &mut bool) {
    if token.is_empty() {
        return;
    }
    let (replacement, is_bearer_kw) = classify_token(token);
    if *pending_bearer && token.len() >= 20 && !is_bearer_kw {
        out.push_str("<redacted:bearer>");
    } else if !replacement.is_empty() {
        out.push_str(&replacement);
    } else {
        out.push_str(token);
    }
    *pending_bearer = is_bearer_kw;
    token.clear();
}

fn is_base64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

const SENSITIVE_JSON_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "authorization",
    "token",
    "secret",
];

fn redact_json_sensitive_values(input: &str) -> String {
    let mut value: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return input.to_string(),
    };
    redact_json_recursive(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| input.to_string())
}

fn redact_json_recursive(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let kl = k.to_ascii_lowercase();
                if SENSITIVE_JSON_KEYS.contains(&kl.as_str()) {
                    *val = Value::String("<redacted>".to_string());
                } else {
                    redact_json_recursive(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_json_recursive(item);
            }
        }
        _ => {}
    }
}

/// View of an upstream non-2xx response, split into a client-safe summary and
/// a log-only body excerpt.
pub(crate) struct UpstreamError {
    /// Parser-derived message (or `<provider> upstream returned <status>`).
    /// Never contains raw body bytes — safe for `last_error` and the client.
    pub(crate) detail: String,
    /// Redacted + truncated body for `warn!` logging only.
    pub(crate) body_excerpt: String,
    /// Whether the caller-supplied parser produced a usable summary. Logged
    /// as a field for filtering; not used to gate emission.
    pub(crate) parser_hit: bool,
}

/// Build the log view of a failed upstream call. `parsed` is the caller's
/// parser output (parsers live in `proxy.rs` / `provider/*` so `util.rs`
/// stays dependency-free).
pub(crate) fn format_upstream_error(
    provider: &str,
    status: StatusCode,
    body: &str,
    parsed: Option<String>,
) -> UpstreamError {
    let (detail, parser_hit) = match parsed.filter(|s| !s.is_empty()) {
        Some(p) => (p, true),
        None => (
            format!("{} upstream returned {}", provider, status.as_u16()),
            false,
        ),
    };
    let body_excerpt = if body.trim().is_empty() {
        "<empty body>".to_string()
    } else {
        truncate_text(&redact_secrets(body), UPSTREAM_BODY_EXCERPT_CHARS)
    };
    UpstreamError {
        detail,
        body_excerpt,
        parser_hit,
    }
}

// ---------------------------------------------------------------------------
// Rate-limit value parsing helpers (shared by the x-codex-* header parser and
// the usage-endpoint parsers).
// ---------------------------------------------------------------------------

pub(crate) fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}


pub(crate) fn value_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}


/// Treat values that look like an absolute Unix epoch as a timestamp (convert
/// to delta-from-now, clamped at 0 — a PAST epoch must yield 0, never the raw
/// epoch itself, which would read as a ~50-year cooldown); anything smaller is
/// already a delta in seconds.
pub(crate) const EPOCH_THRESHOLD_SECS: i64 = 100_000_000; // ~3.17 years as a delta

pub(crate) fn epoch_to_after_seconds(ts: i64) -> i64 {
    if ts >= EPOCH_THRESHOLD_SECS {
        (ts - Utc::now().timestamp()).max(0)
    } else {
        ts.max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_in_the_past_clamps_to_zero() {
        // A stale reset_at epoch must never be returned verbatim (it would read
        // as a ~50-year cooldown).
        let past_epoch = Utc::now().timestamp() - 10;
        assert_eq!(epoch_to_after_seconds(past_epoch), 0);
    }

    #[test]
    fn epoch_in_the_future_becomes_delta() {
        let future_epoch = Utc::now().timestamp() + 300;
        let d = epoch_to_after_seconds(future_epoch);
        assert!((299..=301).contains(&d), "got {}", d);
    }

    #[test]
    fn small_values_are_treated_as_delta_seconds() {
        assert_eq!(epoch_to_after_seconds(45), 45);
        assert_eq!(epoch_to_after_seconds(0), 0);
        assert_eq!(epoch_to_after_seconds(-5), 0);
    }

    #[test]
    fn confined_path_rejects_traversal_and_escape() {
        std::env::set_var("HOME", "/home/tester");
        // Legit relative + tilde paths resolve under HOME.
        assert_eq!(
            resolve_confined_home_path("~/.codex/auth.json").unwrap(),
            "/home/tester/.codex/auth.json"
        );
        assert_eq!(
            resolve_confined_home_path(".codex/auth.json").unwrap(),
            "/home/tester/.codex/auth.json"
        );
        // Arbitrary host files and traversal are rejected.
        assert!(resolve_confined_home_path("/etc/passwd").is_err());
        assert!(resolve_confined_home_path("/proc/self/environ").is_err());
        assert!(resolve_confined_home_path("~/../../etc/passwd").is_err());
        assert!(resolve_confined_home_path("").is_err());
    }

    #[test]
    fn redact_secrets_masks_openai_sk_keys() {
        let input = "leaked key sk-abcdef1234567890abcdef12 in body";
        let out = redact_secrets(input);
        assert!(out.contains("<redacted:sk>"), "got: {}", out);
        assert!(!out.contains("abcdef1234567890abcdef12"), "got: {}", out);
    }

    #[test]
    fn redact_secrets_masks_anthropic_sk_ant_keys() {
        // sk-ant-… is just another sk- prefix variant.
        let input = "token sk-ant-api03-abcdefghijklmnopqrstuv here";
        let out = redact_secrets(input);
        assert!(out.contains("<redacted:sk>"), "got: {}", out);
        assert!(!out.contains("abcdefghijklmnopqrstuv"), "got: {}", out);
    }

    #[test]
    fn redact_secrets_masks_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\
                   .eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0\
                   .SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_secrets(jwt);
        assert!(out.contains("<redacted:jwt>"), "got: {}", out);
        assert!(
            !out.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "header leaked: {}",
            out
        );
    }

    #[test]
    fn redact_secrets_masks_bearer_header() {
        let input = "Authorization: Bearer abcdef1234567890abcdef";
        let out = redact_secrets(input);
        assert!(out.contains("Bearer <redacted:bearer>"), "got: {}", out);
        assert!(!out.contains("abcdef1234567890abcdef"), "got: {}", out);
    }

    #[test]
    fn redact_secrets_masks_json_value_with_sensitive_key() {
        let input = r#"{"api_key":"sk-abcdef1234567890abcdef12","error":"bad"}"#;
        let out = redact_secrets(input);
        assert!(out.contains(r#""api_key":"<redacted>""#), "got: {}", out);
        assert!(!out.contains("abcdef1234567890abcdef12"), "got: {}", out);
        // Non-sensitive fields pass through.
        assert!(out.contains(r#""error":"bad""#), "got: {}", out);
    }

    #[test]
    fn redact_secrets_ignores_short_or_safe_values() {
        // Too short to be a real key — leave alone.
        assert_eq!(redact_secrets("sk-short"), "sk-short");
        // Plain numbers — leave alone.
        assert_eq!(redact_secrets("12345678"), "12345678");
        // Prose with dots — three "segments" but not JWT-shaped (too short).
        assert_eq!(redact_secrets("hello.world.foo"), "hello.world.foo");
        // Empty input — empty output.
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn redact_secrets_caps_huge_inputs() {
        // 1 MiB of garbage — must not OOM, must not blow past work cap.
        let big = "x".repeat(1024 * 1024);
        let out = redact_secrets(&big);
        // Output is the pass-1 + pass-2 view of the capped slice; in either
        // case the size stays bounded by REDACT_WORK_CAP plus a few bytes for
        // any JSON re-emission overhead.
        assert!(
            out.len() <= REDACT_WORK_CAP * 2,
            "output too large: {}",
            out.len()
        );
    }

    #[test]
    fn format_upstream_error_uses_parser_when_available() {
        let up = format_upstream_error(
            "minimax",
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"boom"}"#,
            Some("rate limit hit".to_string()),
        );
        assert_eq!(up.detail, "rate limit hit");
        assert!(up.parser_hit);
        assert!(up.body_excerpt.contains("boom"), "got: {}", up.body_excerpt);
    }

    #[test]
    fn format_upstream_error_falls_back_when_parser_misses() {
        let up = format_upstream_error(
            "minimax",
            StatusCode::INTERNAL_SERVER_ERROR,
            "<html>oops</html>",
            None,
        );
        assert_eq!(up.detail, "minimax upstream returned 500");
        assert!(!up.parser_hit);
        assert!(up.body_excerpt.contains("oops"), "got: {}", up.body_excerpt);
    }

    #[test]
    fn format_upstream_error_handles_empty_body() {
        let up = format_upstream_error(
            "minimax",
            StatusCode::BAD_GATEWAY,
            "",
            None,
        );
        assert_eq!(up.body_excerpt, "<empty body>");
        assert_eq!(up.detail, "minimax upstream returned 502");
    }

    #[test]
    fn format_upstream_error_truncates_long_body() {
        let long = "x".repeat(2000);
        let up = format_upstream_error(
            "minimax",
            StatusCode::INTERNAL_SERVER_ERROR,
            &long,
            None,
        );
        assert!(up.body_excerpt.ends_with("..."), "got tail: {:?}", up.body_excerpt);
        // 500 chars + "..." marker.
        assert!(up.body_excerpt.chars().count() <= 503, "got: {}", up.body_excerpt.chars().count());
    }

    #[test]
    fn format_upstream_error_redacts_secrets_in_body() {
        let up = format_upstream_error(
            "minimax",
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"bad","api_key":"sk-abcdef1234567890abcdef12"}"#,
            None,
        );
        // Client-facing detail must never contain the raw key.
        assert!(!up.detail.contains("sk-abcdef"), "got: {}", up.detail);
        // Log-only excerpt must mask it.
        assert!(
            !up.body_excerpt.contains("abcdef1234567890abcdef12"),
            "got: {}",
            up.body_excerpt
        );
        assert!(up.body_excerpt.contains("<redacted>"), "got: {}", up.body_excerpt);
    }
}


