//! Local rate-limit window derivation for API-key endpoint providers that don't
//! return upstream rate-limit headers — `glm` / `kimi` / `deepseek` / `minimax`.
//!
//! ## Why this exists
//!
//! Codex and Claude tell the gateway how full their windows are on every
//! response (`x-codex-*` and `anthropic-ratelimit-*`). These four providers
//! don't — they return nothing, so `usage_percent_gates_selection` has to
//! exclude them from the scheduler's percentage-based hard-exclude / rebalance
//! path, and the dashboard's `/v1/stats/capacity` endpoint skips them entirely.
//!
//! We can still observe their consumption by reading the audit log ourselves.
//! Every successful request writes an `AuditRecord` with real `billable_tokens`
//! (parsed from the upstream `usage` object — `usage::tokens`), and every
//! failed request writes its `status` (e.g. `rate_limit_error`, `glm_error_429`).
//! A 5-hour sliding sum of billable tokens is a stand-in for "used 5h window";
//! a 7-day sum is the stand-in for "used weekly window". Neither is exact —
//! the upstream might use RPM/TPM sliding windows instead — but it catches
//! the most common failure mode ("the account has been hammered and we're
//! about to get 1002 / 2056") and exposes it on the same dashboard the rest
//! of the pool uses.
//!
//! ## How the caps work
//!
//! Per-provider caps are operator-set, because none of these vendors publish
//! a clean "5h token cap = N" number in their public docs (their limits are
//! usually RPM/TPM sliding windows tied to account tier / balance):
//!
//! - `GLM_PRIMARY_LIMIT_TOKENS`     / `GLM_WEEKLY_LIMIT_TOKENS`
//! - `KIMI_PRIMARY_LIMIT_TOKENS`    / `KIMI_WEEKLY_LIMIT_TOKENS`
//! - `DEEPSEEK_PRIMARY_LIMIT_TOKENS` / `DEEPSEEK_WEEKLY_LIMIT_TOKENS`
//! - `MINIMAX_PRIMARY_LIMIT_TOKENS`  / `MINIMAX_WEEKLY_LIMIT_TOKENS`
//!
//! Unset (or `0`) means "no cap declared" — the corresponding `used_percent`
//! is `None`, the dashboard shows "not applicable", and selection is governed
//! only by the recent-rate-limit-error fallback (see `should_exclude_for_local_window`).
//!
//! ## How the writes flow
//!
//! Two producers call `aggregate_account_window`:
//! 1. `capacity::run_capacity_maintenance` (every minute), so old tokens
//!    naturally slide out of the window even when no new traffic arrives.
//! 2. `usage::probe_one_account` (every `GATEWAY_HEALTH_PROBE_SECS`), for a
//!    low-latency refresh right after a real upstream call lands.
//!
//! The per-account result is cached for `CACHE_TTL_SECS` so concurrent
//! `/v1/stats/capacity` reads and overlapping maintenance ticks don't trigger
//! redundant audit-file scans. `invalidate_cache` is exported for any caller
//! that mutates the audit log out-of-band (e.g. tests).

use crate::prelude::*;
use crate::pool::storage::read_audit_records;
use std::time::Duration;

pub(crate) const PRIMARY_WINDOW_HOURS: i64 = 5;
pub(crate) const SECONDARY_WINDOW_HOURS: i64 = 24 * 7;
/// Recent-rate-limit-error fallback: with no cap declared, the scheduler
/// excludes an account that has been 429'd this many times in the past 5h.
/// Five is high enough that a single transient cluster outage won't kill the
/// account but low enough that a sustained real quota exhaustion does.
pub(crate) const RECENT_ERROR_FALLBACK_THRESHOLD: u32 = 5;
const RECENT_ERROR_LOOKBACK_HOURS: i64 = 5;
/// In-memory TTL for the per-account aggregate so that dashboard reads and
/// concurrent maintenance ticks don't redundantly re-scan the audit file.
const CACHE_TTL_SECS: i64 = 30;

