//! `GET /v1/stats/capacity` — the pool burn-down view: per-account window
//! outlooks (used %, burn rate, projected exhaustion) plus per-provider pool
//! aggregation ("when does the whole pool hit the wall") and a downsampled
//! usage-history series for the dashboard chart.

use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::capacity::{compute_account_outlook, AccountOutlook, WindowSample, HISTORY_RETENTION_HOURS};
use crate::pool::storage::read_audit_records;
use crate::pool::usage_percent_gates_selection;
use crate::routes::accounts::account_health;

/// Chart resolution: one point every 30 minutes over the retention window.
const SERIES_BUCKET_MINUTES: i64 = 30;
/// A per-account observation older than this (relative to the bucket time) is
/// not carried forward into the pool average — the account was dark.
const SERIES_CARRY_FORWARD_MINUTES: i64 = 120;

pub(crate) async fn get_capacity(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = extract_user_id(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response();
    }

    // Refresh snapshots for accounts whose data has gone stale (same path the
    // accounts dashboard uses), so the view reflects upstream reality even on
    // an idle gateway.
    crate::usage::refresh_rate_limits_from_usage(&state).await;

    let now = Utc::now();
    let accounts = state.accounts.read().await.clone();
    let rate_limits = state.rate_limits.read().await.clone();
    let history: HashMap<String, Vec<WindowSample>> = state
        .capacity_history
        .read()
        .await
        .iter()
        .map(|(id, series)| (id.clone(), series.iter().copied().collect()))
        .collect();

    // Billable-token burn per provider from the audit log (real magnitude to
    // pair with the percent-based projections).
    let audit_records = read_audit_records(&state.audit_file).await;
    let token_burn = provider_token_burn(&audit_records, now);

    let mut providers: Vec<ProviderCapacity> = Vec::new();
    for provider in ["claude", "codex", "cursor"] {
        let provider_accounts: Vec<&UpstreamAccount> =
            accounts.iter().filter(|a| a.provider == provider).collect();
        if provider_accounts.is_empty() {
            continue;
        }

        let mut account_views: Vec<AccountCapacity> = Vec::new();
        for account in &provider_accounts {
            let series = history.get(&account.id).cloned().unwrap_or_default();
            // Live snapshot, falling back to the last persisted history sample
            // (covers the gap between a restart and the first probe/request).
            let snapshot = rate_limits
                .get(&account.id)
                .cloned()
                .or_else(|| crate::capacity::snapshot_from_history(&series));
            let outlook = snapshot
                .as_ref()
                .map(|s| compute_account_outlook(provider, s, &series, now))
                .unwrap_or_default();
            account_views.push(AccountCapacity {
                id: account.id.clone(),
                account_label: account.account_label.clone(),
                owner_user_id: account.owner_user_id.clone(),
                share_enabled: account.share_enabled,
                status: account_health(account, now).status,
                plan_type: snapshot.as_ref().and_then(|s| s.plan_type.clone()),
                captured_at: snapshot.as_ref().and_then(|s| s.captured_at),
                outlook,
            });
        }

        let pool = pool_summary(provider, &account_views, token_burn.get(provider), now);
        let series = pool_series(provider, &provider_accounts, &history, now);
        providers.push(ProviderCapacity {
            provider: provider.to_string(),
            pool,
            series,
            accounts: account_views,
        });
    }

    (
        StatusCode::OK,
        Json(CapacityResponse {
            generated_at: now,
            retention_hours: HISTORY_RETENTION_HOURS,
            providers,
        }),
    )
        .into_response()
}

