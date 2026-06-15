//! Capacity model: per-account rate-limit-window history, burn rates, and
//! time-to-exhaustion outlooks.
//!
//! Every `RateLimitSnapshot` the gateway sees (live response headers, usage
//! probes, relay calls) already carries the upstream's own usage percentages —
//! but until now each snapshot overwrote the last and the trajectory was lost.
//! This module keeps a bounded time series per account and derives from it:
//!
//! - **burn rate** (percent/hour) per window, reset-aware: a big downward jump
//!   between samples is a window reset, so the rate comes from the most recent
//!   monotonic segment only;
//! - **outlooks** (`AccountOutlook`): projected minutes until a window hits
//!   100% and whether that happens before the window resets. The scheduler uses
//!   these to migrate sticky sessions off accounts that are about to hit the
//!   wall (`pool::should_rebalance_affinity`), and `/v1/stats/capacity` exposes
//!   them with pool-level aggregation for the dashboard's burn-down view.
//!
//! History survives restarts via a periodic whole-file snapshot
//! (`data/capacity.ndjson`, temp+fsync+rename like the account store) — losing
//! at most one save interval of samples is acceptable for hour-scale trends.

use crate::prelude::*;
use crate::pool::storage::sync_parent_dir;
use std::collections::VecDeque;

/// Samples older than this are pruned; also bounds the burn-down chart x-axis.
pub(crate) const HISTORY_RETENTION_HOURS: i64 = 48;
/// Hard per-account sample cap (~1 sample/90s for 48h), guarding memory even
/// if something floods snapshots.
const HISTORY_MAX_SAMPLES: usize = 2000;
/// Collapse near-duplicate samples arriving in a burst (parallel requests all
/// reporting the same percentages).
const HISTORY_MIN_GAP_SECS: i64 = 30;
/// A drop of at least this many percentage points between consecutive samples
/// is treated as a window reset (segment boundary), not negative burn.
const RESET_DROP_PERCENT: f64 = 5.0;
/// Burn-rate lookback per window kind. The 5h primary window moves fast; the
/// weekly secondary window needs a longer baseline to be meaningful.
const PRIMARY_BURN_LOOKBACK_HOURS: i64 = 3;
const SECONDARY_BURN_LOOKBACK_HOURS: i64 = 24;
/// Minimum observed span before a burn rate is trusted; below this the rate is
/// reported as unknown rather than wildly extrapolated from two close samples.
const MIN_BURN_SPAN_MINUTES: i64 = 10;
/// Burn rates below this round to "not burning" so an idle account never gets
/// a multi-week absurd exhaustion projection.
const MIN_BURN_RATE_PCT_PER_HOUR: f64 = 0.05;

/// One observed point of an account's usage windows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct WindowSample {
    pub(crate) at: DateTime<Utc>,
    /// Primary (5h) window used percent, when reported.
    pub(crate) primary: Option<f64>,
    /// Secondary (weekly) window used percent, when reported.
    pub(crate) secondary: Option<f64>,
}

/// Persisted line shape: the in-memory map key flattened in.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedSample {
    account_id: String,
    at: DateTime<Utc>,
    primary: Option<f64>,
    secondary: Option<f64>,
}

/// Forward-looking view of one usage window.
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct WindowOutlook {
    pub(crate) used_percent: f64,
    /// None = not enough history to estimate.
    pub(crate) burn_rate_pct_per_hour: Option<f64>,
    /// Projected minutes until 100% at the current burn rate; None when the
    /// account isn't burning (or burn is unknown).
    pub(crate) minutes_to_exhaust: Option<i64>,
    /// Seconds until the window resets, adjusted to "from now" (the raw
    /// snapshot value is relative to its capture time).
    pub(crate) reset_after_seconds: Option<i64>,
    /// True when the projection says the window hits 100% before it resets —
    /// i.e. this account WILL hit the wall unless traffic moves off it.
    pub(crate) exhaust_before_reset: bool,
}

