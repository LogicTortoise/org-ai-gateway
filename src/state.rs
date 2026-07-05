use crate::prelude::*;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) audit_file: PathBuf,
    pub(crate) account_file: PathBuf,
    /// Gateway-issued API-key snapshot file (the "open API" credentials).
    pub(crate) api_key_file: PathBuf,
    /// Capacity-history snapshot file (derived data; see `capacity`).
    pub(crate) capacity_file: PathBuf,
    /// Provider-priority chains config file (`provider_chains.json`).
    pub(crate) chain_file: PathBuf,
    pub(crate) accounts: Arc<RwLock<Vec<UpstreamAccount>>>,
    /// Latest real rate-limit snapshot per account internal id, captured from the
    /// `x-codex-*` response headers ChatGPT returns on each `responses` call.
    pub(crate) rate_limits: Arc<RwLock<HashMap<String, RateLimitSnapshot>>>,
    /// Per-account usage-window time series (the trajectory behind
    /// `rate_limits`), feeding burn rates and the burn-down dashboard.
    pub(crate) capacity_history:
        Arc<RwLock<HashMap<String, std::collections::VecDeque<crate::capacity::WindowSample>>>>,
    /// Per-account exhaustion outlooks recomputed every minute by
    /// `capacity::run_capacity_maintenance`; read by the scheduler's sticky-
    /// session rebalance check and the capacity endpoint.
    pub(crate) capacity_outlooks: Arc<RwLock<HashMap<String, crate::capacity::AccountOutlook>>>,
    /// Session-aware websocket stickiness: resumable session -> last healthy account.
    pub(crate) ws_session_bindings: Arc<RwLock<HashMap<String, StickyAccountBinding>>>,
    /// Prompt-cache locality: transient prompt cache key -> preferred account.
    pub(crate) prompt_cache_bindings: Arc<RwLock<HashMap<String, StickyAccountBinding>>>,
    /// Per-account owner-vs-others billable split over the last 7 days (plus
    /// today's donated tokens for the `daily_token_limit` cap), rebuilt from
    /// the audit log by `run_owner_usage_refresh` and live-bumped per request.
    /// Keyed by internal id.
    pub(crate) owner_usage: Arc<RwLock<HashMap<String, OwnerUsageStat>>>,
    /// Per-user BORROWED billable tokens (day + rolling 7d), same rebuild +
    /// live-bump cycle as `owner_usage`. Feeds the per-user token budgets.
    pub(crate) user_usage: Arc<RwLock<HashMap<String, UserUsageStat>>>,
    /// Per-user fixed-window request counters for `GATEWAY_USER_RPM_LIMIT`.
    pub(crate) user_request_rate: Arc<RwLock<HashMap<String, UserRequestRate>>>,
    /// Serializes the account snapshot write/rename so concurrent persisters
    /// (failures, refreshes, connects) can never publish a partial or stale
    /// snapshot of the only credential store.
    pub(crate) persist_lock: Arc<tokio::sync::Mutex<()>>,
    /// Per-account (by internal id) single-flight refresh guard. OAuth refresh
    /// tokens rotate on use, so concurrent reactive refreshes for the same
    /// account must be serialized — otherwise all but the first redeem a
    /// now-consumed token and wrongly mark a just-refreshed account dead.
    pub(crate) refresh_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Global provider-priority chains (one per protocol slot). Read on every
    /// proxied request to decide the failover/round-robin order; edited via the
    /// `/v1/provider/chains` API and persisted to `chain_file`.
    pub(crate) chains: Arc<RwLock<crate::provider::chains::ProviderChains>>,
    /// Per-slot round-robin rotation counters (keyed by `ChainSlot::as_str`).
    /// Incremented once per request in round-robin mode to rotate the start.
    pub(crate) chain_rr: Arc<tokio::sync::Mutex<HashMap<String, usize>>>,
}

