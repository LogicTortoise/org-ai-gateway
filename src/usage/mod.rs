use crate::prelude::*;
use crate::pool::storage::persist_all_accounts;
use crate::pool::storage::read_audit_records;
use crate::provider::account_identity_email;
use crate::provider::label_is_generic;
use crate::provider::claude::claude_account_refreshable;
use crate::provider::claude::refresh_claude_account_tokens;
use crate::provider::codex::codex_account_refreshable;
use crate::provider::codex::refresh_codex_account_tokens;
use crate::provider::cursor::cursor_account_expired;
use crate::provider::cursor::try_reimport_cursor_from_local;
use crate::retry::reset_backoff;
use crate::util::epoch_to_after_seconds;
use crate::util::http_client;
use crate::util::value_as_f64;
use crate::util::value_as_i64;
pub(crate) mod tokens;

pub(crate) fn parse_rate_limit_headers(headers: &HeaderMap) -> Option<RateLimitSnapshot> {
    fn s(h: &HeaderMap, k: &str) -> Option<String> {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
    }
    fn f(h: &HeaderMap, k: &str) -> Option<f64> {
        s(h, k).and_then(|v| v.parse().ok())
    }
    fn fp(h: &HeaderMap, keys: &[&str]) -> Option<f64> {
        for key in keys {
            if let Some(raw) = s(h, key) {
                if let Ok(v) = raw.trim_end_matches('%').trim().parse::<f64>() {
                    let pct = if v <= 1.0 { v * 100.0 } else { v };
                    return Some(pct.clamp(0.0, 100.0));
                }
            }
        }
        None
    }
    fn usage_from_remaining(h: &HeaderMap, remaining_key: &str, limit_key: &str) -> Option<f64> {
        let remaining = f(h, remaining_key)?;
        let limit = f(h, limit_key)?;
        if limit <= 0.0 {
            return None;
        }
        Some((((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0))
    }
    fn i(h: &HeaderMap, k: &str) -> Option<i64> {
        s(h, k).and_then(|v| v.parse().ok())
    }
    fn reset_after_seconds(h: &HeaderMap, keys: &[&str]) -> Option<i64> {
        let now = Utc::now().timestamp();
        for key in keys {
            if let Some(raw) = s(h, key) {
                if let Ok(v) = raw.parse::<i64>() {
                    if v > 0 {
                        // Epoch-looking values convert to a delta clamped at 0;
                        // a slightly-past epoch must NOT be returned verbatim
                        // (it would read as a ~50-year cooldown).
                        return Some(epoch_to_after_seconds(v));
                    }
                }
                if let Ok(dt) = DateTime::parse_from_rfc3339(&raw) {
                    let ts = dt.with_timezone(&Utc).timestamp();
                    return Some((ts - now).max(0));
                }
            }
        }
        None
    }
    fn b(h: &HeaderMap, k: &str) -> Option<bool> {
        s(h, k).map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
    }

    let codex = RateLimitSnapshot {
        active_limit: s(headers, "x-codex-active-limit"),
        plan_type: s(headers, "x-codex-plan-type"),
        primary_used_percent: f(headers, "x-codex-primary-used-percent"),
        primary_window_minutes: i(headers, "x-codex-primary-window-minutes"),
        primary_reset_after_seconds: i(headers, "x-codex-primary-reset-after-seconds"),
        secondary_used_percent: f(headers, "x-codex-secondary-used-percent"),
        secondary_window_minutes: i(headers, "x-codex-secondary-window-minutes"),
        secondary_reset_after_seconds: i(headers, "x-codex-secondary-reset-after-seconds"),
        credits_has_credits: b(headers, "x-codex-credits-has-credits"),
        credits_unlimited: b(headers, "x-codex-credits-unlimited"),
        credits_balance: s(headers, "x-codex-credits-balance"),
        captured_at: Some(Utc::now()),
    };

    if codex.primary_used_percent.is_some() || codex.plan_type.is_some() || codex.active_limit.is_some()
    {
        return Some(codex);
    }

    let claude_primary = fp(
        headers,
        &[
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-primary-utilization",
            "anthropic-ratelimit-unified-tokens-utilization",
            "anthropic-ratelimit-tokens-utilization",
        ],
    );
    let claude_secondary = fp(
        headers,
        &[
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-secondary-utilization",
            "anthropic-ratelimit-unified-requests-utilization",
            "anthropic-ratelimit-requests-utilization",
        ],
    );
    let claude_primary_reset = reset_after_seconds(
        headers,
        &[
            "anthropic-ratelimit-unified-primary-reset",
            "anthropic-ratelimit-unified-5h-reset",
            "anthropic-ratelimit-unified-tokens-reset",
            "anthropic-ratelimit-tokens-reset",
            "anthropic-ratelimit-unified-reset",
            "anthropic-ratelimit-requests-reset",
        ],
    );
    let claude_secondary_reset = reset_after_seconds(
        headers,
        &[
            "anthropic-ratelimit-unified-secondary-reset",
            "anthropic-ratelimit-unified-7d-reset",
            "anthropic-ratelimit-unified-requests-reset",
            "anthropic-ratelimit-requests-reset",
            "anthropic-ratelimit-unified-reset",
        ],
    );
    let claude = RateLimitSnapshot {
        active_limit: s(headers, "anthropic-ratelimit-unified-status")
            .or_else(|| s(headers, "anthropic-ratelimit-unified-5h-status")),
        plan_type: s(headers, "anthropic-ratelimit-tier").or_else(|| Some("claude".to_string())),
        primary_used_percent: claude_primary,
        primary_window_minutes: claude_primary.map(|_| 5 * 60),
        primary_reset_after_seconds: claude_primary_reset,
        secondary_used_percent: claude_secondary,
        secondary_window_minutes: claude_secondary.map(|_| 7 * 24 * 60),
        secondary_reset_after_seconds: claude_secondary_reset,
        credits_has_credits: None,
        credits_unlimited: None,
        credits_balance: None,
        captured_at: Some(Utc::now()),
    };

    if claude.primary_used_percent.is_some()
        || claude.secondary_used_percent.is_some()
        || claude.active_limit.is_some()
    {
        Some(claude)
    }

    else {
        let generic_primary = fp(
            headers,
            &[
                "x-ratelimit-requests-utilization",
                "x-ratelimit-utilization",
            ],
        )
        .or_else(|| usage_from_remaining(headers, "x-ratelimit-remaining-requests", "x-ratelimit-limit-requests"))
        .or_else(|| usage_from_remaining(headers, "x-ratelimit-requests-remaining", "x-ratelimit-requests-limit"))
        .or_else(|| usage_from_remaining(headers, "x-ratelimit-remaining", "x-ratelimit-limit"));
        let generic_secondary = fp(headers, &["x-ratelimit-tokens-utilization"])
            .or_else(|| usage_from_remaining(headers, "x-ratelimit-remaining-tokens", "x-ratelimit-limit-tokens"))
            .or_else(|| usage_from_remaining(headers, "x-ratelimit-tokens-remaining", "x-ratelimit-tokens-limit"));
        let generic = RateLimitSnapshot {
            active_limit: None,
            plan_type: Some("standard".to_string()),
            primary_used_percent: generic_primary,
            primary_window_minutes: generic_primary.map(|_| 60),
            primary_reset_after_seconds: reset_after_seconds(
                headers,
                &[
                    "x-ratelimit-reset-requests",
                    "x-ratelimit-requests-reset",
                    "x-ratelimit-reset",
                ],
            ),
            secondary_used_percent: generic_secondary,
            secondary_window_minutes: generic_secondary.map(|_| 24 * 60),
            secondary_reset_after_seconds: reset_after_seconds(
                headers,
                &[
                    "x-ratelimit-reset-tokens",
                    "x-ratelimit-tokens-reset",
                    "x-ratelimit-reset",
                ],
            ),
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            captured_at: Some(Utc::now()),
        };
        if generic.primary_used_percent.is_some() || generic.secondary_used_percent.is_some() {
            Some(generic)
        } else {
            None
        }
    }
}


pub(crate) fn synthesize_rate_limit_from_error(
    provider: &str,
    status: StatusCode,
    body: &str,
) -> Option<RateLimitSnapshot> {
    let lower = body.to_ascii_lowercase();
    let now = Utc::now();
    if provider == "claude"
        && (status == StatusCode::TOO_MANY_REQUESTS || lower.contains("rate_limit_error"))
    {
        return Some(RateLimitSnapshot {
            active_limit: Some("rejected".to_string()),
            plan_type: Some("claude".to_string()),
            // Claude error bodies often omit ratelimit headers on 429. The
            // 100%/100% snapshot is a fallback signal for the dashboard, but
            // note it ALSO feeds the scheduler's hard-exclude until the next
            // usage probe overwrites it (~3 min) — i.e. one 429 sidelines the
            // account in both windows for that interval, deliberately erring
            // toward protecting the account.
            primary_used_percent: Some(100.0),
            primary_window_minutes: Some(5 * 60),
            primary_reset_after_seconds: None,
            secondary_used_percent: Some(100.0),
            secondary_window_minutes: Some(7 * 24 * 60),
            secondary_reset_after_seconds: None,
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            captured_at: Some(now),
        });
    }
    if provider == "codex"
        && (status == StatusCode::UNAUTHORIZED
            || lower.contains("token_invalidated")
            || lower.contains("app_session_terminated"))
    {
        return Some(RateLimitSnapshot {
            active_limit: Some("auth_invalid".to_string()),
            plan_type: Some("codex".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_reset_after_seconds: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_reset_after_seconds: None,
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            captured_at: Some(now),
        });
    }
    None
}


pub(crate) async fn refresh_rate_limits_from_usage(state: &AppState) {
    let accounts = state.accounts.read().await.clone();
    let due: Vec<UpstreamAccount> = {
        let limits = state.rate_limits.read().await;
        accounts
            .into_iter()
            .filter(|account| {
                limits
                    .get(&account.id)
                    .and_then(|snap| snap.captured_at)
                    .is_none_or(|at| Utc::now() - at > chrono::Duration::minutes(3))
            })
            .collect()
    };
    if due.is_empty() {
        return;
    }
    // One upstream roundtrip per account — run them concurrently so the
    // dashboard isn't blocked for (accounts × latency).
    let fetches = due.into_iter().map(|account| async move {
        let snapshot = match account.provider.as_str() {
            "codex" => fetch_codex_usage_snapshot(&account).await,
            "claude" => fetch_claude_usage_snapshot(&account).await,
            "cursor" => fetch_cursor_usage_snapshot(&account).await,
            _ => None,
        };
        snapshot.map(|snap| (account.id, snap))
    });
    let results = futures_util::future::join_all(fetches).await;
    for (id, snap) in results.into_iter().flatten() {
        // Don't let a slow fetch clobber a newer snapshot captured from live
        // response headers while this fetch was in flight.
        let stale = {
            let limits = state.rate_limits.read().await;
            limits
                .get(&id)
                .and_then(|existing| existing.captured_at)
                .zip(snap.captured_at)
                .map(|(existing_at, new_at)| new_at < existing_at)
                .unwrap_or(false)
        };
        if !stale {
            crate::capacity::store_rate_limit(state, &id, snap).await;
        }
    }
}


pub(crate) async fn fetch_codex_usage_snapshot(account: &UpstreamAccount) -> Option<RateLimitSnapshot> {
    let bearer = account.bearer();
    if bearer.is_empty() {
        return None;
    }
    let client = http_client();
    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .bearer_auth(bearer)
        .header("Accept", "application/json");
    if !account.account_id.trim().is_empty() {
        req = req.header("ChatGPT-Account-ID", account.account_id.trim());
    }
    let resp = req.send().await.ok()?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return Some(RateLimitSnapshot {
            active_limit: Some("auth_invalid".to_string()),
            plan_type: Some("codex".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_reset_after_seconds: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_reset_after_seconds: None,
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            captured_at: Some(Utc::now()),
        });
    }
    if !status.is_success() {
        return None;
    }
    let value: Value = resp.json().await.ok()?;
    let rate_limit = value.get("rate_limit")?;
    let primary = rate_limit.get("primary_window");
    let secondary = rate_limit.get("secondary_window");
    // `used_percent` is ALWAYS a 0-100 percent (the Go original divides it by
    // 100 to get a ratio). Do NOT apply the ratio-or-percent header heuristic
    // here: it reads a genuinely low usage like 1.0 (= 1%) as a ratio and
    // blows it up to 100%, hard-excluding a nearly-idle account.
    let primary_used = primary
        .and_then(|v| v.get("used_percent"))
        .and_then(value_as_f64)
        .map(|v| v.clamp(0.0, 100.0));
    let secondary_used = secondary
        .and_then(|v| v.get("used_percent"))
        .and_then(value_as_f64)
        .map(|v| v.clamp(0.0, 100.0));
    if primary_used.is_none() && secondary_used.is_none() {
        return None;
    }
    Some(RateLimitSnapshot {
        active_limit: Some("ok".to_string()),
        plan_type: Some("codex".to_string()),
        primary_used_percent: primary_used,
        primary_window_minutes: Some(5 * 60),
        primary_reset_after_seconds: primary
            .and_then(|v| v.get("reset_at"))
            .and_then(value_as_i64)
            .map(epoch_to_after_seconds),
        secondary_used_percent: secondary_used,
        secondary_window_minutes: Some(7 * 24 * 60),
        secondary_reset_after_seconds: secondary
            .and_then(|v| v.get("reset_at"))
            .and_then(value_as_i64)
            .map(epoch_to_after_seconds),
        credits_has_credits: None,
        credits_unlimited: None,
        credits_balance: None,
        captured_at: Some(Utc::now()),
    })
}


pub(crate) async fn fetch_claude_usage_snapshot(account: &UpstreamAccount) -> Option<RateLimitSnapshot> {
    let token = account.access_token.trim();
    if token.is_empty() || !token.starts_with("sk-ant-oat") {
        return None;
    }
    let client = http_client();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        // 429 from the *usage-metadata* endpoint means we polled it too often —
        // NOT that the account's inference quota is exhausted. Return None so we
        // keep the last-good snapshot instead of clobbering it with a fake
        // 100%/100% (which both misled the dashboard and hard-excluded the
        // account). A real inference rate-limit surfaces as utilization=100 in a
        // successful response, or as a 429 on the actual /v1/messages call.
        return None;
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Some(RateLimitSnapshot {
            active_limit: Some("auth_invalid".to_string()),
            plan_type: Some("claude".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_reset_after_seconds: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_reset_after_seconds: None,
            credits_has_credits: None,
            credits_unlimited: None,
            credits_balance: None,
            captured_at: Some(Utc::now()),
        });
    }
    if !status.is_success() {
        return None;
    }
    let value: Value = resp.json().await.ok()?;
    let five_hour = value.get("five_hour");
    let seven_day = value
        .get("seven_day")
        .or_else(|| value.get("seven_day_sonnet"))
        .or_else(|| value.get("seven_day_opus"));
    // Like wham's `used_percent`, Anthropic's `utilization` is a 0-100 percent
    // (Go: `*payload.FiveHour.Utilization / 100.0`) — clamp only, no ratio
    // heuristic, or sub-1% usage reads as 100%.
    let primary_used = five_hour
        .and_then(|v| v.get("utilization"))
        .and_then(value_as_f64)
        .map(|v| v.clamp(0.0, 100.0));
    let secondary_used = seven_day
        .and_then(|v| v.get("utilization"))
        .and_then(value_as_f64)
        .map(|v| v.clamp(0.0, 100.0));
    if primary_used.is_none() && secondary_used.is_none() {
        return None;
    }
    // `resets_at` is an RFC3339 timestamp string (e.g.
    // "2026-06-11T16:40:00+00:00"), NOT an epoch int — parse it as such and
    // convert to seconds-from-now. (Using the numeric parser here silently
    // dropped it, so Claude cards showed "重置: —".)
    let reset_after = |w: Option<&Value>| -> Option<i64> {
        w.and_then(|v| v.get("resets_at"))
            .and_then(|r| r.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| (dt.with_timezone(&Utc) - Utc::now()).num_seconds().max(0))
    };
    Some(RateLimitSnapshot {
        active_limit: Some("ok".to_string()),
        plan_type: Some("claude".to_string()),
        primary_used_percent: primary_used,
        primary_window_minutes: Some(5 * 60),
        primary_reset_after_seconds: reset_after(five_hour),
        secondary_used_percent: secondary_used,
        secondary_window_minutes: Some(7 * 24 * 60),
        secondary_reset_after_seconds: reset_after(seven_day),
        credits_has_credits: None,
        credits_unlimited: None,
        credits_balance: None,
        captured_at: Some(Utc::now()),
    })
}

/// Cursor billing-cycle window in minutes (~30 days) — Cursor meters usage
/// monthly, not on Codex's 5h/weekly windows, so the dashboard shows a single
/// "30天" bar (the secondary window stays empty).
const CURSOR_CYCLE_WINDOW_MINUTES: i64 = 30 * 24 * 60;

/// Fetch a Cursor account's quota usage. Cursor has no public usage API; this
/// uses the same reverse-engineered endpoints the community usage trackers do
/// (`cursor.com/api/usage` for the legacy request-count model, falling back to
/// `GetCurrentPeriodUsage` for the newer USD-credit model). The single
/// month-to-date percentage is mapped onto the primary window; there is no
/// secondary window.
pub(crate) async fn fetch_cursor_usage_snapshot(account: &UpstreamAccount) -> Option<RateLimitSnapshot> {
    use crate::provider::cursor::{cursor_session_user_id, cursor_usage_cookie, normalize_token};

    let raw = account.access_token.trim();
    if raw.is_empty() {
        return None;
    }
    let user_id = cursor_session_user_id(raw)?;
    let jwt = normalize_token(raw);
    let client = http_client();

    // Legacy request-count model: `gpt-4.numRequests / maxRequestUsage`.
    let legacy = client
        .get(format!("https://cursor.com/api/usage?user={}", user_id))
        .header("Cookie", cursor_usage_cookie(&user_id, &jwt))
        .header("Accept", "application/json")
        .send()
        .await
        .ok();
    if let Some(resp) = legacy {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(snap) = cursor_snapshot_from_legacy(&v) {
                    return Some(snap);
                }
            }
        }
    }

    // USD-credit model fallback: `planUsage.totalPercentUsed` (or used/limit).
    let resp = client
        .post("https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage")
        .bearer_auth(&jwt)
        .header("Connect-Protocol-Version", "1")
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({}))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    cursor_snapshot_from_credit(&v)
}