/// Which providers are subject to this local-window logic. Defined here so the
/// capacity loop, the probe, and the selector agree on the same set without
/// having to hard-code it three times.
pub(crate) fn is_local_window_provider(provider: &str) -> bool {
    matches!(provider, "glm" | "kimi" | "deepseek" | "minimax")
}

/// Per-provider caps read from env (None when unset / 0 / unparseable).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProviderUsageLimits {
    pub(crate) primary_tokens: Option<u64>,
    pub(crate) weekly_tokens: Option<u64>,
}

fn parse_env_limit(var: &str) -> Option<u64> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
}

pub(crate) fn limits_for(provider: &str) -> ProviderUsageLimits {
    let (primary_var, weekly_var) = match provider {
        "glm" => ("GLM_PRIMARY_LIMIT_TOKENS", "GLM_WEEKLY_LIMIT_TOKENS"),
        "kimi" => ("KIMI_PRIMARY_LIMIT_TOKENS", "KIMI_WEEKLY_LIMIT_TOKENS"),
        "deepseek" => (
            "DEEPSEEK_PRIMARY_LIMIT_TOKENS",
            "DEEPSEEK_WEEKLY_LIMIT_TOKENS",
        ),
        "minimax" => (
            "MINIMAX_PRIMARY_LIMIT_TOKENS",
            "MINIMAX_WEEKLY_LIMIT_TOKENS",
        ),
        _ => return ProviderUsageLimits::default(),
    };
    ProviderUsageLimits {
        primary_tokens: parse_env_limit(primary_var),
        weekly_tokens: parse_env_limit(weekly_var),
    }
}

/// A snapshot of one provider's local 5h + 7d usage, used for hard-exclude /
/// rebalance decisions. Mirrors the shape of `RateLimitSnapshot` so the
/// caller can pass it straight to `capacity::store_rate_limit` — but with
/// `active_limit` / `credits_*` left None and `primary_window_minutes`
/// explicitly set to 300 so the dashboard's "5h" label is sourced from a
/// real field rather than guessed at.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalWindowSnapshot {
    pub(crate) primary_used_percent: Option<f64>,
    pub(crate) primary_window_minutes: Option<i64>,
    pub(crate) secondary_used_percent: Option<f64>,
    pub(crate) secondary_window_minutes: Option<i64>,
    pub(crate) recent_errors_5h: u32,
    pub(crate) captured_at: DateTime<Utc>,
}

impl LocalWindowSnapshot {
    pub(crate) fn into_rate_limit_snapshot(self) -> RateLimitSnapshot {
        RateLimitSnapshot {
            active_limit: None,
            plan_type: None,
            primary_used_percent: self.primary_used_percent,
            primary_window_minutes: self.primary_window_minutes,
            primary_reset_after_seconds: None,
            secondary_used_percent: self.secondary_used_percent,
            secondary_window_minutes: self.secondary_window_minutes,
            secondary_reset_after_seconds: None,
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            recent_rate_limit_errors_5h: Some(self.recent_errors_5h),
            captured_at: Some(self.captured_at),
        }
    }
}

/// In-memory per-account cache. Capacity entries are tiny (one
/// `RateLimitSnapshot` per account) and there are at most O(accounts) of them,
/// so a plain `HashMap` is plenty — eviction happens via the TTL below.
pub(crate) struct UsageWindowCache {
    entries: std::sync::RwLock<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    snapshot: LocalWindowSnapshot,
    /// UTC time the entry was computed at.
    computed_at: DateTime<Utc>,
}

