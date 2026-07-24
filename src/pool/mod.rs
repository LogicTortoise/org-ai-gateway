use crate::prelude::*;
use crate::retry::eligible_accounts;
pub(crate) mod storage;

/// Hard exclusion thresholds shared by affinity checks and selection: an
/// account at >=95% of its primary (5h) window or >=99% of its secondary
/// (weekly) window is skipped. Defined once — `account_eligible_for_affinity`
/// and `select_account_for_request` must agree or sticky sessions would pin to
/// accounts the selector refuses.
const PRIMARY_HARD_EXCLUDE_PERCENT: f64 = 95.0;
const SECONDARY_HARD_EXCLUDE_PERCENT: f64 = 99.0;

/// Whether a provider's usage percentage represents a true rate-limit window
/// that should hard-exclude / cool down an account at high utilization.
///
/// Cursor and Ollama are the exceptions:
/// - Cursor's percentage is the *included spend allowance* (e.g. the $20 plan
///   credit), not a usability cap — team/on-demand accounts keep working past
///   100% by paying for overage. A genuine block surfaces as a failed request
///   and cools the account down through the normal failure path instead.
/// - Ollama is a local, non-metered upstream: it has no rate-limit windows at
///   all (and never carries a snapshot), so usage percent must never gate it.
pub(crate) fn usage_percent_gates_selection(provider: &str) -> bool {
    !matches!(provider, "cursor" | "ollama" | "glm" | "kimi")
}

/// Sticky-session rebalance thresholds: a bound account is "pressured" when
/// its primary window passes this percent (or the weekly window the second
/// one), or the capacity model projects a wall within
/// `AFFINITY_MIGRATE_EXHAUST_MINUTES`. Migration additionally requires a peer
/// whose worst window sits at least `AFFINITY_MIGRATE_HEADROOM_GAP` points
/// lower — losing the upstream prompt cache costs real tokens, so a marginal
/// improvement is not worth the move.
const AFFINITY_MIGRATE_PRIMARY_PERCENT: f64 = 85.0;
const AFFINITY_MIGRATE_SECONDARY_PERCENT: f64 = 97.0;
const AFFINITY_MIGRATE_HEADROOM_GAP: f64 = 20.0;
const AFFINITY_MIGRATE_EXHAUST_MINUTES: i64 = 15;

/// Whether an `active_limit` status string says the account is currently not
/// serving (rejected / rate-limited / bad auth). Substring match on purpose:
/// upstreams send variants ("rejected", "rate_limited", "usage_limit", ...)
/// and erring toward exclusion just rotates to another account.
fn active_limit_blocked(active: &str) -> bool {
    let active = active.trim().to_ascii_lowercase();
    active.contains("auth_invalid") || active.contains("reject") || active.contains("limit")
}

pub(crate) fn normalize_sticky_key(raw: &str) -> Option<String> {
    let key = raw.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}


pub(crate) fn websocket_session_key(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    query
        .get("session_id")
        .and_then(|v| normalize_sticky_key(v))
        .or_else(|| query.get("conversation_id").and_then(|v| normalize_sticky_key(v)))
        .or_else(|| {
            headers
                .get("x-codex-session-id")
                .and_then(|v| v.to_str().ok())
                .and_then(normalize_sticky_key)
        })
}


pub(crate) fn transient_prompt_cache_key(payload: &Value) -> Option<String> {
    let obj = payload.as_object()?;
    if let Some(explicit) = obj
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .and_then(normalize_sticky_key)
    {
        return Some(explicit);
    }
    if let Some(explicit) = obj
        .get("metadata")
        .and_then(|v| v.get("prompt_cache_key"))
        .and_then(|v| v.as_str())
        .and_then(normalize_sticky_key)
    {
        return Some(explicit);
    }
    // Claude Code sends metadata.user_id with an embedded per-session uuid —
    // stable across every turn of one conversation, the ideal affinity key.
    // (The gateway's own fingerprint injection happens after selection, so
    // this always sees the client's original value.)
    if let Some(session) = obj
        .get("metadata")
        .and_then(|v| v.get("user_id"))
        .and_then(|v| v.as_str())
        .and_then(normalize_sticky_key)
    {
        return Some(format!("meta:{}", session));
    }
    // Fallback: fingerprint the conversation PREFIX, not the full transcript.
    // The transcript grows every turn, so hashing all of it yields a fresh key
    // per turn and the sticky binding never matches — follow-up turns would
    // re-select an account and forfeit the upstream KV/prompt cache. The
    // prefix (system/instructions + the opening message, skipping a leading
    // system-role entry) is identical for every turn of one conversation.
    let model = obj.get("model").and_then(|v| v.as_str()).unwrap_or_default();
    let mut fingerprint = String::new();
    fingerprint.push_str(model.trim());
    for context_key in ["system", "instructions"] {
        if let Some(v) = obj.get(context_key) {
            fingerprint.push('|');
            fingerprint.push_str(&serde_json::to_string(v).unwrap_or_default());
        }
    }
    let transcript = obj.get("input").or_else(|| obj.get("messages"))?;
    match transcript {
        Value::Array(items) => {
            let mut prefix = items.iter().take(2).collect::<Vec<_>>();
            // Keep [system, first-real-message] when the transcript embeds the
            // system prompt; otherwise the first message alone identifies the
            // conversation (turn 1 may not have a second element yet).
            let first_is_system = prefix
                .first()
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
                .map(|r| r == "system" || r == "developer")
                .unwrap_or(false);
            if !first_is_system {
                prefix.truncate(1);
            }
            if prefix.is_empty() {
                return None;
            }
            for item in prefix {
                fingerprint.push('|');
                fingerprint.push_str(&serde_json::to_string(item).unwrap_or_default());
            }
        }
        other => {
            fingerprint.push('|');
            fingerprint.push_str(&serde_json::to_string(other).unwrap_or_default());
        }
    }
    if fingerprint.trim().is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    fingerprint.hash(&mut hasher);
    Some(format!("auto:{:016x}", hasher.finish()))
}


