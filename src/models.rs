//! Core domain models shared across layers: the pooled account record and its
//! runtime health, rate-limit snapshots, usage/audit records, and the generic
//! upstream call result. Route-specific request/response DTOs live next to
//! their handlers (`routes/*`), provider-specific wire types next to their
//! provider (`provider/*`).
use crate::prelude::*;

/// Real rate-limit info parsed from ChatGPT's `x-codex-*` response headers.
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct RateLimitSnapshot {
    pub(crate) active_limit: Option<String>,
    pub(crate) plan_type: Option<String>,
    pub(crate) primary_used_percent: Option<f64>,
    pub(crate) primary_window_minutes: Option<i64>,
    pub(crate) primary_reset_after_seconds: Option<i64>,
    pub(crate) secondary_used_percent: Option<f64>,
    pub(crate) secondary_window_minutes: Option<i64>,
    pub(crate) secondary_reset_after_seconds: Option<i64>,
    pub(crate) credits_has_credits: Option<bool>,
    pub(crate) credits_unlimited: Option<bool>,
    pub(crate) credits_balance: Option<String>,
    pub(crate) captured_at: Option<DateTime<Utc>>,
}

/// Per-account usage derived from the audit log.
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct AccountUsage {
    pub(crate) requests: u64,
    pub(crate) success: u64,
    pub(crate) errors: u64,
    pub(crate) output_bytes: u64,
    pub(crate) last_used: Option<DateTime<Utc>>,
}

/// Last-7-days billable-token split between the account's owner and everyone
/// else, derived from the audit log. Drives the owner-heavy-usage protection:
/// an account whose owner is themselves consuming most of it stops being
/// offered to other users while its weekly window is already high.
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct OwnerUsageStat {
    pub(crate) owner_billable: u64,
    pub(crate) others_billable: u64,
    /// Billable tokens served to NON-owners on `usage_day` (UTC). Drives the
    /// `daily_token_limit` donation cap; the owner's own usage never counts.
    pub(crate) others_billable_today: u64,
    /// UTC day `others_billable_today` refers to; a stale day reads as 0.
    pub(crate) usage_day: Option<NaiveDate>,
    pub(crate) computed_at: Option<DateTime<Utc>>,
}

impl OwnerUsageStat {
    /// Today's non-owner billable tokens, treating a stale `usage_day` as 0 so
    /// the cap resets at the UTC day boundary without a write.
    pub(crate) fn others_billable_on(&self, day: NaiveDate) -> u64 {
        if self.usage_day == Some(day) {
            self.others_billable_today
        } else {
            0
        }
    }
}

/// Per-user BORROWED usage (requests served by accounts the user does NOT
/// own), rebuilt from the audit log and bumped live on each request. Feeds the
/// per-user token budgets (`GATEWAY_USER_*_TOKEN_LIMIT`); usage of one's own
/// accounts is deliberately excluded — the budgets protect the shared pool.
#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct UserUsageStat {
    /// UTC day `daily_billable` refers to; a stale day reads as 0.
    pub(crate) day: Option<NaiveDate>,
    pub(crate) daily_billable: u64,
    /// Rolling last-7-days borrowed billable tokens (rebuilt every refresh
    /// tick, live-bumped in between).
    pub(crate) weekly_billable: u64,
    pub(crate) computed_at: Option<DateTime<Utc>>,
}

impl UserUsageStat {
    pub(crate) fn daily_billable_on(&self, day: NaiveDate) -> u64 {
        if self.day == Some(day) {
            self.daily_billable
        } else {
            0
        }
    }
}

