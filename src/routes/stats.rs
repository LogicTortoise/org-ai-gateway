use crate::prelude::*;
use crate::auth::extract_user_id;
use crate::pool::storage::read_audit_records;
use crate::usage::short_status;

/// Return (input, output) token counts for an audit record, preferring the real
/// `tokens` object and falling back to the char-length fields for old records.
fn audit_token_counts(r: &Value) -> (u64, u64) {
    if let Some(tokens) = r.get("tokens") {
        let input = tokens.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let output = tokens.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        if input > 0 || output > 0 {
            return (input.max(0) as u64, output.max(0) as u64);
        }
    }
    // Char-length fallback only for SUCCESS rows (legacy records predating token
    // parsing). Failure rows carry `prompt_length` = the full request-payload
    // char count with zero real tokens; counting that as input tokens — once per
    // retry attempt — would massively inflate the totals.
    if r.get("status").and_then(|v| v.as_str()) != Some("success") {
        return (0, 0);
    }
    (
        r.get("prompt_length").and_then(|v| v.as_u64()).unwrap_or(0),
        r.get("output_length").and_then(|v| v.as_u64()).unwrap_or(0),
    )
}

/// Return (cached_input, billable) token counts from the real `tokens` object,
/// or (0, 0) for old records that predate token parsing.
fn audit_cache_counts(r: &Value) -> (u64, u64) {
    let Some(tokens) = r.get("tokens") else {
        return (0, 0);
    };
    let cached = tokens.get("cached_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let billable = tokens.get("billable_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    (cached.max(0) as u64, billable.max(0) as u64)
}

/// Total prompt tokens for one record, normalized across providers so that a
/// cache hit rate (`cached / prompt`) is comparable and never exceeds 100%.
/// Codex's `input_tokens` ALREADY includes cache reads; Anthropic's excludes
/// both cache read and cache creation, so those must be added back. Without this
/// normalization the UI divided cached by an input count that, for Claude,
/// didn't contain the cached tokens at all — inflating the rate past 100%.
fn audit_prompt_tokens(r: &Value, input: u64, cached: u64) -> u64 {
    if r.get("routed_provider").and_then(|v| v.as_str()) == Some("claude") {
        let creation = r
            .get("tokens")
            .and_then(|t| t.get("cache_creation_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as u64;
        input + cached + creation
    } else {
        // Codex/others: `input_tokens` already includes cache reads. Guard
        // against malformed rows where cached > input.
        input.max(cached)
    }
}

pub(crate) async fn get_stats(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(u) => u,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response(),
    };
    // Cross-user usage (every user's `by_user`/accounts) is admin-only. Admins
    // are listed in `GATEWAY_ADMIN_USERS` (comma-separated); when that's UNSET
    // the gateway is treated as single-tenant and everyone sees the full view
    // (preserves existing local/single-user dashboards). Otherwise a non-admin
    // only ever sees their own rows.
    let admins: Vec<String> = std::env::var("GATEWAY_ADMIN_USERS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let scope_to_self = !admins.is_empty() && !admins.iter().any(|a| a == &user_id);
    let mut records = read_audit_records(&state.audit_file).await;
    if scope_to_self {
        records.retain(|r| r.get("user_id").and_then(|v| v.as_str()) == Some(user_id.as_str()));
    }

    let mut total = 0u64;
    let mut success = 0u64;
    let mut errors = 0u64;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cached_tokens = 0u64;
    let mut prompt_tokens = 0u64;
    let mut billable_tokens = 0u64;
    // (requests, input_tokens, output_tokens, cached_tokens, prompt_tokens)
    let mut by_model: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
    let mut by_provider: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
    let mut by_user: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
    let mut by_account: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
    let mut by_day: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();
    let account_name_map: HashMap<String, String> = state
        .accounts
        .read()
        .await
        .iter()
        .map(|a| {
            (
                a.id.clone(),
                format!("{}/{}", a.owner_user_id, a.account_label),
            )
        })
        .collect();

    for r in &records {
        total += 1;
        match r.get("status").and_then(|v| v.as_str()) {
            Some("success") => success += 1,
            _ => errors += 1,
        }
        // Prefer real parsed token counts; fall back to char-length proxies for
        // older audit records written before token parsing existed.
        let (itok, otok) = audit_token_counts(r);
        let (ctok, btok) = audit_cache_counts(r);
        let ptok = audit_prompt_tokens(r, itok, ctok);
        input_tokens += itok;
        output_tokens += otok;
        cached_tokens += ctok;
        prompt_tokens += ptok;
        billable_tokens += btok;

        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let provider = r.get("routed_provider").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let user = r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let acct = r
            .get("upstream_account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let day = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(10).collect::<String>())
            .filter(|s| s.len() == 10)
            .unwrap_or_else(|| "(未知)".into());

        let bump = |m: &mut HashMap<String, (u64, u64, u64, u64, u64)>, key: String| {
            let e = m.entry(key).or_default();
            e.0 += 1;
            e.1 += itok;
            e.2 += otok;
            e.3 += ctok;
            e.4 += ptok;
        };

        bump(&mut by_model, if model.is_empty() { "(未指定)".into() } else { model });
        bump(&mut by_provider, if provider.is_empty() { "(未知)".into() } else { provider });
        bump(&mut by_user, if user.is_empty() { "(未知)".into() } else { user });
        bump(&mut by_day, day);
        if !acct.is_empty() {
            let key = account_name_map.get(&acct).cloned().unwrap_or(acct);
            bump(&mut by_account, key);
        }
    }

    enum BucketOrder {
        ByRequestsDesc,
        ByKeyDesc,
    }
    let to_buckets = |m: HashMap<String, (u64, u64, u64, u64, u64)>, order: BucketOrder| {
        let mut v: Vec<StatBucket> = m
            .into_iter()
            .map(
                |(key, (requests, input_tokens, output_tokens, cached_tokens, prompt_tokens))| {
                    StatBucket {
                        key,
                        requests,
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        prompt_tokens,
                    }
                },
            )
            .collect();
        match order {
            BucketOrder::ByRequestsDesc => v.sort_by(|a, b| b.requests.cmp(&a.requests)),
            BucketOrder::ByKeyDesc => v.sort_by(|a, b| b.key.cmp(&a.key)),
        }
        v
    };

    let recent: Vec<RecentEntry> = records
        .iter()
        .rev()
        .take(20)
        .filter_map(|r| {
            let (input_tokens, output_tokens) = audit_token_counts(r);
            Some(RecentEntry {
                created_at: r
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())?
                    .with_timezone(&Utc),
                requester: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                used_account: {
                    let aid = r
                        .get("upstream_account_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    account_name_map
                        .get(&aid)
                        .cloned()
                        .unwrap_or(aid)
                },
                model: r.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                status: short_status(r.get("status").and_then(|v| v.as_str()).unwrap_or("")),
                input_tokens: input_tokens as usize,
                output_tokens: output_tokens as usize,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(StatsResponse {
            generated_at: Utc::now(),
            total_requests: total,
            success,
            errors,
            total_input_tokens: input_tokens,
            total_output_tokens: output_tokens,
            total_cached_tokens: cached_tokens,
            total_prompt_tokens: prompt_tokens,
            total_billable_tokens: billable_tokens,
            by_model: to_buckets(by_model, BucketOrder::ByRequestsDesc),
            by_provider: to_buckets(by_provider, BucketOrder::ByRequestsDesc),
            by_user: to_buckets(by_user, BucketOrder::ByRequestsDesc),
            by_account: to_buckets(by_account, BucketOrder::ByRequestsDesc),
            by_day: to_buckets(by_day, BucketOrder::ByKeyDesc),
            recent,
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Response DTOs for `GET /v1/stats`.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct StatsResponse {
    pub(crate) generated_at: DateTime<Utc>,
    pub(crate) total_requests: u64,
    pub(crate) success: u64,
    pub(crate) errors: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cached_tokens: u64,
    /// Provider-normalized total prompt tokens (input incl. cache reads + cache
    /// creation). Denominator for a comparable cache hit rate; see
    /// `audit_prompt_tokens`.
    pub(crate) total_prompt_tokens: u64,
    pub(crate) total_billable_tokens: u64,
    pub(crate) by_model: Vec<StatBucket>,
    pub(crate) by_provider: Vec<StatBucket>,
    pub(crate) by_user: Vec<StatBucket>,
    pub(crate) by_account: Vec<StatBucket>,
    pub(crate) by_day: Vec<StatBucket>,
    pub(crate) recent: Vec<RecentEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StatBucket {
    pub(crate) key: String,
    pub(crate) requests: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cached_tokens: u64,
    /// Provider-normalized prompt-token total for this bucket; cache-hit-rate
    /// denominator (see `audit_prompt_tokens`).
    pub(crate) prompt_tokens: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecentEntry {
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) requester: String,
    pub(crate) used_account: String,
    pub(crate) model: String,
    pub(crate) status: String,
    pub(crate) input_tokens: usize,
    pub(crate) output_tokens: usize,
}