#[cfg(test)]
mod affinity_key_tests {
    use super::*;

    #[test]
    fn key_is_stable_across_turns_anthropic_shape() {
        let turn1 = json!({
            "model": "claude-sonnet-4-6",
            "system": "you are helpful",
            "messages": [{"role": "user", "content": "第一条消息"}]
        });
        let turn2 = json!({
            "model": "claude-sonnet-4-6",
            "system": "you are helpful",
            "messages": [
                {"role": "user", "content": "第一条消息"},
                {"role": "assistant", "content": "回复"},
                {"role": "user", "content": "第二条消息"}
            ]
        });
        assert_eq!(
            transient_prompt_cache_key(&turn1),
            transient_prompt_cache_key(&turn2)
        );
    }

    #[test]
    fn key_is_stable_across_turns_openai_shape() {
        let turn1 = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"}
            ]
        });
        let turn2 = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "more"}
            ]
        });
        assert_eq!(
            transient_prompt_cache_key(&turn1),
            transient_prompt_cache_key(&turn2)
        );
    }

    #[test]
    fn different_conversations_get_different_keys() {
        let a = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "对话A"}]
        });
        let b = json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "对话B"}]
        });
        assert_ne!(transient_prompt_cache_key(&a), transient_prompt_cache_key(&b));
    }

    #[test]
    fn explicit_keys_win_over_fingerprint() {
        let with_pck = json!({
            "model": "gpt-5",
            "prompt_cache_key": "conv-42",
            "input": [{"role": "user", "content": "x"}]
        });
        assert_eq!(transient_prompt_cache_key(&with_pck).as_deref(), Some("conv-42"));
        let with_meta = json!({
            "model": "claude-sonnet-4-6",
            "metadata": {"user_id": "user_abc_session_def"},
            "messages": [{"role": "user", "content": "x"}]
        });
        assert_eq!(
            transient_prompt_cache_key(&with_meta).as_deref(),
            Some("meta:user_abc_session_def")
        );
    }
}

pub(crate) async fn resolve_affinity_account(
    bindings: &Arc<RwLock<HashMap<String, StickyAccountBinding>>>,
    key: &str,
    user_id: &str,
    provider: &str,
) -> Option<String> {
    let key = normalize_sticky_key(key)?;
    let now = Utc::now();
    let mut guard = bindings.write().await;
    guard.retain(|_, binding| binding.expires_at > now);
    if let Some(binding) = guard.get(&key) {
        if binding.user_id == user_id && binding.provider == provider && binding.expires_at > now {
            return Some(binding.account_id.clone());
        }
    }
    None
}


pub(crate) async fn remember_affinity_account(
    bindings: &Arc<RwLock<HashMap<String, StickyAccountBinding>>>,
    key: String,
    account_id: &str,
    provider: &str,
    user_id: &str,
    ttl_secs: i64,
) {
    let Some(key) = normalize_sticky_key(&key) else {
        return;
    };
    let now = Utc::now();
    let mut guard = bindings.write().await;
    guard.retain(|_, binding| binding.expires_at > now);
    guard.insert(
        key,
        StickyAccountBinding {
            account_id: account_id.to_string(),
            provider: provider.to_string(),
            user_id: user_id.to_string(),
            expires_at: now + chrono::Duration::seconds(ttl_secs.max(1)),
        },
    );
}


/// One-shot healthy selection for entry points without their own retry loop
/// (WebSocket, relay, models APIs): dead/disabled accounts are always excluded;
/// accounts inside their cooldown window are only used when nothing warm exists.
/// `proxy_provider` keeps its own loop because it also tracks per-attempt
/// exclusions.
pub(crate) async fn select_healthy_account(
    state: &AppState,
    provider: &str,
    user_id: &str,
    preferred_account_id: Option<&str>,
    owned_only: bool,
    shared_only: bool,
) -> Option<UpstreamAccount> {
    let now = Utc::now();
    let excluded = std::collections::HashSet::new();
    let selected = {
        let accounts = state.accounts.read().await;
        let rate_limits = state.rate_limits.read().await;
        let owner_usage = state.owner_usage.read().await;
        let outlooks = state.capacity_outlooks.read().await;
        let mut warm = eligible_accounts(&accounts, provider, user_id, &excluded, now, false);
        if owned_only {
            warm.retain(|a| a.owner_user_id == user_id);
        }
        // Untrusted callers may only ever touch shared accounts, never an
        // owner-matched private one (see `Caller::owner_trusted`).
        if shared_only {
            warm.retain(|a| a.share_enabled);
        }
        let sel = select_account_for_request_with_preference(
            &warm,
            user_id,
            provider,
            &rate_limits,
            &owner_usage,
            &outlooks,
            preferred_account_id,
        );
        if sel.is_some() {
            sel
        } else {
            let mut cooling = eligible_accounts(&accounts, provider, user_id, &excluded, now, true);
            if owned_only {
                cooling.retain(|a| a.owner_user_id == user_id);
            }
            if shared_only {
                cooling.retain(|a| a.share_enabled);
            }
            let cooling = crate::retry::prefer_near_expiry(cooling, now);
            select_account_for_request_with_preference(
                &cooling,
                user_id,
                provider,
                &rate_limits,
                &owner_usage,
                &outlooks,
                preferred_account_id,
            )
        }
    };
    if let Some(account) = &selected {
        note_account_pick(state, &account.id).await;
    }
    selected
}