/// Forward-looking view of one account (both windows).
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct AccountOutlook {
    pub(crate) primary: Option<WindowOutlook>,
    pub(crate) secondary: Option<WindowOutlook>,
    pub(crate) computed_at: Option<DateTime<Utc>>,
}

impl AccountOutlook {
    /// Whether the account is projected to hit its primary wall within
    /// `within_minutes` (and before the window resets). Drives the sticky-
    /// session rebalance decision.
    pub(crate) fn primary_exhaust_within(&self, within_minutes: i64) -> bool {
        self.primary
            .as_ref()
            .map(|w| {
                w.exhaust_before_reset
                    && w.minutes_to_exhaust
                        .map(|m| m <= within_minutes)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    }
}

/// Store a fresh rate-limit snapshot: updates the live `rate_limits` map AND
/// appends to the capacity history. All snapshot producers should go through
/// here so the trajectory is never lost.
pub(crate) async fn store_rate_limit(state: &AppState, account_id: &str, snapshot: RateLimitSnapshot) {
    note_history_sample(state, account_id, &snapshot).await;
    state
        .rate_limits
        .write()
        .await
        .insert(account_id.to_string(), snapshot);
}

/// Append one history sample from a snapshot (no-op for snapshots that carry
/// no percentages, e.g. pure auth_invalid markers).
async fn note_history_sample(state: &AppState, account_id: &str, snapshot: &RateLimitSnapshot) {
    if snapshot.primary_used_percent.is_none() && snapshot.secondary_used_percent.is_none() {
        return;
    }
    let sample = WindowSample {
        at: snapshot.captured_at.unwrap_or_else(Utc::now),
        primary: snapshot.primary_used_percent.map(|v| v.clamp(0.0, 100.0)),
        secondary: snapshot.secondary_used_percent.map(|v| v.clamp(0.0, 100.0)),
    };
    let mut history = state.capacity_history.write().await;
    let series = history.entry(account_id.to_string()).or_default();
    if let Some(last) = series.back() {
        // Out-of-order (a slow probe finishing after a live header) — drop.
        if sample.at < last.at {
            return;
        }
        // Burst collapse: same numbers within the dedup gap add no signal.
        let close = (sample.at - last.at).num_seconds() < HISTORY_MIN_GAP_SECS;
        let same = approx_eq(sample.primary, last.primary) && approx_eq(sample.secondary, last.secondary);
        if close && same {
            return;
        }
    }
    series.push_back(sample);
    while series.len() > HISTORY_MAX_SAMPLES {
        series.pop_front();
    }
}

fn approx_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() < 0.01,
        _ => false,
    }
}

/// Drop an account's history + outlook (account deleted).
pub(crate) async fn forget_account(state: &AppState, account_id: &str) {
    state.capacity_history.write().await.remove(account_id);
    state.capacity_outlooks.write().await.remove(account_id);
}

/// Reset-aware burn rate over `(time, percent)` points: percent/hour derived
/// from the most recent monotonic segment within the lookback (a drop of
/// >= RESET_DROP_PERCENT starts a new segment). Returns None when the segment
/// is too short to trust.
fn window_burn_rate(points: &[(DateTime<Utc>, f64)], lookback_hours: i64, now: DateTime<Utc>) -> Option<f64> {
    let cutoff = now - chrono::Duration::hours(lookback_hours);
    let recent: Vec<&(DateTime<Utc>, f64)> = points.iter().filter(|(t, _)| *t >= cutoff).collect();
    if recent.len() < 2 {
        return None;
    }
    let mut start = 0usize;
    for i in 1..recent.len() {
        if recent[i].1 - recent[i - 1].1 < -RESET_DROP_PERCENT {
            start = i;
        }
    }
    let segment = &recent[start..];
    let (first, last) = (segment.first()?, segment.last()?);
    let span_minutes = (last.0 - first.0).num_minutes();
    if segment.len() < 2 || span_minutes < MIN_BURN_SPAN_MINUTES {
        return None;
    }
    let hours = (last.0 - first.0).num_seconds() as f64 / 3600.0;
    Some(((last.1 - first.1) / hours).max(0.0))
}

