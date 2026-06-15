//! Per-user quota enforcement: token budgets (daily/weekly, UTC) and a
//! fixed-window requests-per-minute limit, all configured by environment
//! variables. Budgets count BORROWED usage only — tokens served by accounts
//! the user does not own — because their purpose is to protect the shared
//! pool, not to ration owners on their own subscriptions. A user over budget
//! is therefore not cut off outright: requests are restricted to their own
//! accounts, and only rejected with 429 when they own none for the provider.
//!
//! Token counts come from the audit ledger (`state.user_usage`): rebuilt from
//! the audit log every 5 minutes and live-bumped per request, so enforcement
//! lags one in-flight generation at most. The Codex WS relay does not parse
//! token usage, so WS traffic is gated at connect time (RPM + current budget)
//! but does not add to the budgets.

use crate::prelude::*;

/// Org-wide per-user limits, read once from the environment. Unset or 0
/// disables the corresponding limit.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct QuotaConfig {
    /// `GATEWAY_USER_DAILY_TOKEN_LIMIT`: borrowed billable tokens per user per UTC day.
    pub(crate) daily_tokens: Option<u64>,
    /// `GATEWAY_USER_WEEKLY_TOKEN_LIMIT`: borrowed billable tokens per user per rolling 7 days.
    pub(crate) weekly_tokens: Option<u64>,
    /// `GATEWAY_USER_RPM_LIMIT`: requests per user per minute, across all providers.
    pub(crate) rpm: Option<u32>,
}

impl QuotaConfig {
    fn from_env() -> QuotaConfig {
        fn limit(name: &str) -> Option<u64> {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v > 0)
        }
        QuotaConfig {
            daily_tokens: limit("GATEWAY_USER_DAILY_TOKEN_LIMIT"),
            weekly_tokens: limit("GATEWAY_USER_WEEKLY_TOKEN_LIMIT"),
            rpm: limit("GATEWAY_USER_RPM_LIMIT").map(|v| v.min(u32::MAX as u64) as u32),
        }
    }

    fn any_enabled(&self) -> bool {
        self.daily_tokens.is_some() || self.weekly_tokens.is_some() || self.rpm.is_some()
    }
}

fn config() -> &'static QuotaConfig {
    static CONFIG: std::sync::OnceLock<QuotaConfig> = std::sync::OnceLock::new();
    CONFIG.get_or_init(QuotaConfig::from_env)
}

pub(crate) fn log_startup() {
    let cfg = config();
    if cfg.any_enabled() {
        info!(
            "per-user quotas enabled: daily_tokens={:?} weekly_tokens={:?} rpm={:?} (borrowed usage only)",
            cfg.daily_tokens, cfg.weekly_tokens, cfg.rpm
        );
    }
}

/// Which budget a user has exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetExceeded {
    Daily,
    Weekly,
}

/// Pure budget check against the borrowed-usage ledger entry.
fn budget_exceeded(
    cfg: &QuotaConfig,
    stat: Option<&UserUsageStat>,
    today: NaiveDate,
) -> Option<BudgetExceeded> {
    let stat = stat?;
    if let Some(limit) = cfg.daily_tokens {
        if stat.daily_billable_on(today) >= limit {
            return Some(BudgetExceeded::Daily);
        }
    }
    if let Some(limit) = cfg.weekly_tokens {
        if stat.weekly_billable >= limit {
            return Some(BudgetExceeded::Weekly);
        }
    }
    None
}

/// Record one request in the user's fixed-window minute counter and report
/// whether it pushed them over `rpm`. Counting rejected attempts too is
/// deliberate — a client hammering into the limit stays limited.
fn note_request_and_check_rpm(entry: &mut UserRequestRate, minute: i64, rpm: u32) -> bool {
    if entry.minute != minute {
        entry.minute = minute;
        entry.count = 0;
    }
    entry.count = entry.count.saturating_add(1);
    entry.count > rpm
}

fn too_many_requests(message: String, retry_after_secs: Option<i64>) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": {
                "type": "rate_limit_error",
                "message": message,
            }
        })),
    )
        .into_response();
    if let Some(secs) = retry_after_secs.filter(|s| *s > 0) {
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            response.headers_mut().insert("retry-after", v);
        }
    }
    response
}

/// Seconds until the next UTC midnight (when the daily budget resets). At
/// exactly midnight this is 0, not a full day (the outer `% 86_400` collapses
/// the `86_400 - 0` boundary case).
fn seconds_to_utc_midnight(now: DateTime<Utc>) -> i64 {
    (86_400 - now.timestamp().rem_euclid(86_400)).rem_euclid(86_400)
}