/// Record one selection on the account so subsequent picks within the same
/// rate-limit snapshot window spread to comparable peers instead of piling on.
pub(crate) async fn note_account_pick(state: &AppState, account_id: &str) {
    let mut accounts = state.accounts.write().await;
    if let Some(a) = accounts.iter_mut().find(|a| a.id == account_id) {
        a.runtime.recent_picks += 1.0;
    }
}


pub(crate) fn select_account_for_request_with_preference(
    accounts: &[UpstreamAccount],
    request_user_id: &str,
    provider: &str,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
    outlooks: &HashMap<String, crate::capacity::AccountOutlook>,
    preferred_account_id: Option<&str>,
) -> Option<UpstreamAccount> {
    if let Some(account_id) = preferred_account_id {
        if let Some(account) = accounts.iter().find(|a| a.id == account_id) {
            if account.provider == provider
                && account_visible_to_user(account, request_user_id)
                && account_open_to_requester(account, request_user_id, rate_limits, owner_usage)
                && account_eligible_for_affinity(provider, account, rate_limits)
                && !should_rebalance_affinity(
                    provider,
                    account,
                    accounts,
                    request_user_id,
                    rate_limits,
                    owner_usage,
                    outlooks,
                )
            {
                return Some(account.clone());
            }
        }
    }
    select_account_for_request(accounts, request_user_id, provider, rate_limits, owner_usage)
}

/// The "worst window" pressure of an account: max of its primary/secondary
/// used percents (0 with no snapshot).
fn account_pressure_percent(
    account: &UpstreamAccount,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
) -> f64 {
    rate_limits
        .get(&account.id)
        .map(|s| {
            s.primary_used_percent
                .unwrap_or(0.0)
                .max(s.secondary_used_percent.unwrap_or(0.0))
                .clamp(0.0, 100.0)
        })
        .unwrap_or(0.0)
}

/// Affinity-vs-headroom tradeoff: keeping a sticky session on its bound
/// account preserves the upstream prompt cache, so by default it stays. But a
/// binding that rides the account all the way into the 95% hard-exclude wall
/// ends with a forced, badly-timed swap anyway — so when the bound account is
/// already near its limits (or the capacity model projects it hitting 100%
/// before reset within minutes) AND a clearly fresher peer is available,
/// migrate proactively. The new account is rebound on the next success, making
/// the move permanent for the rest of the session.
pub(crate) fn should_rebalance_affinity(
    provider: &str,
    preferred: &UpstreamAccount,
    accounts: &[UpstreamAccount],
    request_user_id: &str,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
    outlooks: &HashMap<String, crate::capacity::AccountOutlook>,
) -> bool {
    // Cursor's percent is a spend allowance, not a wall — never migrate on it.
    if !usage_percent_gates_selection(provider) {
        return false;
    }
    let Some(snapshot) = rate_limits.get(&preferred.id) else {
        return false;
    };
    let primary = snapshot.primary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let secondary = snapshot.secondary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let exhaust_soon = outlooks
        .get(&preferred.id)
        .map(|o| o.primary_exhaust_within(AFFINITY_MIGRATE_EXHAUST_MINUTES))
        .unwrap_or(false);
    let pressured = primary >= AFFINITY_MIGRATE_PRIMARY_PERCENT
        || secondary >= AFFINITY_MIGRATE_SECONDARY_PERCENT
        || exhaust_soon;
    if !pressured {
        return false;
    }
    let preferred_pressure = account_pressure_percent(preferred, rate_limits);
    accounts.iter().any(|candidate| {
        candidate.id != preferred.id
            && candidate.provider == provider
            && !candidate.runtime.dead
            && !candidate.runtime.disabled
            && account_visible_to_user(candidate, request_user_id)
            && account_open_to_requester(candidate, request_user_id, rate_limits, owner_usage)
            && account_eligible_for_affinity(provider, candidate, rate_limits)
            && account_pressure_percent(candidate, rate_limits)
                <= preferred_pressure - AFFINITY_MIGRATE_HEADROOM_GAP
    })
}


pub(crate) fn account_visible_to_user(account: &UpstreamAccount, request_user_id: &str) -> bool {
    account.owner_user_id == request_user_id || account.share_enabled
}

/// Owner-heavy-usage protection, configured once from the environment. Donated
/// accounts are meant to be shared, so this guard is **off by default**: every
/// shared account is fully available to non-owners regardless of who has been
/// using it. It exists only as an opt-in knob — set `GATEWAY_OWNER_PROTECTION=on`
/// to reserve a shared account for its owner once its weekly window is high AND
/// the owner produced most of the last 7 days' tokens, and tune the two
/// thresholds to control how aggressively that reservation kicks in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnerProtectionConfig {
    /// `GATEWAY_OWNER_PROTECTION`: master switch. Off by default; `1`/`on`/`true`/`yes` enables.
    pub(crate) enabled: bool,
    /// `GATEWAY_OWNER_PROTECT_USAGE_PERCENT`: weekly-window usage above which reservation may kick in.
    pub(crate) usage_percent: f64,
    /// `GATEWAY_OWNER_PROTECT_OWNER_SHARE`: owner's minimum share (0..1) of last-7-days billable tokens.
    pub(crate) owner_share: f64,
}