/// Build the outlook for one window from its history + the latest snapshot
/// values. `reset_after` is the raw snapshot value, re-anchored to `now` here.
fn window_outlook(
    points: &[(DateTime<Utc>, f64)],
    used_percent: f64,
    reset_after: Option<i64>,
    captured_at: Option<DateTime<Utc>>,
    lookback_hours: i64,
    gates_selection: bool,
    now: DateTime<Utc>,
) -> WindowOutlook {
    let burn = window_burn_rate(points, lookback_hours, now)
        .map(|b| if b < MIN_BURN_RATE_PCT_PER_HOUR { 0.0 } else { b });
    let reset_after_seconds = reset_after.map(|s| {
        let elapsed = captured_at.map(|t| (now - t).num_seconds()).unwrap_or(0);
        (s - elapsed).max(0)
    });
    let minutes_to_exhaust = burn.filter(|b| *b > 0.0).map(|b| {
        let remaining = (100.0 - used_percent).max(0.0);
        (remaining / b * 60.0).round() as i64
    });
    // "Will hit the wall": projected exhaustion lands before the reset. With no
    // reset hint, a real projection counts as a threat (conservative). Windows
    // whose percent doesn't gate selection (Cursor's spend allowance) never
    // count as a wall.
    let exhaust_before_reset = gates_selection
        && match (minutes_to_exhaust, reset_after_seconds) {
            (Some(m), Some(r)) => m * 60 < r,
            (Some(_), None) => true,
            (None, _) => false,
        };
    WindowOutlook {
        used_percent,
        burn_rate_pct_per_hour: burn,
        minutes_to_exhaust,
        reset_after_seconds,
        exhaust_before_reset,
    }
}

/// Compute the outlook for one account from its snapshot + history series.
pub(crate) fn compute_account_outlook(
    provider: &str,
    snapshot: &RateLimitSnapshot,
    series: &[WindowSample],
    now: DateTime<Utc>,
) -> AccountOutlook {
    let gates = crate::pool::usage_percent_gates_selection(provider);
    let primary_points: Vec<(DateTime<Utc>, f64)> =
        series.iter().filter_map(|s| s.primary.map(|p| (s.at, p))).collect();
    let secondary_points: Vec<(DateTime<Utc>, f64)> =
        series.iter().filter_map(|s| s.secondary.map(|p| (s.at, p))).collect();
    AccountOutlook {
        primary: snapshot.primary_used_percent.map(|used| {
            window_outlook(
                &primary_points,
                used.clamp(0.0, 100.0),
                snapshot.primary_reset_after_seconds,
                snapshot.captured_at,
                PRIMARY_BURN_LOOKBACK_HOURS,
                gates,
                now,
            )
        }),
        secondary: snapshot.secondary_used_percent.map(|used| {
            window_outlook(
                &secondary_points,
                used.clamp(0.0, 100.0),
                snapshot.secondary_reset_after_seconds,
                snapshot.captured_at,
                SECONDARY_BURN_LOOKBACK_HOURS,
                gates,
                now,
            )
        }),
        computed_at: Some(now),
    }
}

/// Latest-known usage as a snapshot, for accounts with history but no live
/// `rate_limits` entry yet (typically right after a restart — the persisted
/// history outlives the in-memory snapshot map). No reset info: percents only.
pub(crate) fn snapshot_from_history(series: &[WindowSample]) -> Option<RateLimitSnapshot> {
    let last = series.last()?;
    Some(RateLimitSnapshot {
        primary_used_percent: last.primary,
        secondary_used_percent: last.secondary,
        captured_at: Some(last.at),
        ..RateLimitSnapshot::default()
    })
}