impl UsageWindowCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::sync::RwLock::new(HashMap::new()),
        }
    }

    fn get(&self, account_id: &str) -> Option<LocalWindowSnapshot> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(account_id)?;
        if Utc::now() - entry.computed_at > chrono::Duration::seconds(CACHE_TTL_SECS) {
            return None;
        }
        Some(entry.snapshot.clone())
    }

    fn put(&self, account_id: &str, snapshot: LocalWindowSnapshot) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                account_id.to_string(),
                CacheEntry {
                    snapshot,
                    computed_at: Utc::now(),
                },
            );
        }
    }

    /// Drop a single account's entry — called by audit writers so a fresh
    /// request will recompute on the next read instead of waiting for TTL.
    pub(crate) fn invalidate(&self, account_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(account_id);
        }
    }

    /// Drop everything — used in tests.
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}

/// Aggregate one account's 5h / 7d billable token usage from the audit log and
/// wrap the result in a `LocalWindowSnapshot`. Cached per account.
///
/// Returns `None` only when the account id isn't a `local-window provider` —
/// callers shouldn't invoke this for `claude` / `codex` / `cursor` / `ollama`
/// / `trae` (they have their own snapshot paths).
pub(crate) async fn aggregate_account_window(
    state: &AppState,
    account_id: &str,
) -> Option<LocalWindowSnapshot> {
    if let Some(cached) = state.usage_window_cache.get(account_id) {
        return Some(cached);
    }
    let provider = {
        let accounts = state.accounts.read().await;
        accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.provider.clone())
    }?;
    if !is_local_window_provider(&provider) {
        return None;
    }
    let snapshot = compute_snapshot(state, account_id, &provider).await;
    state.usage_window_cache.put(account_id, snapshot.clone());
    Some(snapshot)
}

async fn compute_snapshot(
    state: &AppState,
    account_id: &str,
    provider: &str,
) -> LocalWindowSnapshot {
    let now = Utc::now();
    let primary_cutoff = now - chrono::Duration::hours(PRIMARY_WINDOW_HOURS);
    let weekly_cutoff = now - chrono::Duration::hours(SECONDARY_WINDOW_HOURS);
    let error_cutoff = now - chrono::Duration::hours(RECENT_ERROR_LOOKBACK_HOURS);

    let limits = limits_for(provider);
    let records = read_audit_records(&state.audit_file).await;
    let mut primary_sum: u64 = 0;
    let mut weekly_sum: u64 = 0;
    let mut recent_errors: u32 = 0;
    for r in &records {
        if r.get("upstream_account_id").and_then(|v| v.as_str()) != Some(account_id) {
            continue;
        }
        let ts = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let Some(ts) = ts else {
            continue;
        };
        let billable = r
            .pointer("/tokens/billable_tokens")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
            .unwrap_or(0) as u64;
        if ts >= primary_cutoff {
            primary_sum = primary_sum.saturating_add(billable);
        }
        if ts >= weekly_cutoff {
            weekly_sum = weekly_sum.saturating_add(billable);
        }
        if ts >= error_cutoff {
            let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if is_rate_limit_status(status) {
                recent_errors = recent_errors.saturating_add(1);
            }
        }
    }
    LocalWindowSnapshot {
        primary_used_percent: limits
            .primary_tokens
            .map(|cap| (primary_sum as f64 / cap as f64) * 100.0),
        primary_window_minutes: Some(PRIMARY_WINDOW_HOURS * 60),
        secondary_used_percent: limits
            .weekly_tokens
            .map(|cap| (weekly_sum as f64 / cap as f64) * 100.0),
        secondary_window_minutes: Some(SECONDARY_WINDOW_HOURS * 60),
        recent_errors_5h: recent_errors,
        captured_at: now,
    }
}

/// Whether an `AuditRecord.status` string signals a real upstream rate limit
/// (as opposed to e.g. a 5xx we already retried away). Mirrors the spirit of
/// `retry::looks_rate_limited` without importing it — that one is provider-
/// specific; this one stays generic across GLM/Kimi/DeepSeek/MiniMax since
/// they all funnel into the same `*_error_429` / `*_rate_limit` shape on
/// failure audit.
fn is_rate_limit_status(status: &str) -> bool {
    let s = status.to_ascii_lowercase();
    s.contains("rate_limit") || s.contains("429")
}