impl Default for OwnerProtectionConfig {
    fn default() -> Self {
        Self { enabled: false, usage_percent: 60.0, owner_share: 0.5 }
    }
}

impl OwnerProtectionConfig {
    fn from_env() -> Self {
        let d = Self::default();
        let enabled = std::env::var("GATEWAY_OWNER_PROTECTION")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
            .unwrap_or(d.enabled);
        fn num(name: &str, default: f64) -> f64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(default)
        }
        Self {
            enabled,
            usage_percent: num("GATEWAY_OWNER_PROTECT_USAGE_PERCENT", d.usage_percent),
            owner_share: num("GATEWAY_OWNER_PROTECT_OWNER_SHARE", d.owner_share),
        }
    }
}

pub(crate) fn owner_protection_config() -> &'static OwnerProtectionConfig {
    static CONFIG: std::sync::OnceLock<OwnerProtectionConfig> = std::sync::OnceLock::new();
    CONFIG.get_or_init(OwnerProtectionConfig::from_env)
}

pub(crate) fn log_startup() {
    let cfg = owner_protection_config();
    if cfg.enabled {
        info!(
            "owner-heavy-usage protection enabled: usage_percent={} owner_share={}",
            cfg.usage_percent, cfg.owner_share
        );
    } else {
        info!("owner-heavy-usage protection DISABLED (GATEWAY_OWNER_PROTECTION=off)");
    }
}

/// The owner-configured share cap, in percent. None / out-of-range => 100.
pub(crate) fn effective_share_limit(account: &UpstreamAccount) -> f64 {
    account
        .share_limit_percent
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 100.0))
        .unwrap_or(100.0)
}

/// Whether non-owner traffic is still allowed under the owner's share cap:
/// both usage windows must sit below `share_limit_percent`.
pub(crate) fn within_share_limit(
    account: &UpstreamAccount,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
) -> bool {
    let limit = effective_share_limit(account);
    if limit >= 100.0 {
        return true;
    }
    let Some(snapshot) = rate_limits.get(&account.id) else {
        // No usage data yet — be conservative only for a zero cap.
        return limit > 0.0;
    };
    let primary = snapshot.primary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let secondary = snapshot.secondary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    primary < limit && secondary < limit
}

/// Whether non-owner traffic is still allowed under the owner's daily donation
/// cap (`daily_token_limit`): billable tokens served to NON-owners today (UTC)
/// must stay below the cap. The owner is never capped on their own account,
/// mirroring `within_share_limit`. Counts come from the audit log (5-minute
/// rebuild) plus live per-request bumps, so concurrent long generations can
/// overshoot by at most the requests already in flight.
pub(crate) fn within_daily_token_limit(
    account: &UpstreamAccount,
    owner_usage: &HashMap<String, OwnerUsageStat>,
) -> bool {
    let Some(limit) = account.daily_token_limit.filter(|l| *l > 0) else {
        return true;
    };
    let Some(stat) = owner_usage.get(&account.id) else {
        return true;
    };
    stat.others_billable_on(Utc::now().date_naive()) < limit
}

/// Owner-heavy-usage guard: when the weekly window is already high AND the
/// owner produced most of the last 7 days' billable tokens, the owner clearly
/// needs the remaining quota themselves — stop routing other users here.
pub(crate) fn owner_needs_protection(
    account: &UpstreamAccount,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
) -> bool {
    owner_needs_protection_with(owner_protection_config(), account, rate_limits, owner_usage)
}

fn owner_needs_protection_with(
    cfg: &OwnerProtectionConfig,
    account: &UpstreamAccount,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
) -> bool {
    if !cfg.enabled {
        return false;
    }
    let Some(snapshot) = rate_limits.get(&account.id) else {
        return false;
    };
    if snapshot.secondary_used_percent.unwrap_or(0.0) < cfg.usage_percent {
        return false;
    }
    let Some(stat) = owner_usage.get(&account.id) else {
        return false;
    };
    let total = stat.owner_billable + stat.others_billable;
    if total == 0 {
        return false;
    }
    (stat.owner_billable as f64) / (total as f64) >= cfg.owner_share
}

/// Combined gate for using an account on behalf of `request_user_id`. The
/// owner always passes; everyone else must respect the share cap and the
/// owner-heavy-usage reservation.
pub(crate) fn account_open_to_requester(
    account: &UpstreamAccount,
    request_user_id: &str,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
) -> bool {
    if account.owner_user_id == request_user_id {
        return true;
    }
    if !account.share_enabled {
        return false;
    }
    within_share_limit(account, rate_limits)
        && within_daily_token_limit(account, owner_usage)
        && !owner_needs_protection(account, rate_limits, owner_usage)
}


pub(crate) fn account_eligible_for_affinity(
    provider: &str,
    account: &UpstreamAccount,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
) -> bool {
    let Some(snapshot) = rate_limits.get(&account.id) else {
        return true;
    };
    let primary = snapshot.primary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let secondary = snapshot.secondary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    if usage_percent_gates_selection(provider)
        && (primary >= PRIMARY_HARD_EXCLUDE_PERCENT || secondary >= SECONDARY_HARD_EXCLUDE_PERCENT)
    {
        return false;
    }
    if provider != "codex"
        && active_limit_blocked(snapshot.active_limit.as_deref().unwrap_or_default())
    {
        return false;
    }
    true
}