/// Recompute every account's outlook into `state.capacity_outlooks`.
pub(crate) async fn refresh_outlooks(state: &AppState) {
    let now = Utc::now();
    let providers: HashMap<String, String> = state
        .accounts
        .read()
        .await
        .iter()
        .map(|a| (a.id.clone(), a.provider.clone()))
        .collect();
    let rate_limits = state.rate_limits.read().await.clone();
    let history = state.capacity_history.read().await;
    let mut outlooks: HashMap<String, AccountOutlook> = HashMap::new();
    for (account_id, provider) in providers.iter() {
        let series: Vec<WindowSample> = history
            .get(account_id)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default();
        let Some(snapshot) = rate_limits
            .get(account_id)
            .cloned()
            .or_else(|| snapshot_from_history(&series))
        else {
            continue;
        };
        outlooks.insert(
            account_id.clone(),
            compute_account_outlook(provider, &snapshot, &series, now),
        );
    }
    drop(history);
    *state.capacity_outlooks.write().await = outlooks;
}

/// Prune history beyond the retention window (and accounts no longer in the pool).
async fn prune_history(state: &AppState) {
    let cutoff = Utc::now() - chrono::Duration::hours(HISTORY_RETENTION_HOURS);
    let known: std::collections::HashSet<String> = state
        .accounts
        .read()
        .await
        .iter()
        .map(|a| a.id.clone())
        .collect();
    let mut history = state.capacity_history.write().await;
    history.retain(|id, _| known.contains(id));
    for series in history.values_mut() {
        while series.front().map(|s| s.at < cutoff).unwrap_or(false) {
            series.pop_front();
        }
    }
}

/// Load the persisted history snapshot at startup.
pub(crate) async fn load_capacity_history(path: &PathBuf) -> HashMap<String, VecDeque<WindowSample>> {
    let mut out: HashMap<String, VecDeque<WindowSample>> = HashMap::new();
    let Ok(content) = tokio::fs::read_to_string(path).await else {
        return out;
    };
    let cutoff = Utc::now() - chrono::Duration::hours(HISTORY_RETENTION_HOURS);
    for line in content.lines() {
        let Ok(row) = serde_json::from_str::<PersistedSample>(line) else {
            continue;
        };
        if row.at < cutoff {
            continue;
        }
        out.entry(row.account_id).or_default().push_back(WindowSample {
            at: row.at,
            primary: row.primary,
            secondary: row.secondary,
        });
    }
    for series in out.values_mut() {
        series
            .make_contiguous()
            .sort_by_key(|s| s.at);
    }
    out
}

/// Persist the (pruned) history. Whole-file snapshot via temp+fsync+rename —
/// same durability pattern as the account store, but for derived data, so a
/// failed write is only logged.
async fn save_capacity_history(state: &AppState) {
    let history = state.capacity_history.read().await.clone();
    let mut lines = String::new();
    for (account_id, series) in history.iter() {
        for s in series {
            let row = PersistedSample {
                account_id: account_id.clone(),
                at: s.at,
                primary: s.primary,
                secondary: s.secondary,
            };
            if let Ok(json) = serde_json::to_string(&row) {
                lines.push_str(&json);
                lines.push('\n');
            }
        }
    }
    let tmp = state.capacity_file.with_extension("ndjson.tmp");
    let write = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        file.write_all(lines.as_bytes()).await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        tokio::fs::rename(&tmp, &state.capacity_file)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    };
    match write.await {
        Ok(()) => sync_parent_dir(&state.capacity_file).await,
        Err(e) => warn!("failed persisting capacity history: {}", e),
    }
}

/// Background maintenance: recompute outlooks every minute (they feed the
/// scheduler's rebalance decision), prune history, and persist it every 5 ticks.
pub(crate) async fn run_capacity_maintenance(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    let mut ticks: u64 = 0;
    loop {
        tick.tick().await;
        ticks += 1;
        prune_history(&state).await;
        refresh_outlooks(&state).await;
        if ticks % 5 == 0 {
            save_capacity_history(&state).await;
        }
    }
}