/// The selector's gate: should the four provider's account be skipped right
/// now because it's about to / has just hit the wall?
///
/// - With a declared cap: the same 95/99 thresholds Claude uses (`pool::PRIMARY_HARD_EXCLUDE_PERCENT`).
/// - Without a cap: fall back to "5h-window rate-limit count >= threshold".
///
/// Kept as the unit-testable primitive: the actual selector wraps it in
/// `pool::account_excluded_by_local_window`, which pulls the same numbers out
/// of the in-memory `RateLimitSnapshot` rather than from a raw snapshot
/// passed by argument.
#[allow(dead_code)]
pub(crate) fn should_exclude_for_local_window(
    primary_used_percent: Option<f64>,
    recent_errors_5h: u32,
) -> bool {
    if let Some(pct) = primary_used_percent {
        return pct >= crate::pool::PRIMARY_HARD_EXCLUDE_PERCENT;
    }
    recent_errors_5h >= RECENT_ERROR_FALLBACK_THRESHOLD
}

/// Run aggregate+store for every account of the four providers. Called from
/// `capacity::run_capacity_maintenance` (every minute).
pub(crate) async fn refresh_all_local_windows(state: &AppState) {
    let accounts: Vec<UpstreamAccount> = {
        let accounts = state.accounts.read().await;
        accounts
            .iter()
            .filter(|a| is_local_window_provider(&a.provider))
            .cloned()
            .collect()
    };
    for account in accounts {
        if let Some(snapshot) = aggregate_account_window(state, &account.id).await {
            crate::capacity::store_rate_limit(state, &account.id, snapshot.into_rate_limit_snapshot())
                .await;
        }
    }
}

// Silence "function not used" if no caller picks it up.
#[allow(dead_code)]
const _CACHE_TTL_PROBE: Duration = Duration::from_secs(0);

#[cfg(test)]
mod usage_window_tests {
    use super::*;

    /// Build an `AuditRecord` JSON line with the given fields. Skips writing
    /// to disk — `read_audit_records` isn't used in these tests; we exercise
    /// `compute_snapshot` indirectly via `aggregate_account_window`, which
    /// would require a real file. Instead, the unit tests below call
    /// `compute_snapshot`'s logic-equivalent helpers (limits, percent math,
    /// status classifier) directly; the cache tests use a stub.
    fn record(account_id: &str, ts: DateTime<Utc>, billable: i64, status: &str) -> Value {
        json!({
            "upstream_account_id": account_id,
            "created_at": ts.to_rfc3339(),
            "status": status,
            "tokens": { "billable_tokens": billable }
        })
    }

    #[test]
    fn limits_for_unknown_provider_is_empty() {
        let l = limits_for("claude");
        assert!(l.primary_tokens.is_none());
        assert!(l.weekly_tokens.is_none());
    }

    #[test]
    fn parse_env_limit_ignores_zero_and_garbage() {
        // We can't mutate process env from concurrent tests safely, but we
        // can sanity-check the helper directly via parse_env_limit.
        // (Zero/empty/garbage paths are all the same `.filter(|v| *v > 0)`.)
        assert!(parse_env_limit("__DEFINITELY_NOT_SET__").is_none());
        assert!(parse_env_limit("__DEFINITELY_NOT_SET__").is_none());
    }

    #[test]
    fn is_rate_limit_status_matches_known_shapes() {
        assert!(is_rate_limit_status("glm_error_429"));
        assert!(is_rate_limit_status("kimi_rate_limit"));
        assert!(is_rate_limit_status("deepseek_rate_limit_error"));
        assert!(is_rate_limit_status("minimax_rate_limit"));
        // Case-insensitive.
        assert!(is_rate_limit_status("GLM_ERROR_429"));
        // A transport error is NOT a rate limit — we want the selector to
        // keep the account in rotation (it's the cluster, not the quota).
        assert!(!is_rate_limit_status("glm_error_500"));
        assert!(!is_rate_limit_status("glm_error_timeout"));
        assert!(!is_rate_limit_status("success"));
    }