/// Sum billable tokens per provider over the trailing 24h / 1h.
fn provider_token_burn(records: &[Value], now: DateTime<Utc>) -> HashMap<String, TokenBurn> {
    let day_cutoff = now - chrono::Duration::hours(24);
    let hour_cutoff = now - chrono::Duration::hours(1);
    let mut map: HashMap<String, TokenBurn> = HashMap::new();
    for r in records {
        let Some(created_at) = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
        else {
            continue;
        };
        if created_at < day_cutoff {
            continue;
        }
        let provider = r
            .get("routed_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if provider.is_empty() {
            continue;
        }
        let billable = r
            .pointer("/tokens/billable_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as u64;
        if billable == 0 {
            continue;
        }
        let burn = map.entry(provider.to_string()).or_default();
        burn.billable_tokens_last_24h += billable;
        if created_at >= hour_cutoff {
            burn.billable_tokens_last_1h += billable;
        }
    }
    map
}

/// Aggregate one provider's accounts into the pool-level outlook.
fn pool_summary(
    provider: &str,
    accounts: &[AccountCapacity],
    token_burn: Option<&TokenBurn>,
    now: DateTime<Utc>,
) -> PoolSummary {
    let usable: Vec<&AccountCapacity> = accounts
        .iter()
        .filter(|a| a.status != "dead" && a.status != "disabled")
        .collect();

    let avg = |pick: fn(&AccountOutlook) -> Option<&crate::capacity::WindowOutlook>| -> Option<f64> {
        let vals: Vec<f64> = usable
            .iter()
            .filter_map(|a| pick(&a.outlook).map(|w| w.used_percent))
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
    };
    let avg_burn = |pick: fn(&AccountOutlook) -> Option<&crate::capacity::WindowOutlook>| -> Option<f64> {
        let vals: Vec<f64> = usable
            .iter()
            .filter_map(|a| pick(&a.outlook).and_then(|w| w.burn_rate_pct_per_hour))
            .collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
    };
    fn primary(o: &AccountOutlook) -> Option<&crate::capacity::WindowOutlook> {
        o.primary.as_ref()
    }
    fn secondary(o: &AccountOutlook) -> Option<&crate::capacity::WindowOutlook> {
        o.secondary.as_ref()
    }

    // An account is "at risk" when its primary window is projected to hit 100%
    // before reset, or is already hard-excluded by the scheduler (>=95%).
    let gates = usage_percent_gates_selection(provider);
    let at_risk: Vec<&&AccountCapacity> = usable
        .iter()
        .filter(|a| {
            gates
                && a.outlook
                    .primary
                    .as_ref()
                    .map(|w| w.exhaust_before_reset || w.used_percent >= 95.0)
                    .unwrap_or(false)
        })
        .collect();

    // Earliest projected single-account wall.
    let earliest = at_risk
        .iter()
        .filter_map(|a| {
            let w = a.outlook.primary.as_ref()?;
            // Already over the hard-exclude line counts as "now".
            let minutes = if w.used_percent >= 95.0 {
                0
            } else {
                w.minutes_to_exhaust?
            };
            Some((minutes, a.account_label.clone()))
        })
        .min_by_key(|(m, _)| *m);

    // The POOL hits the wall only when every usable account does. If any
    // account survives until its reset (or isn't burning), the pool stays up.
    let pool_exhaust_at = (gates && !usable.is_empty() && at_risk.len() == usable.len())
        .then(|| {
            at_risk
                .iter()
                .filter_map(|a| {
                    let w = a.outlook.primary.as_ref()?;
                    Some(if w.used_percent >= 95.0 { 0 } else { w.minutes_to_exhaust? })
                })
                .max()
                .map(|minutes| now + chrono::Duration::minutes(minutes))
        })
        .flatten();

    PoolSummary {
        accounts_total: accounts.len(),
        accounts_usable: usable.len(),
        accounts_at_risk: at_risk.len(),
        avg_primary_used_percent: avg(primary),
        avg_secondary_used_percent: avg(secondary),
        avg_primary_burn_pct_per_hour: avg_burn(primary),
        avg_secondary_burn_pct_per_hour: avg_burn(secondary),
        earliest_exhaust_minutes: earliest.as_ref().map(|(m, _)| *m),
        earliest_exhaust_label: earliest.map(|(_, l)| l),
        pool_exhaust_at,
        billable_tokens_last_24h: token_burn.map(|t| t.billable_tokens_last_24h).unwrap_or(0),
        billable_tokens_last_1h: token_burn.map(|t| t.billable_tokens_last_1h).unwrap_or(0),
    }
}

/// Pool-average usage series for the burn-down chart: for each 30-minute
/// bucket, each account contributes its latest observation within the carry-
/// forward horizon; the point is the average across contributing accounts.
fn pool_series(
    provider: &str,
    accounts: &[&UpstreamAccount],
    history: &HashMap<String, Vec<WindowSample>>,
    now: DateTime<Utc>,
) -> Vec<SeriesPoint> {
    let _ = provider;
    let start = now - chrono::Duration::hours(HISTORY_RETENTION_HOURS);
    let carry = chrono::Duration::minutes(SERIES_CARRY_FORWARD_MINUTES);
    let bucket = chrono::Duration::minutes(SERIES_BUCKET_MINUTES);

    // Per-account sorted series (already chronological by construction).
    let series_per_account: Vec<&Vec<WindowSample>> = accounts
        .iter()
        .filter_map(|a| history.get(&a.id))
        .filter(|s| !s.is_empty())
        .collect();
    if series_per_account.is_empty() {
        return Vec::new();
    }

    let mut points = Vec::new();
    let mut t = start;
    // Walk a cursor per account so the whole series build stays O(samples).
    let mut cursors = vec![0usize; series_per_account.len()];
    while t <= now {
        let mut primary_vals = Vec::new();
        let mut secondary_vals = Vec::new();
        for (i, series) in series_per_account.iter().enumerate() {
            while cursors[i] + 1 < series.len() && series[cursors[i] + 1].at <= t {
                cursors[i] += 1;
            }
            let sample = series[cursors[i]];
            if sample.at > t || t - sample.at > carry {
                continue;
            }
            if let Some(p) = sample.primary {
                primary_vals.push(p);
            }
            if let Some(s) = sample.secondary {
                secondary_vals.push(s);
            }
        }
        if !primary_vals.is_empty() || !secondary_vals.is_empty() {
            points.push(SeriesPoint {
                at: t,
                primary: (!primary_vals.is_empty())
                    .then(|| primary_vals.iter().sum::<f64>() / primary_vals.len() as f64),
                secondary: (!secondary_vals.is_empty())
                    .then(|| secondary_vals.iter().sum::<f64>() / secondary_vals.len() as f64),
            });
        }
        t += bucket;
    }
    points
}

// ---------------------------------------------------------------------------
// Response DTOs for `GET /v1/stats/capacity`.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct CapacityResponse {
    pub(crate) generated_at: DateTime<Utc>,
    pub(crate) retention_hours: i64,
    pub(crate) providers: Vec<ProviderCapacity>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCapacity {
    pub(crate) provider: String,
    pub(crate) pool: PoolSummary,
    pub(crate) series: Vec<SeriesPoint>,
    pub(crate) accounts: Vec<AccountCapacity>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PoolSummary {
    pub(crate) accounts_total: usize,
    pub(crate) accounts_usable: usize,
    pub(crate) accounts_at_risk: usize,
    pub(crate) avg_primary_used_percent: Option<f64>,
    pub(crate) avg_secondary_used_percent: Option<f64>,
    pub(crate) avg_primary_burn_pct_per_hour: Option<f64>,
    pub(crate) avg_secondary_burn_pct_per_hour: Option<f64>,
    /// Minutes until the FIRST account is projected to hit its primary wall
    /// (0 = an account is already hard-excluded).
    pub(crate) earliest_exhaust_minutes: Option<i64>,
    pub(crate) earliest_exhaust_label: Option<String>,
    /// When the LAST usable account is projected to hit the wall — i.e. the
    /// whole pool goes down. None = at the current rate the pool survives
    /// (some account resets first or isn't burning).
    pub(crate) pool_exhaust_at: Option<DateTime<Utc>>,
    pub(crate) billable_tokens_last_24h: u64,
    pub(crate) billable_tokens_last_1h: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct SeriesPoint {
    pub(crate) at: DateTime<Utc>,
    pub(crate) primary: Option<f64>,
    pub(crate) secondary: Option<f64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountCapacity {
    pub(crate) id: String,
    pub(crate) account_label: String,
    pub(crate) owner_user_id: String,
    pub(crate) share_enabled: bool,
    pub(crate) status: String,
    pub(crate) plan_type: Option<String>,
    pub(crate) captured_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub(crate) outlook: AccountOutlook,
}

#[derive(Debug, Default, Clone, Copy)]
struct TokenBurn {
    billable_tokens_last_24h: u64,
    billable_tokens_last_1h: u64,
}