#[cfg(test)]
mod burn_tests {
    use super::*;

    fn points(now: DateTime<Utc>, steps: &[(i64, f64)]) -> Vec<(DateTime<Utc>, f64)> {
        // steps: (minutes ago, percent)
        steps
            .iter()
            .map(|(mins, pct)| (now - chrono::Duration::minutes(*mins), *pct))
            .collect()
    }

    #[test]
    fn burn_rate_from_monotonic_segment() {
        let now = Utc::now();
        // 20% -> 50% over 60 minutes = 30%/h.
        let pts = points(now, &[(60, 20.0), (30, 35.0), (0, 50.0)]);
        let burn = window_burn_rate(&pts, 3, now).expect("burn");
        assert!((burn - 30.0).abs() < 0.5, "burn={burn}");
    }

    #[test]
    fn burn_rate_ignores_pre_reset_history() {
        let now = Utc::now();
        // 90% then RESET to 5%, then climbs to 15%: rate must come from the
        // post-reset segment (10%/30min = 20%/h), not read the drop as negative.
        let pts = points(now, &[(120, 80.0), (90, 90.0), (30, 5.0), (15, 10.0), (0, 15.0)]);
        let burn = window_burn_rate(&pts, 3, now).expect("burn");
        assert!((burn - 20.0).abs() < 1.0, "burn={burn}");
    }

    #[test]
    fn burn_rate_requires_enough_span() {
        let now = Utc::now();
        let pts = points(now, &[(5, 10.0), (0, 20.0)]); // only 5 minutes
        assert!(window_burn_rate(&pts, 3, now).is_none());
    }

    #[test]
    fn idle_account_never_projects_exhaustion() {
        let now = Utc::now();
        let pts = points(now, &[(120, 40.0), (0, 40.0)]);
        let outlook = window_outlook(&pts, 40.0, Some(3600), Some(now), 3, true, now);
        assert_eq!(outlook.burn_rate_pct_per_hour, Some(0.0));
        assert_eq!(outlook.minutes_to_exhaust, None);
        assert!(!outlook.exhaust_before_reset);
    }

    #[test]
    fn exhaust_before_reset_compares_projection_to_reset() {
        let now = Utc::now();
        // 50% used, +30%/h => ~100 minutes to wall.
        let pts = points(now, &[(60, 20.0), (0, 50.0)]);
        // Reset in 4h: wall hits first.
        let hits = window_outlook(&pts, 50.0, Some(4 * 3600), Some(now), 3, true, now);
        assert!(hits.exhaust_before_reset);
        let m = hits.minutes_to_exhaust.expect("minutes");
        assert!((90..=110).contains(&m), "minutes={m}");
        // Reset in 30min: reset wins, no wall.
        let resets = window_outlook(&pts, 50.0, Some(30 * 60), Some(now), 3, true, now);
        assert!(!resets.exhaust_before_reset);
    }

    #[test]
    fn non_gating_provider_never_flags_wall() {
        let now = Utc::now();
        let pts = points(now, &[(60, 20.0), (0, 50.0)]);
        // Cursor-style window: percent is a spend allowance, not a wall.
        let outlook = window_outlook(&pts, 50.0, Some(4 * 3600), Some(now), 3, false, now);
        assert!(!outlook.exhaust_before_reset);
        // Burn/projection still reported for display.
        assert!(outlook.minutes_to_exhaust.is_some());
    }

    #[test]
    fn reset_seconds_reanchored_to_now() {
        let now = Utc::now();
        let captured = now - chrono::Duration::seconds(120);
        let outlook = window_outlook(&[], 10.0, Some(600), Some(captured), 3, true, now);
        assert_eq!(outlook.reset_after_seconds, Some(480));
    }
}