#[cfg(test)]
mod share_policy_tests {
    use super::*;

    fn account(id: &str, owner: &str, share: bool, limit: Option<f64>) -> UpstreamAccount {
        UpstreamAccount {
            id: id.to_string(),
            owner_user_id: owner.to_string(),
            provider: "claude".to_string(),
            account_label: id.to_string(),
            access_token: "tok".to_string(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            api_key: String::new(),
            base_url: String::new(),
            base_url_alt: String::new(),
            share_enabled: share,
            share_limit_percent: limit,
            daily_token_limit: None,
            created_at: Utc::now(),
            runtime: AccountRuntime::default(),
        }
    }

    fn snapshot(primary: f64, secondary: f64) -> RateLimitSnapshot {
        RateLimitSnapshot {
            primary_used_percent: Some(primary),
            secondary_used_percent: Some(secondary),
            ..RateLimitSnapshot::default()
        }
    }

    #[test]
    fn cursor_at_100_percent_is_still_selectable() {
        // 100% included-spend must NOT exclude a Cursor account (on-demand keeps
        // it usable), whereas the same number sidelines a Codex/Claude account.
        let mut a = account("c1", "alice", true, None);
        a.provider = "cursor".to_string();
        let mut limits = HashMap::new();
        limits.insert("c1".to_string(), snapshot(100.0, 100.0));
        let accounts = vec![a];
        let picked = select_account_for_request(&accounts, "alice", "cursor", &limits, &HashMap::new());
        assert_eq!(picked.map(|p| p.id), Some("c1".to_string()));
        assert!(account_eligible_for_affinity("cursor", &accounts[0], &limits));

        // Sanity: a codex account at 100% IS excluded.
        let mut codex = account("x1", "alice", true, None);
        codex.provider = "codex".to_string();
        let mut climits = HashMap::new();
        climits.insert("x1".to_string(), snapshot(100.0, 100.0));
        assert!(!account_eligible_for_affinity("codex", &codex, &climits));
    }

    #[test]
    fn share_cap_blocks_non_owner_but_never_owner() {
        let a = account("a1", "alice", true, Some(60.0));
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(10.0, 65.0));
        let usage = HashMap::new();
        assert!(!account_open_to_requester(&a, "bob", &limits, &usage));
        assert!(account_open_to_requester(&a, "alice", &limits, &usage));
        // Below the cap it opens back up.
        limits.insert("a1".to_string(), snapshot(10.0, 30.0));
        assert!(account_open_to_requester(&a, "bob", &limits, &usage));
    }

