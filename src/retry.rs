//! Error classification + retry/backoff/cooldown machinery, ported from the Go
//! `error_class.go` and the `proxyRequest` retry loop. The gateway buffers each
//! upstream response fully before deciding, so unlike the Go streaming path we
//! can always retry on another account as long as attempts remain.

use crate::prelude::*;
use crate::pool::account_visible_to_user;
use crate::pool::storage::persist_all_accounts;

/// Coarse classification of an upstream HTTP outcome, deciding retry/cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorClass {
    /// 2xx/3xx — success, stop.
    None,
    /// 408/5xx/524/529 — transient, retry on another account.
    Transient,
    /// 429 — rate limited, cooldown + retry.
    RateLimit,
    /// 401/403 — auth, refresh/swap.
    Auth,
    /// 402 — payment/subscription.
    Payment,
    /// 404 / model unavailable.
    NotFound,
    /// 400 — bad request, return to client (no retry).
    Invalid,
    /// anything else — fatal, return to client.
    Fatal,
}

impl ErrorClass {
    /// Mirror of Go `classifyStatus`.
    pub(crate) fn from_status(status: u16) -> ErrorClass {
        match status {
            200..=399 => ErrorClass::None,
            400 => ErrorClass::Invalid,
            401 | 403 => ErrorClass::Auth,
            402 => ErrorClass::Payment,
            404 => ErrorClass::NotFound,
            408 => ErrorClass::Transient,
            429 => ErrorClass::RateLimit,
            500..=599 => ErrorClass::Transient,
            _ => ErrorClass::Fatal,
        }
    }

    pub(crate) fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorClass::Transient
                | ErrorClass::RateLimit
                | ErrorClass::Auth
                | ErrorClass::Payment
                | ErrorClass::NotFound
        )
    }

    /// Penalty added to the account on this failure class.
    pub(crate) fn penalty(self) -> f64 {
        match self {
            ErrorClass::RateLimit => 0.2,
            ErrorClass::Transient => 0.3,
            ErrorClass::NotFound => 0.1,
            ErrorClass::Auth => 10.0,
            ErrorClass::Payment => 50.0,
            _ => 0.0,
        }
    }
}

/// 429 exponential backoff: min(1s * 2^level, 30m), no jitter (matches Go).
pub(crate) fn backoff_duration(level: u32) -> chrono::Duration {
    let capped = level.min(20);
    let secs = (1i64 << capped).min(30 * 60);
    chrono::Duration::seconds(secs)
}

// ---- body / header detectors (ported from error_class.go) ----

pub(crate) fn is_codex_model_unavailable(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    const NEEDLES: [&str; 7] = [
        "model_not_found",
        "model not found",
        "model_not_supported",
        "model not supported",
        "model is not supported",
        "not available for",
        "does not have access to model",
    ];
    NEEDLES.iter().any(|n| b.contains(n))
}

/// Whether an error body indicates a permanently-dead workspace/subscription.
/// Deliberately broad substring matching, but only consulted on HTTP 402
/// (`class == Payment` — see `proxy_provider`), so a chat completion that
/// merely *mentions* "billing" can never kill an account; a real 402 whose body
/// matches marks it dead rather than letting it burn retries forever.
pub(crate) fn is_deactivated_workspace(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    const NEEDLES: [&str; 4] = [
        "deactivated_workspace",
        "subscription",
        "billing",
        "payment_required",
    ];
    NEEDLES.iter().any(|n| b.contains(n))
}

pub(crate) fn is_claude_organization_disabled(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    const NEEDLES: [&str; 5] = [
        "organization has been disabled",
        "organization is disabled",
        "organization_has_been_disabled",
        "organization_disabled",
        "oauth authentication is currently not allowed",
    ];
    NEEDLES.iter().any(|n| b.contains(n))
}

/// Cloudflare bot-challenge: header `cf-mitigated: challenge`, or
/// `server: cloudflare` plus an HTML body. Treated as transient, no penalty bump
/// beyond transient.
pub(crate) fn is_cloudflare_challenge(body: &str, cf_mitigated: Option<&str>, server: Option<&str>) -> bool {
    if cf_mitigated
        .map(|v| v.to_ascii_lowercase().contains("challenge"))
        .unwrap_or(false)
    {
        return true;
    }
    let is_cf = server.map(|v| v.to_ascii_lowercase().contains("cloudflare")).unwrap_or(false);
    is_cf && body.to_ascii_lowercase().contains("<html")
}