/// Gate one request for `user_id` against the per-user quotas.
///
/// Returns `Ok(owned_only)`: `false` = unrestricted, `true` = the user is over
/// a token budget but owns usable accounts for this provider, so selection
/// must be restricted to those. Returns `Err(429 response)` when the RPM limit
/// is hit, or a token budget is exhausted and the user owns no usable account.
///
/// `owner_trusted` gates the over-budget fallback to the user's own accounts:
/// an untrusted identity (unverified JWT / anonymous) has no provable ownership,
/// so it can never borrow against "its own" accounts and is rejected outright
/// once over the shared budget.
pub(crate) async fn enforce_user_quota(
    state: &AppState,
    provider: &str,
    user_id: &str,
    owner_trusted: bool,
) -> Result<bool, Response> {
    let cfg = config();
    if !cfg.any_enabled() {
        return Ok(false);
    }
    let now = Utc::now();

    if let Some(rpm) = cfg.rpm {
        let minute = now.timestamp().div_euclid(60);
        let over = {
            let mut rates = state.user_request_rate.write().await;
            let entry = rates.entry(user_id.to_string()).or_default();
            note_request_and_check_rpm(entry, minute, rpm)
        };
        if over {
            return Err(too_many_requests(
                format!("user `{}` exceeded the request rate limit ({}/min)", user_id, rpm),
                Some(60 - now.timestamp().rem_euclid(60)),
            ));
        }
    }

    let exceeded = {
        let usage = state.user_usage.read().await;
        budget_exceeded(cfg, usage.get(user_id), now.date_naive())
    };
    let Some(kind) = exceeded else {
        return Ok(false);
    };

    // Over budget: own accounts stay usable (the budget meters borrowing) — but
    // only for an owner-trusted identity. Untrusted callers can't claim
    // ownership, so they fall straight through to the 429.
    let owns_usable = owner_trusted && {
        let accounts = state.accounts.read().await;
        accounts.iter().any(|a| {
            a.provider == provider
                && a.owner_user_id == user_id
                && !a.runtime.dead
                && !a.runtime.disabled
        })
    };
    if owns_usable {
        return Ok(true);
    }
    let (label, limit, retry_after) = match kind {
        BudgetExceeded::Daily => (
            "daily",
            cfg.daily_tokens.unwrap_or_default(),
            Some(seconds_to_utc_midnight(now)),
        ),
        BudgetExceeded::Weekly => ("weekly", cfg.weekly_tokens.unwrap_or_default(), None),
    };
    Err(too_many_requests(
        format!(
            "user `{}` exhausted the {} shared-pool token budget ({} billable tokens); \
             connect your own {} account to keep going",
            user_id, label, limit, provider
        ),
        retry_after,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(daily: Option<u64>, weekly: Option<u64>) -> QuotaConfig {
        QuotaConfig { daily_tokens: daily, weekly_tokens: weekly, rpm: None }
    }

    #[test]
    fn budgets_check_daily_then_weekly() {
        let today = Utc::now().date_naive();
        let stat = UserUsageStat {
            day: Some(today),
            daily_billable: 100,
            weekly_billable: 500,
            computed_at: None,
        };
        // No usage recorded => within budget.
        assert_eq!(budget_exceeded(&cfg(Some(1), Some(1)), None, today), None);
        // Under both limits.
        assert_eq!(budget_exceeded(&cfg(Some(101), Some(501)), Some(&stat), today), None);
        // At the daily limit => Daily wins.
        assert_eq!(
            budget_exceeded(&cfg(Some(100), Some(501)), Some(&stat), today),
            Some(BudgetExceeded::Daily)
        );
        // Weekly only.
        assert_eq!(
            budget_exceeded(&cfg(None, Some(500)), Some(&stat), today),
            Some(BudgetExceeded::Weekly)
        );
        // Limits disabled => never exceeded.
        assert_eq!(budget_exceeded(&cfg(None, None), Some(&stat), today), None);
    }

    #[test]
    fn stale_daily_counter_reads_as_zero() {
        let today = Utc::now().date_naive();
        let stat = UserUsageStat {
            day: Some(today - chrono::Duration::days(1)),
            daily_billable: 10_000,
            weekly_billable: 10_000,
            computed_at: None,
        };
        // Yesterday's exhausted daily counter doesn't block today...
        assert_eq!(budget_exceeded(&cfg(Some(100), None), Some(&stat), today), None);
        // ...but the rolling weekly total still does.
        assert_eq!(
            budget_exceeded(&cfg(Some(100), Some(100)), Some(&stat), today),
            Some(BudgetExceeded::Weekly)
        );
    }

    #[test]
    fn rpm_fixed_window_counts_and_resets() {
        let mut entry = UserRequestRate::default();
        // 3 requests allowed at rpm=3, the 4th is over.
        assert!(!note_request_and_check_rpm(&mut entry, 100, 3));
        assert!(!note_request_and_check_rpm(&mut entry, 100, 3));
        assert!(!note_request_and_check_rpm(&mut entry, 100, 3));
        assert!(note_request_and_check_rpm(&mut entry, 100, 3));
        // Rejected attempts keep counting within the window.
        assert!(note_request_and_check_rpm(&mut entry, 100, 3));
        // Next minute resets the window.
        assert!(!note_request_and_check_rpm(&mut entry, 101, 3));
    }
}