/// Build a snapshot from the legacy `/api/usage` response. Returns `None` when
/// there's no request cap (the account is on the USD-credit model).
fn cursor_snapshot_from_legacy(v: &Value) -> Option<RateLimitSnapshot> {
    let gpt4 = v.get("gpt-4")?;
    let max = gpt4.get("maxRequestUsage").and_then(value_as_f64).filter(|m| *m > 0.0)?;
    let used = gpt4.get("numRequests").and_then(value_as_f64).unwrap_or(0.0);
    let percent = ((used / max) * 100.0).clamp(0.0, 100.0);

    // `startOfMonth` is an RFC3339 instant; the cycle resets a month later.
    let reset_after = v
        .get("startOfMonth")
        .and_then(|s| s.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|start| start.with_timezone(&Utc) + chrono::Duration::days(30))
        .map(|end| (end - Utc::now()).num_seconds().max(0));

    let balance = format!("请求 {}/{}", used as i64, max as i64);
    Some(cursor_snapshot(percent, reset_after, Some(balance)))
}

/// Build a snapshot from the USD-credit `GetCurrentPeriodUsage` response.
///
/// `planUsage.totalPercentUsed` is the **included-allowance** percentage (e.g.
/// the $20 plan credit), NOT a usability limit: team/on-demand accounts keep
/// working past 100% by paying for overage. We surface that percentage and the
/// dollar breakdown for display only — the scheduler does NOT hard-exclude
/// Cursor on it (see `provider == "cursor"` guards in `pool`); a genuine block
/// shows up as a failed request and cools the account down through the normal
/// failure path.
fn cursor_snapshot_from_credit(v: &Value) -> Option<RateLimitSnapshot> {
    let plan = v.get("planUsage")?;
    let percent = if let Some(pct) = plan.get("totalPercentUsed").and_then(value_as_f64) {
        pct.clamp(0.0, 100.0)
    } else {
        let limit = plan.get("limit").and_then(value_as_f64).filter(|l| *l > 0.0)?;
        let remaining = plan.get("remaining").and_then(value_as_f64).unwrap_or(0.0);
        (((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0)
    };

    // Dollar breakdown (cents -> USD) matching the Cursor dashboard's
    // "included usage" + "on-demand usage" figures.
    let usd = |cents: f64| format!("US${:.2}", cents / 100.0);
    let included_limit = plan.get("limit").and_then(value_as_f64);
    let included_used = plan.get("includedSpend").and_then(value_as_f64);
    let on_demand = v
        .pointer("/spendLimitUsage/individualUsed")
        .and_then(value_as_f64);
    let mut parts = Vec::new();
    if let (Some(used), Some(limit)) = (included_used, included_limit) {
        parts.push(format!("套餐内 {}/{}", usd(used.min(limit)), usd(limit)));
    }
    if let Some(od) = on_demand.filter(|v| *v > 0.0) {
        parts.push(format!("按量 {}", usd(od)));
    }
    let balance = (!parts.is_empty()).then(|| parts.join(" · "));

    // Billing-cycle reset from `billingCycleEnd` (ms epoch), matching the
    // dashboard's "Resets <date>".
    let reset_after = v
        .get("billingCycleEnd")
        .and_then(value_as_i64)
        .map(|ms| ((ms / 1000) - Utc::now().timestamp()).max(0));

    Some(cursor_snapshot(percent, reset_after, balance))
}

fn cursor_snapshot(
    primary_percent: f64,
    primary_reset_after_seconds: Option<i64>,
    credits_balance: Option<String>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        active_limit: Some("ok".to_string()),
        plan_type: Some("cursor".to_string()),
        primary_used_percent: Some(primary_percent),
        primary_window_minutes: Some(CURSOR_CYCLE_WINDOW_MINUTES),
        primary_reset_after_seconds,
        secondary_used_percent: None,
        secondary_window_minutes: None,
        secondary_reset_after_seconds: None,
        credits_has_credits: None,
        credits_unlimited: None,
        credits_balance,
        captured_at: Some(Utc::now()),
    }
}


/// Billable tokens for one audit record: the parsed `tokens.billable_tokens`
/// when present, otherwise (legacy success rows only) the char-length proxy.
fn audit_billable_tokens(record: &Value) -> u64 {
    if let Some(tokens) = record.get("tokens") {
        let billable = tokens.get("billable_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        if billable > 0 {
            return billable as u64;
        }
        let input = tokens.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let output = tokens.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        if input > 0 || output > 0 {
            return (input.max(0) + output.max(0)) as u64;
        }
    }
    if record.get("status").and_then(|v| v.as_str()) != Some("success") {
        return 0;
    }
    record.get("prompt_length").and_then(|v| v.as_u64()).unwrap_or(0)
        + record.get("output_length").and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Aggregate the last 7 days of audit records into a per-account owner/others
/// billable-token split (plus today's donated tokens for the
/// `daily_token_limit` cap), feeding the owner-heavy-usage guard.
pub(crate) fn compute_owner_usage_7d(
    records: &[Value],
    accounts: &[UpstreamAccount],
) -> HashMap<String, OwnerUsageStat> {
    let now = Utc::now();
    let today = now.date_naive();
    let cutoff = now - chrono::Duration::days(7);
    let owners: HashMap<&str, &str> = accounts
        .iter()
        .map(|a| (a.id.as_str(), a.owner_user_id.as_str()))
        .collect();
    let mut map: HashMap<String, OwnerUsageStat> = HashMap::new();
    for r in records {
        let Some(created_at) = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
        else {
            continue;
        };
        if created_at < cutoff {
            continue;
        }
        let account_id = r
            .get("upstream_account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if account_id.is_empty() {
            continue;
        }
        let billable = audit_billable_tokens(r);
        if billable == 0 {
            continue;
        }
        let requester = r.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
        // The pool's current owner wins over the historical record's owner
        // field, so ownership data stays consistent with selection.
        let owner = owners
            .get(account_id)
            .copied()
            .or_else(|| r.get("upstream_owner_user_id").and_then(|v| v.as_str()))
            .unwrap_or("");
        let stat = map.entry(account_id.to_string()).or_default();
        if !requester.is_empty() && requester == owner {
            stat.owner_billable += billable;
        } else {
            stat.others_billable += billable;
            if created_at.date_naive() == today {
                stat.usage_day = Some(today);
                stat.others_billable_today += billable;
            }
        }
        stat.computed_at = Some(now);
    }
    map
}

/// Aggregate the last 7 days of audit records into per-user BORROWED billable
/// tokens (requests served by accounts the user does not own). Usage of one's
/// own accounts is excluded by design: the per-user budgets exist to protect
/// the shared pool, not to ration an owner on their own subscription.
pub(crate) fn compute_user_usage_7d(
    records: &[Value],
    accounts: &[UpstreamAccount],
) -> HashMap<String, UserUsageStat> {
    let now = Utc::now();
    let today = now.date_naive();
    let cutoff = now - chrono::Duration::days(7);
    let owners: HashMap<&str, &str> = accounts
        .iter()
        .map(|a| (a.id.as_str(), a.owner_user_id.as_str()))
        .collect();
    let mut map: HashMap<String, UserUsageStat> = HashMap::new();
    for r in records {
        let Some(created_at) = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
        else {
            continue;
        };
        if created_at < cutoff {
            continue;
        }
        let requester = r.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
        if requester.is_empty() {
            continue;
        }
        let account_id = r
            .get("upstream_account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let owner = owners
            .get(account_id)
            .copied()
            .or_else(|| r.get("upstream_owner_user_id").and_then(|v| v.as_str()))
            .unwrap_or("");
        if requester == owner {
            continue;
        }
        let billable = audit_billable_tokens(r);
        if billable == 0 {
            continue;
        }
        let stat = map.entry(requester.to_string()).or_default();
        stat.weekly_billable += billable;
        if created_at.date_naive() == today {
            stat.day = Some(today);
            stat.daily_billable += billable;
        }
        stat.computed_at = Some(now);
    }
    map
}

/// Billable tokens for a fresh in-memory audit record — same precedence as
/// `audit_billable_tokens` reads back from disk (parsed billable, then
/// input+output, then the char-length proxy for token-less success rows like
/// cursor/relay), so live bumps and the 5-minute rebuild agree.
fn live_billable_tokens(record: &AuditRecord) -> u64 {
    if record.tokens.billable_tokens > 0 {
        return record.tokens.billable_tokens as u64;
    }
    let input = record.tokens.input_tokens.max(0);
    let output = record.tokens.output_tokens.max(0);
    if input > 0 || output > 0 {
        return (input + output) as u64;
    }
    if record.status != "success" {
        return 0;
    }
    (record.prompt_length + record.output_length) as u64
}

/// Live usage accounting, called once per audited request. Bumps the same
/// counters the 5-minute audit-log rebuild produces, so quota enforcement
/// (`daily_token_limit`, per-user budgets) sees consumption immediately
/// instead of up to one refresh interval late. The rebuild later overwrites
/// these maps from the audit log, keeping the two sources convergent.
pub(crate) async fn note_request_usage(state: &AppState, record: &AuditRecord) {
    let billable = live_billable_tokens(record);
    if billable == 0 {
        return;
    }
    let now = Utc::now();
    let today = now.date_naive();
    let is_owner =
        !record.user_id.is_empty() && record.user_id == record.upstream_owner_user_id;
    if !record.upstream_account_id.is_empty() {
        let mut owner_usage = state.owner_usage.write().await;
        let stat = owner_usage
            .entry(record.upstream_account_id.clone())
            .or_default();
        if is_owner {
            stat.owner_billable += billable;
        } else {
            stat.others_billable += billable;
            if stat.usage_day != Some(today) {
                stat.usage_day = Some(today);
                stat.others_billable_today = 0;
            }
            stat.others_billable_today += billable;
        }
        stat.computed_at = Some(now);
    }
    if !is_owner && !record.user_id.is_empty() {
        let mut user_usage = state.user_usage.write().await;
        let stat = user_usage.entry(record.user_id.clone()).or_default();
        if stat.day != Some(today) {
            stat.day = Some(today);
            stat.daily_billable = 0;
        }
        stat.daily_billable += billable;
        stat.weekly_billable += billable;
        stat.computed_at = Some(now);
    }
}

/// Background task: rebuild the 7-day owner/others usage split and the
/// per-user borrowed-usage ledger every 5 minutes (and once at startup) so the
/// owner-heavy guard and the quota checks work from fresh data.
pub(crate) async fn run_owner_usage_refresh(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
    loop {
        tick.tick().await;
        let records = read_audit_records(&state.audit_file).await;
        let accounts = state.accounts.read().await.clone();
        let owner_map = compute_owner_usage_7d(&records, &accounts);
        let user_map = compute_user_usage_7d(&records, &accounts);
        *state.owner_usage.write().await = owner_map;
        *state.user_usage.write().await = user_map;
    }
}

/// Background account-health heartbeat. Without it, a credential only reveals
/// itself as broken when a real user request happens to land on it (a first-hit
/// failure + retry). This loop probes each account's lightweight usage endpoint
/// on a timer so failing accounts are refreshed-or-quarantined *before* traffic
/// hits them, and recovered accounts are revived promptly.
///
/// Disabled by setting `GATEWAY_HEALTH_PROBE_SECS=0`; defaults to every 120s.
pub(crate) async fn run_account_health_probe(state: AppState) {
    let period = std::env::var("GATEWAY_HEALTH_PROBE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    if period == 0 {
        info!("account health probe disabled (GATEWAY_HEALTH_PROBE_SECS=0)");
        return;
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(period));
    loop {
        tick.tick().await;
        // Snapshot the accounts worth probing: skip administratively disabled
        // ones (intentionally parked) but DO probe dead ones so they can revive.
        let targets: Vec<UpstreamAccount> = {
            let accounts = state.accounts.read().await;
            accounts
                .iter()
                .filter(|a| !a.runtime.disabled)
                .filter(|a| matches!(a.provider.as_str(), "codex" | "claude" | "cursor"))
                .cloned()
                .collect()
        };
        for account in targets {
            backfill_account_identity(&state, &account).await;
            if account.provider == "cursor" {
                probe_cursor_account(&state, &account).await;
            } else {
                probe_one_account(&state, &account).await;
            }
        }
    }
}

/// Self-healing fallback for accounts connected before connect-time labeling
/// (or where that network resolve failed): if an account still carries the
/// generic default label, replace it with the real upstream account email and
/// persist. Runs each probe tick but no-ops once a real label is in place.
async fn backfill_account_identity(state: &AppState, account: &UpstreamAccount) {
    if !label_is_generic(&account.account_label, &account.owner_user_id, &account.provider) {
        return;
    }
    let Some(email) = account_identity_email(account).await else {
        return;
    };

    let changed = {
        let mut accounts = state.accounts.write().await;
        match accounts.iter_mut().find(|a| a.id == account.id) {
            Some(a) if label_is_generic(&a.account_label, &a.owner_user_id, &a.provider) => {
                a.account_label = email.clone();
                true
            }
            _ => false,
        }
    };
    if changed {
        if let Err(e) = persist_all_accounts(state).await {
            warn!("failed persisting backfilled identity for {}: {}", account.id, e);
        } else {
            info!("labeled {} account {} as {}", account.provider, account.id, email);
        }
    }
}

/// Cursor health: the session token is a bare JWT with no refresh endpoint, so
/// "health" is simply whether it has expired. Expired tokens are first given a
/// best-effort local re-import (single-host deploys), then quarantined as dead
/// so selection skips them and the dashboard flags them for re-import. A token
/// that's valid again revives a previously-dead account.
async fn probe_cursor_account(state: &AppState, account: &UpstreamAccount) {
    let now = Utc::now();
    if cursor_account_expired(account, now) {
        if try_reimport_cursor_from_local(state, account).await {
            info!(
                "health probe re-imported a fresh cursor token for {} from local Cursor",
                account.account_label
            );
            return;
        }
        let needs_mark = {
            let accounts = state.accounts.read().await;
            accounts
                .iter()
                .find(|a| a.id == account.id)
                .map(|a| !a.runtime.dead)
                .unwrap_or(false)
        };
        if needs_mark {
            {
                let mut accounts = state.accounts.write().await;
                if let Some(a) = accounts.iter_mut().find(|a| a.id == account.id) {
                    a.runtime.dead = true;
                    a.runtime.penalty += 100.0;
                }
            }
            if let Err(e) = persist_all_accounts(state).await {
                warn!("failed persisting dead flag for {}: {}", account.account_label, e);
            }
            warn!(
                "cursor session token for {} expired and could not be re-imported; \
                 marked dead — please re-import the WorkosCursorSessionToken",
                account.account_label
            );
        }
        return;
    }
    // Token still valid: revive if it had been quarantined.
    let was_dead = {
        let accounts = state.accounts.read().await;
        accounts
            .iter()
            .find(|a| a.id == account.id)
            .map(|a| a.runtime.dead)
            .unwrap_or(false)
    };
    if was_dead {
        {
            let mut accounts = state.accounts.write().await;
            if let Some(a) = accounts.iter_mut().find(|a| a.id == account.id) {
                a.runtime.dead = false;
                a.runtime.penalty = 0.0;
            }
        }
        reset_backoff(state, &account.id).await;
        if let Err(e) = persist_all_accounts(state).await {
            warn!("failed persisting revived cursor account {}: {}", account.account_label, e);
        }
        info!(
            "health probe revived previously-dead cursor account {}",
            account.account_label
        );
    }

    // Refresh the cached quota snapshot so the dashboard shows real usage even
    // before any traffic flows through the gateway.
    if let Some(snapshot) = fetch_cursor_usage_snapshot(account).await {
        crate::capacity::store_rate_limit(state, &account.id, snapshot).await;
    }
}

/// Probe one account, refresh its cached rate-limit snapshot, and apply the
/// resulting health transition. Conservative by design: a bad-auth signal only
/// triggers a refresh (which itself marks dead solely on an irrecoverable
/// `invalid_grant`), and a clean probe is what revives a previously-dead account.
async fn probe_one_account(state: &AppState, account: &UpstreamAccount) {
    let snapshot = match account.provider.as_str() {
        "codex" => fetch_codex_usage_snapshot(account).await,
        "claude" => fetch_claude_usage_snapshot(account).await,
        _ => None,
    };
    let Some(snapshot) = snapshot else {
        // No signal (network blip / provider with no usage endpoint) — leave
        // health untouched rather than guess.
        return;
    };
    let auth_invalid = snapshot
        .active_limit
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("auth_invalid"))
        .unwrap_or(false);

    crate::capacity::store_rate_limit(state, &account.id, snapshot).await;

    if auth_invalid {
        // Try to recover the credential. `refresh_*_account_tokens` revives on
        // success and marks the account dead only on a truly revoked token.
        let refreshable = match account.provider.as_str() {
            "codex" => codex_account_refreshable(account),
            "claude" => claude_account_refreshable(account),
            _ => false,
        };
        if !refreshable {
            return;
        }
        let result = match account.provider.as_str() {
            "codex" => refresh_codex_account_tokens(state, account).await,
            "claude" => refresh_claude_account_tokens(state, account).await,
            _ => return,
        };
        match result {
            Ok(_) => info!(
                "health probe refreshed credential for {} ({})",
                account.account_label, account.provider
            ),
            Err(e) => warn!(
                "health probe could not refresh {} ({}): {}",
                account.account_label, account.provider, e
            ),
        }
        return;
    }

    // Clean probe: if this account was previously quarantined as dead, the
    // credential is clearly working again — bring it back into rotation.
    let was_dead = {
        let accounts = state.accounts.read().await;
        accounts
            .iter()
            .find(|a| a.id == account.id)
            .map(|a| a.runtime.dead)
            .unwrap_or(false)
    };
    if was_dead {
        {
            let mut accounts = state.accounts.write().await;
            if let Some(a) = accounts.iter_mut().find(|a| a.id == account.id) {
                a.runtime.dead = false;
                a.runtime.penalty = 0.0;
            }
        }
        reset_backoff(state, &account.id).await;
        if let Err(e) = persist_all_accounts(state).await {
            warn!("failed persisting revived account {}: {}", account.account_label, e);
        }
        info!(
            "health probe revived previously-dead account {} ({})",
            account.account_label, account.provider
        );
    }
}

pub(crate) fn short_status(raw: &str) -> String {
    if raw == "success" {
        return "success".to_string();
    }
    if raw.starts_with("upstream_error_") {
        return raw.to_string();
    }
    if raw.starts_with("upstream_error:") {
        return "upstream_error".to_string();
    }
    if raw.trim().is_empty() {
        return "unknown".to_string();
    }
    "error".to_string()
}


#[cfg(test)]
mod usage_ledger_tests {
    use super::*;

    fn record(user: &str, account: &str, owner: &str, billable: i64, created_at: DateTime<Utc>) -> Value {
        json!({
            "user_id": user,
            "upstream_account_id": account,
            "upstream_owner_user_id": owner,
            "status": "success",
            "created_at": created_at.to_rfc3339(),
            "tokens": { "billable_tokens": billable },
        })
    }

    #[test]
    fn user_usage_counts_borrowed_only() {
        let now = Utc::now();
        let records = vec![
            // bob borrowing koltyu's account: counts.
            record("bob", "a1", "koltyu", 100, now),
            // bob on his own account: excluded.
            record("bob", "b1", "bob", 999, now),
            // borrowed 3 days ago: weekly only, not daily.
            record("bob", "a1", "koltyu", 50, now - chrono::Duration::days(3)),
            // outside the 7d window: ignored entirely.
            record("bob", "a1", "koltyu", 1000, now - chrono::Duration::days(8)),
        ];
        let map = compute_user_usage_7d(&records, &[]);
        let stat = map.get("bob").expect("bob has borrowed usage");
        assert_eq!(stat.weekly_billable, 150);
        assert_eq!(stat.daily_billable_on(now.date_naive()), 100);
    }

    #[test]
    fn owner_usage_tracks_todays_donated_tokens() {
        let now = Utc::now();
        let records = vec![
            record("koltyu", "a1", "koltyu", 500, now),                            // owner: not donated
            record("bob", "a1", "koltyu", 100, now),                              // donated today
            record("carol", "a1", "koltyu", 70, now - chrono::Duration::days(2)), // donated, not today
        ];
        let map = compute_owner_usage_7d(&records, &[]);
        let stat = map.get("a1").expect("a1 has usage");
        assert_eq!(stat.owner_billable, 500);
        assert_eq!(stat.others_billable, 170);
        assert_eq!(stat.others_billable_on(now.date_naive()), 100);
    }
}

#[cfg(test)]
mod cursor_usage_tests {
    use super::*;

    #[test]
    fn legacy_request_count_maps_to_percent() {
        let v = json!({
            "gpt-4": { "numRequests": 25, "maxRequestUsage": 500 },
            "startOfMonth": "2026-06-01T00:00:00Z"
        });
        let snap = cursor_snapshot_from_legacy(&v).expect("snapshot");
        assert_eq!(snap.primary_used_percent, Some(5.0)); // 25/500
        assert_eq!(snap.secondary_used_percent, None);
        assert_eq!(snap.primary_window_minutes, Some(CURSOR_CYCLE_WINDOW_MINUTES));
    }

    #[test]
    fn legacy_without_request_cap_falls_through() {
        // USD-credit accounts report a null cap — must return None so the
        // caller tries GetCurrentPeriodUsage instead.
        let v = json!({ "gpt-4": { "numRequests": 0, "maxRequestUsage": null } });
        assert!(cursor_snapshot_from_legacy(&v).is_none());
    }

    #[test]
    fn credit_model_prefers_total_percent() {
        let v = json!({ "planUsage": { "limit": 2000, "remaining": 1500, "totalPercentUsed": 23 } });
        let snap = cursor_snapshot_from_credit(&v).expect("snapshot");
        assert_eq!(snap.primary_used_percent, Some(23.0));
    }

    #[test]
    fn credit_model_derives_percent_from_limit_remaining() {
        let v = json!({ "planUsage": { "limit": 2000, "remaining": 1500 } });
        let snap = cursor_snapshot_from_credit(&v).expect("snapshot");
        assert_eq!(snap.primary_used_percent, Some(25.0)); // (2000-1500)/2000
    }
}