    #[test]
    fn no_cap_means_shared_up_to_hard_limits() {
        let a = account("a1", "alice", true, None);
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(90.0, 90.0));
        assert!(account_open_to_requester(&a, "bob", &limits, &HashMap::new()));
    }

    #[test]
    fn owner_heavy_guard_is_off_by_default() {
        // Donated accounts stay shared: even under owner-heavy usage the default
        // (env-unset) config never reserves, and non-owners keep borrowing.
        let a = account("a1", "alice", true, None);
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(10.0, 70.0));
        let mut usage = HashMap::new();
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat { owner_billable: 800, others_billable: 200, computed_at: Some(Utc::now()), ..OwnerUsageStat::default() },
        );
        assert!(!owner_needs_protection(&a, &limits, &usage));
        assert!(account_open_to_requester(&a, "bob", &limits, &usage));
    }

    #[test]
    fn owner_heavy_guard_reserves_account_when_enabled() {
        // Opt-in via config: high weekly window + owner-produced majority.
        let cfg = OwnerProtectionConfig { enabled: true, usage_percent: 60.0, owner_share: 0.5 };
        let a = account("a1", "alice", true, None);
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(10.0, 70.0));
        let mut usage = HashMap::new();
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat { owner_billable: 800, others_billable: 200, computed_at: Some(Utc::now()), ..OwnerUsageStat::default() },
        );
        // Weekly window high + owner produced 80% of it => protected.
        assert!(owner_needs_protection_with(&cfg, &a, &limits, &usage));
        // Same inputs, guard disabled => never reserves.
        assert!(!owner_needs_protection_with(&OwnerProtectionConfig { enabled: false, ..cfg }, &a, &limits, &usage));
        // Mostly consumed by others => not the owner's problem, keep sharing.
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat { owner_billable: 200, others_billable: 800, computed_at: Some(Utc::now()), ..OwnerUsageStat::default() },
        );
        assert!(!owner_needs_protection_with(&cfg, &a, &limits, &usage));
        // Low weekly usage => no protection regardless of split.
        limits.insert("a1".to_string(), snapshot(10.0, 30.0));
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat { owner_billable: 800, others_billable: 200, computed_at: Some(Utc::now()), ..OwnerUsageStat::default() },
        );
        assert!(!owner_needs_protection_with(&cfg, &a, &limits, &usage));
    }

    #[test]
    fn selection_respects_cap_even_as_last_resort() {
        let a = account("a1", "alice", true, Some(50.0));
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(96.0, 60.0));
        let accounts = vec![a];
        // Bob: the only shared account is over its cap — better to fail than
        // to eat the owner's reserve.
        assert!(select_account_for_request(&accounts, "bob", "claude", &limits, &HashMap::new()).is_none());
        // Alice still gets her own account through the last-resort path.
        let picked = select_account_for_request(&accounts, "alice", "claude", &limits, &HashMap::new());
        assert_eq!(picked.map(|p| p.id), Some("a1".to_string()));
    }

    #[test]
    fn daily_token_limit_blocks_non_owner_but_never_owner() {
        let mut a = account("a1", "alice", true, None);
        a.daily_token_limit = Some(1000);
        let limits = HashMap::new();
        let mut usage = HashMap::new();
        // Cap reached today: closed to bob, still open to alice.
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat {
                others_billable_today: 1000,
                usage_day: Some(Utc::now().date_naive()),
                ..OwnerUsageStat::default()
            },
        );
        assert!(!within_daily_token_limit(&a, &usage));
        assert!(!account_open_to_requester(&a, "bob", &limits, &usage));
        assert!(account_open_to_requester(&a, "alice", &limits, &usage));
        // Below the cap it stays open.
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat {
                others_billable_today: 999,
                usage_day: Some(Utc::now().date_naive()),
                ..OwnerUsageStat::default()
            },
        );
        assert!(account_open_to_requester(&a, "bob", &limits, &usage));
        // Selection (including the last-resort path) honors the cap too.
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat {
                others_billable_today: 1000,
                usage_day: Some(Utc::now().date_naive()),
                ..OwnerUsageStat::default()
            },
        );
        let accounts = vec![a];
        assert!(select_account_for_request(&accounts, "bob", "claude", &limits, &usage).is_none());
    }

    #[test]
    fn daily_token_limit_resets_at_day_boundary() {
        let mut a = account("a1", "alice", true, None);
        a.daily_token_limit = Some(1000);
        let limits = HashMap::new();
        let mut usage = HashMap::new();
        // Yesterday's exhausted counter must not block today.
        usage.insert(
            "a1".to_string(),
            OwnerUsageStat {
                others_billable_today: 5000,
                usage_day: Some(Utc::now().date_naive() - chrono::Duration::days(1)),
                ..OwnerUsageStat::default()
            },
        );
        assert!(within_daily_token_limit(&a, &usage));
        assert!(account_open_to_requester(&a, "bob", &limits, &usage));
        // No usage data at all => open (a zero-history account can't be over).
        assert!(within_daily_token_limit(&a, &HashMap::new()));
    }

    #[test]
    fn affinity_sticks_until_pressure_threshold() {
        let preferred = account("a1", "alice", true, None);
        let fresh = account("a2", "anna", true, None);
        let mut limits = HashMap::new();
        limits.insert("a2".to_string(), snapshot(10.0, 20.0));
        let usage = HashMap::new();
        let outlooks = HashMap::new();
        let accounts = vec![preferred.clone(), fresh];
        // 80% primary: below the 85% migrate threshold — keep the binding.
        limits.insert("a1".to_string(), snapshot(80.0, 50.0));
        assert!(!should_rebalance_affinity("claude", &preferred, &accounts, "bob", &limits, &usage, &outlooks));
        // 86% primary with a much fresher peer — migrate.
        limits.insert("a1".to_string(), snapshot(86.0, 50.0));
        assert!(should_rebalance_affinity("claude", &preferred, &accounts, "bob", &limits, &usage, &outlooks));
        // The selection funnel actually moves the session off a1.
        let picked = select_account_for_request_with_preference(
            &accounts, "bob", "claude", &limits, &usage, &outlooks, Some("a1"),
        );
        assert_eq!(picked.map(|p| p.id), Some("a2".to_string()));
    }

    #[test]
    fn affinity_keeps_binding_without_a_clearly_better_peer() {
        let preferred = account("a1", "alice", true, None);
        let peer = account("a2", "anna", true, None);
        let mut limits = HashMap::new();
        // Pressured, but the peer is only 10 points fresher (< 20-point gap):
        // losing the prompt cache isn't worth a marginal move.
        limits.insert("a1".to_string(), snapshot(86.0, 50.0));
        limits.insert("a2".to_string(), snapshot(76.0, 40.0));
        let usage = HashMap::new();
        let outlooks = HashMap::new();
        let accounts = vec![preferred.clone(), peer];
        assert!(!should_rebalance_affinity("claude", &preferred, &accounts, "bob", &limits, &usage, &outlooks));
        let picked = select_account_for_request_with_preference(
            &accounts, "bob", "claude", &limits, &usage, &outlooks, Some("a1"),
        );
        assert_eq!(picked.map(|p| p.id), Some("a1".to_string()));
    }

    #[test]
    fn affinity_migrates_on_projected_exhaustion() {
        let preferred = account("a1", "alice", true, None);
        let fresh = account("a2", "anna", true, None);
        let mut limits = HashMap::new();
        // 70% used — percent thresholds alone would keep the binding…
        limits.insert("a1".to_string(), snapshot(70.0, 30.0));
        limits.insert("a2".to_string(), snapshot(10.0, 20.0));
        let usage = HashMap::new();
        // …but the capacity model projects the wall in 10 minutes.
        let mut outlooks = HashMap::new();
        outlooks.insert(
            "a1".to_string(),
            crate::capacity::AccountOutlook {
                primary: Some(crate::capacity::WindowOutlook {
                    used_percent: 70.0,
                    burn_rate_pct_per_hour: Some(180.0),
                    minutes_to_exhaust: Some(10),
                    reset_after_seconds: Some(3 * 3600),
                    exhaust_before_reset: true,
                }),
                secondary: None,
                computed_at: Some(Utc::now()),
            },
        );
        let accounts = vec![preferred.clone(), fresh];
        assert!(should_rebalance_affinity("claude", &preferred, &accounts, "bob", &limits, &usage, &outlooks));
    }

    #[test]
    fn cursor_affinity_never_rebalances_on_percent() {
        let mut preferred = account("c1", "alice", true, None);
        preferred.provider = "cursor".to_string();
        let mut fresh = account("c2", "anna", true, None);
        fresh.provider = "cursor".to_string();
        let mut limits = HashMap::new();
        limits.insert("c1".to_string(), snapshot(99.0, 99.0));
        limits.insert("c2".to_string(), snapshot(1.0, 1.0));
        let accounts = vec![preferred.clone(), fresh];
        assert!(!should_rebalance_affinity(
            "cursor", &preferred, &accounts, "bob", &limits, &HashMap::new(), &HashMap::new(),
        ));
    }

    #[test]
    fn recent_picks_spread_load_between_equals() {
        let mut a = account("a1", "alice", true, None);
        let b = account("a2", "anna", true, None);
        let mut limits = HashMap::new();
        limits.insert("a1".to_string(), snapshot(10.0, 20.0));
        limits.insert("a2".to_string(), snapshot(10.0, 20.0));
        // Equal accounts: a1 was just picked several times, so a2 wins now.
        a.runtime.recent_picks = 3.0;
        let accounts = vec![a, b];
        let picked = select_account_for_request(&accounts, "bob", "claude", &limits, &HashMap::new());
        assert_eq!(picked.map(|p| p.id), Some("a2".to_string()));
    }
}