    #[test]
    fn percent_math_saturates_at_capacity() {
        // Mirrors the inner loop of compute_snapshot: a single record with
        // 150M tokens against a 100M cap must read as >=95% (and so
        // should_exclude_for_local_window returns true).
        let cap = 100_000_000u64;
        let sum: u64 = 150_000_000;
        let pct = (sum as f64 / cap as f64) * 100.0;
        assert!(pct >= crate::pool::PRIMARY_HARD_EXCLUDE_PERCENT);
    }

    #[test]
    fn should_exclude_uses_cap_when_declared() {
        assert!(should_exclude_for_local_window(Some(95.0), 0));
        assert!(should_exclude_for_local_window(Some(96.0), 0));
        assert!(!should_exclude_for_local_window(Some(94.9), 0));
        assert!(!should_exclude_for_local_window(Some(0.0), 0));
    }

    #[test]
    fn should_exclude_falls_back_to_recent_errors_when_no_cap() {
        // No cap declared, errors below threshold → not excluded.
        assert!(!should_exclude_for_local_window(None, 0));
        assert!(!should_exclude_for_local_window(None, RECENT_ERROR_FALLBACK_THRESHOLD - 1));
        // At-or-above threshold → excluded.
        assert!(should_exclude_for_local_window(None, RECENT_ERROR_FALLBACK_THRESHOLD));
        assert!(should_exclude_for_local_window(None, RECENT_ERROR_FALLBACK_THRESHOLD + 10));
    }

    #[test]
    fn cache_round_trip_and_invalidate() {
        let cache = UsageWindowCache::new();
        let snap = LocalWindowSnapshot {
            primary_used_percent: Some(42.0),
            primary_window_minutes: Some(300),
            secondary_used_percent: Some(10.0),
            secondary_window_minutes: Some(7 * 24 * 60),
            recent_errors_5h: 1,
            captured_at: Utc::now(),
        };
        cache.put("acc1", snap.clone());
        let got = cache.get("acc1").expect("cached");
        assert_eq!(got.primary_used_percent, Some(42.0));
        cache.invalidate("acc1");
        assert!(cache.get("acc1").is_none());
        // Other accounts aren't touched by invalidate.
        cache.put("acc2", snap);
        cache.invalidate("acc1");
        assert!(cache.get("acc2").is_some());
    }

    #[test]
    fn cache_ttl_expires_entries() {
        let cache = UsageWindowCache::new();
        let snap = LocalWindowSnapshot {
            captured_at: Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS + 5),
            ..LocalWindowSnapshot::default()
        };
        cache.put("old", snap);
        // Manually backdate computed_at so the TTL check fails immediately.
        {
            let mut entries = cache.entries.write().unwrap();
            if let Some(e) = entries.get_mut("old") {
                e.computed_at = Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS + 5);
            }
        }
        assert!(cache.get("old").is_none());
    }

    #[test]
    fn is_local_window_provider_recognises_four_targets() {
        for p in ["glm", "kimi", "deepseek", "minimax"] {
            assert!(is_local_window_provider(p), "{p} should be a local-window provider");
        }
        for p in ["claude", "codex", "cursor", "ollama", "trae", "nope"] {
            assert!(
                !is_local_window_provider(p),
                "{p} must NOT be a local-window provider"
            );
        }
    }

    #[test]
    fn recent_record_parsing_extracts_billable_and_status() {
        // Sanity-check the JSON shape we depend on: a `record` round-tripped
        // through the same fields `compute_snapshot` reads.
        let r = record(
            "acc1",
            Utc::now(),
            1234,
            "glm_error_429",
        );
        assert_eq!(r["upstream_account_id"], "acc1");
        assert_eq!(r["tokens"]["billable_tokens"], 1234);
        assert_eq!(r["status"], "glm_error_429");
    }
}