/// cyber_policy: only matches the real JSON structure (code/type == "cyber_policy")
/// at the top level or under `error`/`response`, to avoid false positives on
/// echoed text. The body may be a single JSON document OR a buffered SSE stream;
/// for SSE the check runs per `data:` event (Go scans events the same way via
/// `cyberPolicyHTTPSuppressor.onEvent`).
pub(crate) fn is_cyber_policy_error(body: &str) -> bool {
    if !body.contains("cyber_policy") {
        return false;
    }
    if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
        return json_is_cyber_policy(&v);
    }
    // Not a single JSON document: scan SSE `data:` events.
    for line in body.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" || !data.contains("cyber_policy") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if json_is_cyber_policy(&v) {
                return true;
            }
        }
    }
    false
}

fn json_is_cyber_policy(v: &Value) -> bool {
    let matches = |node: &Value| -> bool {
        node.get("code").and_then(|c| c.as_str()) == Some("cyber_policy")
            || node.get("type").and_then(|t| t.as_str()) == Some("cyber_policy")
    };
    if matches(v) {
        return true;
    }
    if v.get("error").map(matches).unwrap_or(false) {
        return true;
    }
    // SSE events like response.failed carry the error under `response.error`.
    v.pointer("/response/error").map(matches).unwrap_or(false)
}

#[cfg(test)]
mod cyber_tests {
    use super::*;

    #[test]
    fn detects_cyber_policy_in_plain_json() {
        assert!(is_cyber_policy_error(r#"{"error":{"code":"cyber_policy","message":"x"}}"#));
        assert!(is_cyber_policy_error(r#"{"type":"cyber_policy"}"#));
    }

    #[test]
    fn detects_cyber_policy_inside_sse_stream() {
        let body = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"cyber_policy\"}}}\n\ndata: [DONE]\n\n";
        assert!(is_cyber_policy_error(body));
        let direct = "data: {\"type\":\"error\",\"error\":{\"code\":\"cyber_policy\"}}\n\n";
        assert!(is_cyber_policy_error(direct));
    }

    #[test]
    fn ignores_echoed_text_mentioning_cyber_policy() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"the cyber_policy string in assistant text\"}\n\n";
        assert!(!is_cyber_policy_error(body));
        assert!(!is_cyber_policy_error("plain text cyber_policy mention"));
    }

    #[test]
    fn parse_retry_after_handles_both_forms() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("120"));
        assert_eq!(parse_retry_after(&h), Some(120));

        // HTTP-date in the past -> clamped to 0 (not negative, not ignored).
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&h), Some(0));

        // Unparseable -> None (falls back to exponential backoff).
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn prefer_near_expiry_keeps_soonest_else_all() {
        let now = Utc::now();
        let mk = |id: &str, until: Option<DateTime<Utc>>| {
            let mut runtime = AccountRuntime::default();
            runtime.rate_limit_until = until;
            UpstreamAccount {
                id: id.to_string(),
                owner_user_id: "koltyu".to_string(),
                provider: "claude".to_string(),
                account_label: id.to_string(),
                access_token: "tok".to_string(),
                refresh_token: String::new(),
                id_token: String::new(),
                account_id: String::new(),
                api_key: String::new(),
                base_url: String::new(),
                base_url_alt: String::new(),
                share_enabled: true,
                share_limit_percent: None,
                daily_token_limit: None,
                created_at: now,
                runtime,
            }
        };
        let near = mk("near", Some(now + chrono::Duration::seconds(5)));
        let far = mk("far", Some(now + chrono::Duration::seconds(600)));
        let kept = prefer_near_expiry(vec![near.clone(), far.clone()], now);
        assert_eq!(kept.iter().map(|a| a.id.clone()).collect::<Vec<_>>(), vec!["near"]);
        // If none are near, keep all rather than fail.
        let kept = prefer_near_expiry(vec![far.clone()], now);
        assert_eq!(kept.len(), 1);
    }
}