pub(crate) fn select_account_for_request(
    accounts: &[UpstreamAccount],
    request_user_id: &str,
    provider: &str,
    rate_limits: &HashMap<String, RateLimitSnapshot>,
    owner_usage: &HashMap<String, OwnerUsageStat>,
) -> Option<UpstreamAccount> {
    const TIER_THRESHOLD_PERCENT: f64 = 50.0;
    /// How much better a below-threshold tier-2 account's score must be to
    /// outrank the best tier-1 account (non-codex only): tier-1 (max/team)
    /// accounts are preferred per se, so the upgrade needs a clear margin, not
    /// a coin flip.
    const TIER2_UPGRADE_MARGIN: f64 = 0.3;

    #[derive(Clone, Copy)]
    struct ScoredCandidate<'a> {
        account: &'a UpstreamAccount,
        tier: i32,
        secondary_pct: f64,
        score: f64,
    }

    fn snapshot_for<'a>(
        rate_limits: &'a HashMap<String, RateLimitSnapshot>,
        account: &UpstreamAccount,
    ) -> Option<&'a RateLimitSnapshot> {
        rate_limits.get(&account.id)
    }

    fn plan_hint(account: &UpstreamAccount, snapshot: Option<&RateLimitSnapshot>) -> String {
        // Consider BOTH the (durable, user-set) account label and the snapshot's
        // plan_type. Snapshots usually carry only the generic provider name
        // ("claude"/"codex"), so preferring them outright would hide the real
        // tier ("max"/"team"/"pro") that lives in the label or the codex
        // response-header plan — leaving the tier system unreachable / flapping.
        let mut hint = account.account_label.trim().to_ascii_lowercase();
        if let Some(plan) = snapshot.and_then(|s| s.plan_type.as_ref()) {
            hint.push(' ');
            hint.push_str(&plan.trim().to_ascii_lowercase());
        }
        hint
    }

    fn account_tier(provider: &str, plan: &str) -> i32 {
        match provider {
            "claude" => {
                if plan.contains("max") || plan.contains("team") {
                    1
                } else if plan.contains("pro") {
                    3
                } else {
                    2
                }
            }
            "codex" => {
                if plan.contains("pro") {
                    1
                } else {
                    2
                }
            }
            _ => 2,
        }
    }

    fn cooling_down(provider: &str, snapshot: Option<&RateLimitSnapshot>) -> bool {
        if provider == "codex" {
            return false;
        }
        let Some(s) = snapshot else {
            return false;
        };
        if active_limit_blocked(s.active_limit.as_deref().unwrap_or_default()) {
            return true;
        }
        // Cursor's percent is included-spend, not a rate-limit window — 100%
        // there doesn't mean cooling (see `usage_percent_gates_selection`).
        if !usage_percent_gates_selection(provider) {
            return false;
        }
        s.primary_used_percent.unwrap_or(0.0) >= 100.0
            || s.secondary_used_percent.unwrap_or(0.0) >= 100.0
    }

    fn account_score(snapshot: Option<&RateLimitSnapshot>, penalty: f64, recent_picks: f64) -> f64 {
        // Accumulated failure penalty deprioritizes troubled accounts: a single
        // transient blip (0.3) nudges, an auth/payment failure (10/50) sinks the
        // account to last resort. Penalty decays back to 0 over time.
        // `recent_picks` adds a small per-selection pressure so that between two
        // comparable accounts requests alternate (water-filling) instead of all
        // landing on the one whose stale snapshot still looks best.
        let penalty_adjust = -penalty.max(0.0) - recent_picks.max(0.0) * 0.05;
        let Some(s) = snapshot else {
            return 0.7 + penalty_adjust;
        };
        let primary = s.primary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
        let secondary = s.secondary_used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
        let mut score = 1.0 - (secondary / 100.0);

        if primary > 80.0 {
            score -= ((primary - 80.0) / 100.0) * 2.0;
        } else if primary < 50.0 {
            score += ((50.0 - primary) / 100.0) * 0.15;
        }

        if s.credits_unlimited.unwrap_or(false) || s.credits_has_credits.unwrap_or(false) {
            score += 0.1;
        }
        if s
            .active_limit
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("auth_invalid")
        {
            score -= 10.0;
        }
        score + penalty_adjust
    }

    fn pick_best<'s, 'a>(candidates: &'s [ScoredCandidate<'a>]) -> Option<&'s ScoredCandidate<'a>> {
        candidates.iter().max_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.account.created_at.cmp(&right.account.created_at))
        })
    }

    fn choose_from_group<'a>(
        provider: &str,
        group: &'a [&'a UpstreamAccount],
        rate_limits: &HashMap<String, RateLimitSnapshot>,
    ) -> Option<UpstreamAccount> {
        let mut eligible: Vec<ScoredCandidate<'a>> = Vec::new();
        let mut cooling: Vec<ScoredCandidate<'a>> = Vec::new();

        for account in group {
            let snapshot = snapshot_for(rate_limits, account);
            let primary = snapshot
                .and_then(|s| s.primary_used_percent)
                .unwrap_or(0.0)
                .clamp(0.0, 100.0);
            let secondary = snapshot
                .and_then(|s| s.secondary_used_percent)
                .unwrap_or(0.0)
                .clamp(0.0, 100.0);

            if usage_percent_gates_selection(provider)
                && (primary >= PRIMARY_HARD_EXCLUDE_PERCENT
                    || secondary >= SECONDARY_HARD_EXCLUDE_PERCENT)
            {
                continue;
            }

            let candidate = ScoredCandidate {
                account,
                tier: account_tier(provider, &plan_hint(account, snapshot)),
                secondary_pct: secondary,
                score: account_score(snapshot, account.runtime.penalty, account.runtime.recent_picks),
            };
            if cooling_down(provider, snapshot) {
                cooling.push(candidate);
            } else {
                eligible.push(candidate);
            }
        }

        // Walk the (tier, below-threshold) ladder in lexicographic order:
        // (1,below) → (1,any) → (2,below) → (2,any) → (3,below) → (3,any),
        // with one exception — on non-codex providers a below-threshold tier-2
        // account may outrank the best tier-1 account when its score is better
        // by a clear margin (a healthy pro account beats a max account deep
        // into its weekly window).
        let choose_by_tier = |items: &[ScoredCandidate<'a>]| -> Option<UpstreamAccount> {
            let best_in = |tier: i32, below_only: bool| -> Option<ScoredCandidate<'a>> {
                let filtered: Vec<_> = items
                    .iter()
                    .copied()
                    .filter(|c| {
                        c.tier == tier
                            && (!below_only || c.secondary_pct < TIER_THRESHOLD_PERCENT)
                    })
                    .collect();
                pick_best(&filtered).copied()
            };

            if let Some(best) = best_in(1, true) {
                return Some(best.account.clone());
            }
            if let Some(best_t1) = best_in(1, false) {
                if provider != "codex" {
                    if let Some(best_t2) = best_in(2, true) {
                        if best_t2.score > best_t1.score + TIER2_UPGRADE_MARGIN {
                            return Some(best_t2.account.clone());
                        }
                    }
                }
                return Some(best_t1.account.clone());
            }
            for (tier, below_only) in [(2, true), (2, false), (3, true), (3, false)] {
                if let Some(best) = best_in(tier, below_only) {
                    return Some(best.account.clone());
                }
            }
            pick_best(items).map(|c| c.account.clone())
        };

        if let Some(account) = choose_by_tier(&eligible) {
            return Some(account);
        }
        choose_by_tier(&cooling)
    }

    let mut owned: Vec<&UpstreamAccount> = Vec::new();
    let mut shared: Vec<&UpstreamAccount> = Vec::new();
    for account in accounts.iter().filter(|a| a.provider == provider) {
        if account.owner_user_id == request_user_id {
            owned.push(account);
        } else if account_open_to_requester(account, request_user_id, rate_limits, owner_usage) {
            shared.push(account);
        }
    }

    if let Some(account) = choose_from_group(provider, &owned, rate_limits) {
        return Some(account);
    }
    choose_from_group(provider, &shared, rate_limits)
        .or_else(|| {
            // Last resort: every visible account is hard-excluded (>=95% primary
            // or >=99% secondary). Serving on a nearly-full account beats a hard
            // outage for the whole pool — pick the best-scored one. Share caps
            // and the owner-heavy guard still hold here: an owner's reserve is
            // never sacrificed to keep someone else's request alive.
            accounts
                .iter()
                .filter(|a| {
                    a.provider == provider
                        && account_open_to_requester(a, request_user_id, rate_limits, owner_usage)
                })
                .max_by(|l, r| {
                    account_score(snapshot_for(rate_limits, l), l.runtime.penalty, l.runtime.recent_picks)
                        .partial_cmp(&account_score(
                            snapshot_for(rate_limits, r),
                            r.runtime.penalty,
                            r.runtime.recent_picks,
                        ))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .cloned()
        })
}

/// TTLs for the sticky-account bindings (prompt-cache locality and resumable
/// websocket sessions).
pub(crate) const PROMPT_CACHE_BINDING_TTL_SECS: i64 = 5 * 60;
pub(crate) const WS_SESSION_BINDING_TTL_SECS: i64 = 24 * 60 * 60;