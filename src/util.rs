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
}