/// Fixed-window per-user request counter backing `GATEWAY_USER_RPM_LIMIT`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UserRequestRate {
    /// Unix minute (`timestamp / 60`) the counter refers to.
    pub(crate) minute: i64,
    pub(crate) count: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct StickyAccountBinding {
    pub(crate) account_id: String,
    pub(crate) provider: String,
    pub(crate) user_id: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RelayRequest {
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) preferred_provider: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RelayResponse {
    pub(crate) request_id: String,
    pub(crate) user_id: String,
    pub(crate) routed_provider: String,
    pub(crate) upstream_account_id: String,
    pub(crate) upstream_owner_user_id: String,
    pub(crate) model: String,
    pub(crate) status: String,
    pub(crate) message: String,
    pub(crate) output_text: String,
}

#[derive(Debug)]
pub(crate) struct UpstreamCallResult {
    pub(crate) output_text: String,
    pub(crate) rate_limit_snapshot: Option<RateLimitSnapshot>,
}

#[derive(Debug)]
pub(crate) struct UpstreamCallError {
    pub(crate) message: String,
    pub(crate) rate_limit_snapshot: Option<RateLimitSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AuditRecord {
    pub(crate) request_id: String,
    pub(crate) user_id: String,
    pub(crate) model: String,
    pub(crate) routed_provider: String,
    pub(crate) upstream_account_id: String,
    pub(crate) upstream_owner_user_id: String,
    pub(crate) prompt_length: usize,
    pub(crate) output_length: usize,
    pub(crate) status: String,
    pub(crate) created_at: DateTime<Utc>,
    /// Real token usage parsed from the upstream response (zero when unparsed).
    #[serde(default)]
    pub(crate) tokens: TokenUsage,
}

/// Real token usage for one request, parsed from upstream SSE/JSON.
/// `billable` == uncached_input + output for both providers; the uncached input
/// is derived per provider (Anthropic's `input_tokens` already excludes cache,
/// OpenAI's includes it). See `usage::tokens`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TokenUsage {
    #[serde(default)]
    pub(crate) input_tokens: i64,
    #[serde(default)]
    pub(crate) cached_input_tokens: i64,
    #[serde(default)]
    pub(crate) cache_creation_tokens: i64,
    #[serde(default)]
    pub(crate) output_tokens: i64,
    #[serde(default)]
    pub(crate) reasoning_tokens: i64,
    #[serde(default)]
    pub(crate) billable_tokens: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct UpstreamAccount {
    pub(crate) id: String,
    pub(crate) owner_user_id: String,
    pub(crate) provider: String,
    pub(crate) account_label: String,
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: String,
    #[serde(default)]
    pub(crate) id_token: String,
    #[serde(default)]
    pub(crate) account_id: String,
    // Backward compatibility for older records that used api_key.
    #[serde(default)]
    pub(crate) api_key: String,
    /// Upstream base URL for endpoint-style providers (ollama, e.g.
    /// `http://127.0.0.1:11434`). Empty for the OAuth/account providers
    /// (codex/claude/cursor), which target a hardcoded upstream. `#[serde(default)]`
    /// keeps older records (written before this field existed) loadable.
    #[serde(default)]
    pub(crate) base_url: String,
    /// Secondary upstream base URL for providers that expose two protocol
    /// endpoints. Today only GLM uses it: `base_url` holds the OpenAI-compatible
    /// `/chat/completions` prefix, `base_url_alt` the Anthropic-compatible
    /// `/v1/messages` prefix. Empty for every other provider. `#[serde(default)]`
    /// keeps older records (written before this field existed) loadable.
    #[serde(default)]
    pub(crate) base_url_alt: String,
    pub(crate) share_enabled: bool,
    /// Cap on how far NON-OWNER traffic may drive this account's usage windows,
    /// in percent (e.g. 60 = others stop being routed here once either window
    /// reaches 60%). None = share everything (100). The owner is never capped.
    #[serde(default)]
    pub(crate) share_limit_percent: Option<f64>,
    pub(crate) daily_token_limit: Option<u64>,
    pub(crate) created_at: DateTime<Utc>,
    /// Mutable scheduling/health state used by the retry + cooldown machinery.
    /// `dead`/`disabled` persist across restarts; the rest are runtime-only.
    #[serde(default)]
    pub(crate) runtime: AccountRuntime,
}

impl UpstreamAccount {
    /// The bearer credential for upstream calls: the (trimmed) OAuth access
    /// token if present, otherwise the legacy `api_key`. May be empty.
    pub(crate) fn bearer(&self) -> &str {
        let access = self.access_token.trim();
        if access.is_empty() {
            self.api_key.trim()
        } else {
            access
        }
    }
}

/// Per-account runtime health used to drive retry/backoff/cooldown decisions,
/// ported from the Go `Account` scheduling fields (`Penalty`, `RateLimitUntil`,
/// `BackoffLevel`, `Dead`, `Disabled`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AccountRuntime {
    /// Accumulated penalty; decays *0.8 every 5 minutes. Runtime-only.
    #[serde(default, skip_serializing)]
    pub(crate) penalty: f64,
    /// Cooldown deadline; the account is skipped while `now < rate_limit_until`.
    #[serde(default, skip_serializing)]
    pub(crate) rate_limit_until: Option<DateTime<Utc>>,
    /// Exponential-backoff level, incremented on each 429, reset on success.
    #[serde(default, skip_serializing)]
    pub(crate) backoff_level: u32,
    /// Last time the penalty was decayed.
    #[serde(default, skip_serializing)]
    pub(crate) last_penalty: Option<DateTime<Utc>>,
    /// Recent selection pressure used to spread load evenly across comparable
    /// accounts: each pick adds 1, halved every decay tick. Runtime-only.
    #[serde(default, skip_serializing)]
    pub(crate) recent_picks: f64,
    /// OAuth access-token expiry (Claude). Persisted so proactive refresh can run
    /// correctly across restarts.
    #[serde(default)]
    pub(crate) expires_at: Option<DateTime<Utc>>,
    /// Last successful token refresh, throttling proactive refreshes.
    #[serde(default)]
    pub(crate) last_refresh: Option<DateTime<Utc>>,
    /// Permanently dead (cancelled subscription / deactivated workspace).
    #[serde(default)]
    pub(crate) dead: bool,
    /// Administratively disabled.
    #[serde(default)]
    pub(crate) disabled: bool,
    /// Codex account with cyber-policy access, used as a hot-swap target when a
    /// regular account hits `cyber_policy`. Persisted (set via account config).
    #[serde(default)]
    pub(crate) cyber_access: bool,
}

/// Catalog entry returned by the provider model-listing endpoints.
#[derive(Debug, Serialize)]
pub(crate) struct ModelInfo {
    pub(crate) slug: String,
    pub(crate) display_name: String,
}