/// Parse a `Retry-After` header into whole seconds-from-now. Supports BOTH wire
/// forms: integer delta-seconds, and the HTTP-date (IMF-fixdate) form that
/// Anthropic/Cloudflare frequently send — without the latter the gateway would
/// fall back to a too-short exponential backoff and hammer the account sooner
/// than the upstream asked.
pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Option<i64> {
    let raw = headers.get("retry-after").and_then(|v| v.to_str().ok())?.trim().to_string();
    if let Ok(secs) = raw.parse::<i64>() {
        return (secs >= 0).then_some(secs);
    }
    // HTTP-date form, e.g. "Wed, 21 Oct 2025 07:28:00 GMT".
    let naive = chrono::NaiveDateTime::parse_from_str(&raw, "%a, %d %b %Y %H:%M:%S GMT").ok()?;
    let target = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    Some((target - Utc::now()).num_seconds().max(0))
}

/// A cooling-down account is only "near expiry" — and thus safe to fall back to
/// without re-hitting a deeply rate-limited upstream — when its remaining
/// cooldown is within this window.
pub(crate) const COOLING_NEAR_EXPIRY_SECS: i64 = 30;

/// From a set of cooling-down accounts, keep only those whose cooldown is nearly
/// over. If none qualify, return the input unchanged so the caller still tries
/// (better than failing) — but in the common case this stops every client
/// request from sweeping the whole rate-limited pool and amplifying the limit.
pub(crate) fn prefer_near_expiry(
    cooling: Vec<UpstreamAccount>,
    now: DateTime<Utc>,
) -> Vec<UpstreamAccount> {
    let near: Vec<UpstreamAccount> = cooling
        .iter()
        .filter(|a| {
            a.runtime
                .rate_limit_until
                .map(|until| (until - now).num_seconds() <= COOLING_NEAR_EXPIRY_SECS)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if near.is_empty() {
        cooling
    } else {
        near
    }
}

/// Compute the cooldown deadline for a rate-limited account, preferring an
/// explicit reset hint, then Retry-After, then exponential backoff.
pub(crate) fn cooldown_until(
    now: DateTime<Utc>,
    reset_after_secs: Option<i64>,
    retry_after_secs: Option<i64>,
    backoff_level: u32,
) -> DateTime<Utc> {
    let secs = reset_after_secs
        .filter(|s| *s > 0)
        .or(retry_after_secs.filter(|s| *s > 0))
        .map(chrono::Duration::seconds)
        .unwrap_or_else(|| backoff_duration(backoff_level));
    now + secs
}

/// Apply a failure outcome to an account's runtime state under the write lock.
/// Returns nothing; the caller decides whether to retry based on
/// `class.is_retryable()`.
pub(crate) async fn apply_account_failure(
    state: &AppState,
    account_id: &str,
    class: ErrorClass,
    snapshot: Option<&RateLimitSnapshot>,
    retry_after_secs: Option<i64>,
    dead: bool,
) {
    let now = Utc::now();
    let newly_dead = {
        let mut accounts = state.accounts.write().await;
        let Some(a) = accounts.iter_mut().find(|a| a.id == account_id) else {
            return;
        };
        let newly_dead = dead && !a.runtime.dead;
        let added = class.penalty();
        a.runtime.penalty += added;
        if dead {
            a.runtime.dead = true;
            a.runtime.penalty += 100.0;
        }
        // Anchor decay to when the penalty was added; otherwise a fresh account
        // (last_penalty == None) gets decayed on the very next 60s tick instead
        // of after the documented 5-minute window.
        if added > 0.0 || dead {
            a.runtime.last_penalty = Some(now);
        }
        if class == ErrorClass::RateLimit {
            let reset = snapshot.and_then(|s| s.primary_reset_after_seconds);
            let until = cooldown_until(now, reset, retry_after_secs, a.runtime.backoff_level);
            if a.runtime.rate_limit_until.map(|cur| until > cur).unwrap_or(true) {
                a.runtime.rate_limit_until = Some(until);
            }
            a.runtime.backoff_level = a.runtime.backoff_level.saturating_add(1);
        }
        newly_dead
    };
    // Of everything mutated above only `dead` is persisted (penalty/cooldown/
    // backoff are runtime-only, serde-skipped) — so only a dead-flag flip needs
    // a disk write. Persisting unconditionally meant a retry storm rewrote the
    // whole credential store once per failed attempt for no durable change.
    if newly_dead {
        if let Err(e) = persist_all_accounts(state).await {
            warn!("failed persisting account after failure: {}", e);
        }
    }
}

/// Reset backoff on success. In-memory only by design: `backoff_level` is
/// serde-skipped (runtime-only), so there is nothing to persist — symmetric
/// with `apply_account_failure`, which also only persists the `dead` flag.
pub(crate) async fn reset_backoff(state: &AppState, account_id: &str) {
    let mut accounts = state.accounts.write().await;
    if let Some(a) = accounts.iter_mut().find(|a| a.id == account_id) {
        if a.runtime.backoff_level != 0 {
            a.runtime.backoff_level = 0;
        }
    }
}

/// Claude-specific: if the live snapshot shows 5h primary usage >= 100% and we
/// know the reset time, cool the account down until that reset (Go `syncUsageCooldown`).
pub(crate) async fn sync_usage_cooldown(state: &AppState, account_id: &str, snapshot: &RateLimitSnapshot) {
    let primary = snapshot.primary_used_percent.unwrap_or(0.0);
    if primary < 100.0 {
        return;
    }
    let Some(reset_secs) = snapshot.primary_reset_after_seconds.filter(|s| *s > 0) else {
        return;
    };
    let until = Utc::now() + chrono::Duration::seconds(reset_secs);
    let mut accounts = state.accounts.write().await;
    if let Some(a) = accounts.iter_mut().find(|a| a.id == account_id) {
        if a.runtime.rate_limit_until.map(|cur| until > cur).unwrap_or(true) {
            a.runtime.rate_limit_until = Some(until);
        }
    }
}

/// Periodic penalty decay (*0.8 every >=5 min; floor to 0 below 0.01) and
/// expired-cooldown clearing. Spawned as a background task.
pub(crate) async fn run_penalty_decay(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        let now = Utc::now();
        // Prune stale per-user RPM buckets. The keys are request-header-derived
        // (`user_id`), so without this the map grows without bound as callers
        // rotate identities. Anything older than the current minute is dead.
        {
            let minute = now.timestamp().div_euclid(60);
            let mut rates = state.user_request_rate.write().await;
            rates.retain(|_, r| r.minute >= minute - 1);
        }
        let mut accounts = state.accounts.write().await;
        for a in accounts.iter_mut() {
            let due = a
                .runtime
                .last_penalty
                .map(|t| now - t >= chrono::Duration::minutes(5))
                .unwrap_or(true);
            if due && a.runtime.penalty > 0.0 {
                a.runtime.penalty *= 0.8;
                if a.runtime.penalty < 0.01 {
                    a.runtime.penalty = 0.0;
                }
                a.runtime.last_penalty = Some(now);
            }
            if let Some(until) = a.runtime.rate_limit_until {
                if until <= now {
                    a.runtime.rate_limit_until = None;
                }
            }
            // Selection-spread pressure fades fast: it only needs to outlive
            // the staleness of one rate-limit snapshot (~3 min).
            if a.runtime.recent_picks > 0.0 {
                a.runtime.recent_picks *= 0.5;
                if a.runtime.recent_picks < 0.05 {
                    a.runtime.recent_picks = 0.0;
                }
            }
        }
    }
}

/// Build the set of accounts eligible for the given provider/user, excluding
/// dead/disabled and already-tried accounts. When `include_cooling` is false,
/// accounts still inside their `rate_limit_until` window are also excluded.
pub(crate) fn eligible_accounts(
    accounts: &[UpstreamAccount],
    provider: &str,
    user_id: &str,
    excluded: &std::collections::HashSet<String>,
    now: DateTime<Utc>,
    include_cooling: bool,
) -> Vec<UpstreamAccount> {
    accounts
        .iter()
        .filter(|a| a.provider == provider)
        .filter(|a| account_visible_to_user(a, user_id))
        .filter(|a| !a.runtime.dead && !a.runtime.disabled)
        .filter(|a| !excluded.contains(&a.id))
        .filter(|a| {
            include_cooling
                || a.runtime
                    .rate_limit_until
                    .map(|until| until <= now)
                    .unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// How many upstream attempts to make for a provider: at least the number of
/// usable accounts (so we can sweep the whole pool), with a floor of 2.
pub(crate) async fn provider_attempt_budget(state: &AppState, provider: &str) -> usize {
    let accounts = state.accounts.read().await;
    let count = accounts
        .iter()
        .filter(|a| a.provider == provider && !a.runtime.dead && !a.runtime.disabled)
        .count();
    count.max(2)
}
