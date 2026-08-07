use crate::prelude::*;
use crate::pool::storage::persist_all_accounts;
pub(crate) mod chains;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod cursor;
pub(crate) mod deepseek;
pub(crate) mod glm;
pub(crate) mod kimi;
pub(crate) mod minimax;
pub(crate) mod model_config;
pub(crate) mod ollama;
pub(crate) mod trae;
pub(crate) mod usage_window;

/// The upstream providers the gateway can route to. Accounts persist the
/// provider as a string (`UpstreamAccount.provider`), so this enum is the
/// validation/dispatch boundary: parse once at the edges, compare enums inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Codex,
    Claude,
    Cursor,
    /// A local model server speaking the Ollama API (`/api/chat`). Unlike the
    /// others it has no OAuth/account/rate-limit machinery: an "account" is just
    /// a base URL, it's free, and it serves as both a directly-selectable
    /// provider (`model=ollama/<name>`) and the whole-pool fallback.
    Ollama,
    /// GLM (Zhipu / z.ai). An API-key endpoint provider (no OAuth/refresh). It
    /// can serve BOTH client protocols: Claude-format traffic (`/v1/messages`)
    /// rides GLM's Anthropic-compatible endpoint as a near-native buffered
    /// passthrough, while Codex-format traffic (`/v1/responses`) goes through the
    /// shared format adapter onto GLM's OpenAI-compatible `/chat/completions`.
    /// An "account" is a base URL (OpenAI-compat) + optional alt base URL
    /// (Anthropic-compat) + an api key. Real token usage is returned by both
    /// endpoints, so audited usage is exact.
    Glm,
    /// Kimi (Moonshot AI). Structurally identical to GLM — an API-key endpoint
    /// provider (no OAuth/refresh) serving BOTH client protocols via Moonshot's
    /// Anthropic-compatible (`/anthropic/v1/messages`) and OpenAI-compatible
    /// (`/v1/chat/completions`) endpoints. Endpoints are well-known so the base
    /// URLs default to Moonshot's public ones; an "account" is effectively just
    /// an api key. Serves as a Claude Code fallback via the Claude chain.
    Kimi,
    /// Trae (ByteDance Trae IDE models), reached through a local `trae2anthropic`
    /// sidecar. Unlike GLM/Kimi it serves the **Claude slot only** — the sidecar
    /// exposes an Anthropic-compatible `/v1/messages` and nothing OpenAI-shaped,
    /// so there is no Codex adapter path. An "account" is the sidecar's base URL
    /// plus an OPTIONAL api key (the sidecar's own auth is opt-in); the real Trae
    /// logins and their quotas live inside the sidecar's admin panel. Exists as a
    /// Claude Code 额度降级 fallback. See `trae.rs` for why it's a sidecar.
    Trae,
    /// MiniMax (MiniMax-M series). An API-key endpoint provider (no OAuth/refresh)
    /// serving the **Claude slot only**: it is wired to MiniMax's
    /// Anthropic-compatible `/anthropic/v1/messages` as a buffered passthrough so
    /// tool calls survive. Their OpenAI-ish endpoint is deliberately NOT wired —
    /// it would only reach the text-only adapter. An "account" is effectively just
    /// an api key (the base URL defaults to MiniMax's public endpoint).
    Minimax,
    /// DeepSeek. Structurally identical to MiniMax — an API-key endpoint provider
    /// (no OAuth/refresh) serving the **Claude slot only** via DeepSeek's
    /// Anthropic-compatible `/anthropic/v1/messages`, the same surface their docs
    /// point Claude Code at. An "account" is effectively just an api key.
    Deepseek,
}

impl Provider {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "cursor" => Some(Self::Cursor),
            "ollama" => Some(Self::Ollama),
            "glm" => Some(Self::Glm),
            "kimi" => Some(Self::Kimi),
            "trae" => Some(Self::Trae),
            "minimax" => Some(Self::Minimax),
            "deepseek" => Some(Self::Deepseek),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Ollama => "ollama",
            Self::Glm => "glm",
            Self::Kimi => "kimi",
            Self::Trae => "trae",
            Self::Minimax => "minimax",
            Self::Deepseek => "deepseek",
        }
    }
}

/// Default models when a relay request leaves `model` empty.
const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";
const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4-5";

pub(crate) fn route_provider(model: &str, preferred_provider: Option<&str>) -> String {
    // Only honor a *known* preferred provider. A typo ("claud") would otherwise
    // float downstream as an unroutable string and fail far from its cause;
    // falling through to model-based routing here keeps the request alive.
    if let Some(pref) = preferred_provider.and_then(Provider::parse) {
        return pref.as_str().to_string();
    }

    if ollama::is_ollama_model(model) {
        return Provider::Ollama.as_str().to_string();
    }

    if cursor::is_cursor_model(model) {
        return Provider::Cursor.as_str().to_string();
    }

    if glm::is_glm_model(model) {
        return Provider::Glm.as_str().to_string();
    }

    if kimi::is_kimi_model(model) {
        return Provider::Kimi.as_str().to_string();
    }

    if minimax::is_minimax_model(model) {
        return Provider::Minimax.as_str().to_string();
    }

    if deepseek::is_deepseek_model(model) {
        return Provider::Deepseek.as_str().to_string();
    }

    // Checked before the `contains("claude")` catch-all, though the two can't
    // actually collide: `is_trae_model` only accepts the explicit `trae` /
    // `trae/<model>` forms, never the vendor model ids Trae resells.
    if trae::is_trae_model(model) {
        return Provider::Trae.as_str().to_string();
    }

    if model.to_ascii_lowercase().contains("claude") {
        return Provider::Claude.as_str().to_string();
    }

    Provider::Codex.as_str().to_string()
}

pub(crate) fn normalize_model_for_provider(model: &str, provider: &str) -> String {
    match Provider::parse(provider) {
        Some(Provider::Codex) if model.trim().is_empty() => DEFAULT_CODEX_MODEL.to_string(),
        Some(Provider::Claude) if model.trim().is_empty() => DEFAULT_CLAUDE_MODEL.to_string(),
        Some(Provider::Cursor) => cursor::cursor_canonical_model(model),
        Some(Provider::Ollama) => ollama::ollama_canonical_model(model),
        Some(Provider::Glm) => glm::glm_canonical_model(model),
        Some(Provider::Kimi) => kimi::kimi_canonical_model(model),
        Some(Provider::Trae) => trae::trae_canonical_model(model),
        Some(Provider::Minimax) => minimax::minimax_canonical_model(model),
        Some(Provider::Deepseek) => deepseek::deepseek_canonical_model(model),
        _ => model.to_string(),
    }
}

/// Whether a provider's Anthropic-compatible surface is a third-party imitation
/// rather than Anthropic itself. Everything on this list is reached through the
/// Claude slot's raw `/v1/messages` passthrough, and none of it implements
/// Anthropic's server-side tools — see `strip_anthropic_server_tools`.
pub(crate) fn is_third_party_anthropic(provider: &str) -> bool {
    matches!(provider, "glm" | "kimi" | "trae" | "minimax" | "deepseek")
}

/// Remove Anthropic *server tools* from an Anthropic-shaped payload, returning
/// how many items were dropped.
///
/// Server tools (`web_search_20250305`, `web_fetch_*`, `code_execution_*`,
/// `computer_*`, …) are executed by Anthropic's own backend, so they exist only
/// on first-party Anthropic. Claude Code declares them in every request when the
/// matching feature is on — that's real traffic here, see the pinned-name note
/// in `fingerprint::claude::obfuscate_tool_names`. Forwarding them to a
/// third-party Anthropic-compatible upstream at best wastes prompt tokens on a
/// tool that can never fire, and at worst 400s the request on an unrecognized
/// `type`.
///
/// Three things get cleaned, because dropping only the first would leave the
/// payload self-inconsistent:
///   1. the declarations in `tools`,
///   2. a `tool_choice` pointing at one of them,
///   3. the `server_tool_use` / `*_tool_result` blocks an earlier first-party
///      turn already produced — a session that falls back to a cheap provider
///      mid-conversation carries those in its transcript.
///
/// Client-declared tools are untouched, including client-side MCP tools: those
/// arrive as ordinary custom tools (`mcp__minimax__web_search`), and the client,
/// not the upstream, executes them. That's exactly why a client-side MCP is the
/// working substitute for the built-in web search on these providers.
pub(crate) fn strip_anthropic_server_tools(payload: &mut Value) -> usize {
    let mut removed = 0usize;
    let mut dropped_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    // A client-declared tool has no `type` at all, or `type: "custom"`. Anything
    // else is one of Anthropic's own — same rule the fingerprint's pinned-name
    // set uses, kept identical on purpose.
    let mut tools_now_empty = false;
    if let Some(tools) = payload.get_mut("tools").and_then(|t| t.as_array_mut()) {
        tools.retain(|tool| {
            let is_server = match tool.get("type").and_then(|t| t.as_str()) {
                Some(t) => t != "custom",
                None => false,
            };
            if is_server {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    dropped_names.insert(name.to_string());
                }
                removed += 1;
            }
            !is_server
        });
        tools_now_empty = tools.is_empty();
    }
    if tools_now_empty {
        // An empty `tools` array is not universally accepted, and a `tool_choice`
        // with nothing to choose from certainly isn't. Drop both keys.
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
        }
    } else if !dropped_names.is_empty() {
        let stale = payload
            .pointer("/tool_choice/name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| dropped_names.contains(n));
        if stale {
            payload["tool_choice"] = json!({ "type": "auto" });
        }
    }

    if let Some(messages) = payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            content.retain(|block| {
                let drop = match block.get("type").and_then(|t| t.as_str()) {
                    Some("server_tool_use") | Some("mcp_tool_use") => true,
                    // Every server tool names its result block `<tool>_tool_result`
                    // (`web_search_tool_result`, `code_execution_tool_result`, …).
                    // The client-side block is the bare `tool_result`, which is
                    // shorter than the suffix and so never matches.
                    Some(t) => t.ends_with("_tool_result"),
                    None => false,
                };
                if drop {
                    removed += 1;
                }
                !drop
            });
            if content.is_empty() {
                // Server tool blocks can be a turn's entire content. An empty
                // content array is rejected, and deleting the message would break
                // user/assistant alternation, so leave a marker behind.
                *content = vec![json!({ "type": "text", "text": "[server tool output removed]" })];
            }
        }
    }

    removed
}

/// Whether an account label is still an auto-generated default (no real
/// identity), so it's safe to replace with the upstream account email without
/// clobbering a label the owner set themselves. The UI seeds new accounts with
/// the current user id as the label, so `label == owner` counts as generic too.
pub(crate) fn label_is_generic(label: &str, owner: &str, provider: &str) -> bool {
    let l = label.trim();
    l.is_empty() || l == owner || l == format!("{}-{}", owner, provider)
}

/// The upstream account's real identity email, used to label the account.
/// Codex reads it offline from the JWT; Claude calls the OAuth profile endpoint
/// (which, unlike the usage endpoint, isn't rate-limited). Cursor labels itself
/// from the cached email at connect, so it has no resolver here.
pub(crate) async fn account_identity_email(account: &UpstreamAccount) -> Option<String> {
    match account.provider.as_str() {
        "codex" => codex::codex_account_email(account),
        "claude" => claude::fetch_claude_account_email(account).await,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared OAuth-refresh skeleton. Codex and Claude differ only in the token
// endpoint and response fields; the concurrency-sensitive parts (single-flight,
// re-read under the lock, mark-dead on revoked tokens, persist) live here ONCE
// so the two providers can't drift apart again.
// ---------------------------------------------------------------------------

/// Provider-specific outcome of a token-endpoint call, applied uniformly:
/// `None` fields keep the account's existing value.
pub(crate) struct TokenUpdate {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) id_token: Option<String>,
    pub(crate) account_id: Option<String>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
}

/// Refresh an account's OAuth tokens and persist the result.
///
/// Single-flight per account id: concurrent refreshes (reactive 401/403 retries
/// and the proactive loop) serialize so only one redeems the rotating refresh
/// token — the losers would otherwise get `invalid_grant` and mark a
/// just-refreshed account dead. On a revoked token (`invalid_grant` /
/// `refresh_token_reused`) the account IS marked dead so it stops being
/// selected.
pub(crate) async fn refresh_account_tokens<F, Fut>(
    state: &AppState,
    account: &UpstreamAccount,
    request_refresh: F,
) -> Result<UpstreamAccount, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<TokenUpdate, String>>,
{
    let lock = {
        let mut locks = state.refresh_locks.lock().await;
        locks
            .entry(account.id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // Re-read under the lock: a concurrent refresh may have already rotated the
    // token. If the stored access token has changed (and the account is alive),
    // that refresh succeeded — return it instead of redeeming a stale token.
    let stale_access = account.access_token.trim().to_string();
    let current = {
        let accounts = state.accounts.read().await;
        accounts.iter().find(|a| a.id == account.id).cloned()
    };
    let account = current.as_ref().unwrap_or(account);
    if account.access_token.trim() != stale_access && !account.runtime.dead {
        return Ok(account.clone());
    }
    // Belt-and-suspenders for endpoints that return the SAME access token on a
    // refresh: a very recent `last_refresh` means a concurrent caller already
    // redeemed the rotating refresh token. Redeeming it again would fail with
    // `invalid_grant` and wrongly mark this just-refreshed account dead, so
    // treat the recent refresh as authoritative and return it.
    if !account.runtime.dead {
        if let Some(last) = account.runtime.last_refresh {
            if Utc::now() - last < chrono::Duration::seconds(REFRESH_DEDUP_WINDOW_SECS) {
                return Ok(account.clone());
            }
        }
    }

    let refresh_token = account.refresh_token.trim();
    if refresh_token.is_empty() {
        return Err("account has no refresh_token; please re-import credentials".to_string());
    }

    let update = match request_refresh(refresh_token.to_string()).await {
        Ok(v) => v,
        Err(err) => {
            // A revoked/rotated-away refresh token won't recover on retry —
            // mark the account dead so it stops being selected. Decide on the
            // STRUCTURED OAuth error code, not a bare substring, so an unrelated
            // error body that merely contains the phrase can't kill a live
            // account.
            if refresh_error_is_revoked(&err) {
                {
                    let mut accounts = state.accounts.write().await;
                    if let Some(a) = accounts.iter_mut().find(|a| a.id == account.id) {
                        a.runtime.dead = true;
                        a.runtime.penalty += DEAD_ACCOUNT_PENALTY;
                        a.runtime.last_refresh = Some(Utc::now());
                    }
                }
                if let Err(persist_err) = persist_all_accounts(state).await {
                    warn!(
                        "failed persisting dead-account flag for {}: {}",
                        account.account_label, persist_err
                    );
                }
            }
            return Err(err);
        }
    };

    if update.access_token.trim().is_empty() {
        return Err("token refresh response missing access_token".to_string());
    }

    let updated = {
        let mut accounts = state.accounts.write().await;
        let entry = accounts
            .iter_mut()
            .find(|a| a.id == account.id)
            .ok_or_else(|| "account not found while persisting refreshed token".to_string())?;
        entry.access_token = update.access_token;
        if let Some(rt) = update.refresh_token.filter(|v| !v.trim().is_empty()) {
            entry.refresh_token = rt;
        }
        if let Some(idt) = update.id_token {
            entry.id_token = idt;
        }
        if let Some(aid) = update.account_id {
            entry.account_id = aid;
        }
        entry.runtime.expires_at = update.expires_at.or(entry.runtime.expires_at);
        entry.runtime.last_refresh = Some(Utc::now());
        entry.runtime.dead = false;
        entry.clone()
    };
    persist_all_accounts(state).await?;
    Ok(updated)
}

/// A penalty large enough to sink an account below every live candidate.
pub(crate) const DEAD_ACCOUNT_PENALTY: f64 = 100.0;

/// Window in which a just-completed refresh by a concurrent caller is treated as
/// authoritative, so the loser of the single-flight race never re-redeems the
/// (now-rotated) refresh token. See `refresh_account_tokens`.
pub(crate) const REFRESH_DEDUP_WINDOW_SECS: i64 = 30;

/// Whether a refresh-endpoint error means the refresh token is permanently
/// revoked/rotated away (→ mark the account dead) rather than a transient blip.
/// Prefers the structured OAuth `error` code; falls back to the quoted JSON
/// form only — never a bare, unquoted mention anywhere in the body.
fn refresh_error_is_revoked(err: &str) -> bool {
    if let Some(start) = err.find('{') {
        if let Ok(v) = serde_json::from_str::<Value>(&err[start..]) {
            if let Some(code) = v.get("error").and_then(|e| e.as_str()) {
                return matches!(code, "invalid_grant" | "refresh_token_reused");
            }
        }
    }
    let l = err.to_ascii_lowercase();
    l.contains("\"invalid_grant\"") || l.contains("\"refresh_token_reused\"")
}

/// Proactive-refresh due check shared by Codex and Claude: at most once per 15
/// minutes per account, refresh within 5 minutes of `exp` (or, when expiry is
/// unknown, if we haven't refreshed in 12 hours).
pub(crate) fn token_needs_refresh(
    account: &UpstreamAccount,
    now: DateTime<Utc>,
    exp: Option<DateTime<Utc>>,
) -> bool {
    if let Some(last) = account.runtime.last_refresh {
        if now - last < chrono::Duration::minutes(15) {
            return false;
        }
    }
    match exp {
        Some(exp) => now >= exp - chrono::Duration::minutes(5),
        None => account
            .runtime
            .last_refresh
            .map(|last| now - last > chrono::Duration::hours(12))
            .unwrap_or(true),
    }
}

#[cfg(test)]
mod refresh_error_tests {
    use super::refresh_error_is_revoked;

    #[test]
    fn revoked_only_on_structured_oauth_code() {
        // Real OAuth revoked-token bodies -> dead.
        assert!(refresh_error_is_revoked(
            "token refresh failed 400: {\"error\":\"invalid_grant\"}"
        ));
        assert!(refresh_error_is_revoked(
            "claude token refresh failed 403: {\"error\":\"refresh_token_reused\"}"
        ));
        // An unrelated error body that merely MENTIONS the phrase must NOT kill
        // a live account.
        assert!(!refresh_error_is_revoked(
            "token refresh failed 500: {\"error\":\"server_error\",\"detail\":\"invalid_grant types are listed here\"}"
        ));
        assert!(!refresh_error_is_revoked("failed to call token refresh endpoint: timeout"));
    }
}

/// Proactive refresh loop shared by Codex and Claude: every 5 minutes, refresh
/// the due accounts of `provider`. Without this, a pooled account's access
/// token silently lapses and only "comes back" if its owner happens to
/// re-import credentials.
pub(crate) async fn run_token_refresh_loop(
    state: AppState,
    provider: Provider,
    refreshable: fn(&UpstreamAccount) -> bool,
    needs_refresh: fn(&UpstreamAccount, DateTime<Utc>) -> bool,
    refresh: for<'a> fn(
        &'a AppState,
        &'a UpstreamAccount,
    ) -> futures_util::future::BoxFuture<'a, Result<UpstreamAccount, String>>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
    loop {
        tick.tick().await;
        let now = Utc::now();
        let due: Vec<UpstreamAccount> = {
            let accounts = state.accounts.read().await;
            accounts
                .iter()
                .filter(|a| {
                    a.provider == provider.as_str() && !a.runtime.dead && !a.runtime.disabled
                })
                .filter(|a| refreshable(a))
                .filter(|a| needs_refresh(a, now))
                .cloned()
                .collect()
        };
        for account in due {
            match refresh(&state, &account).await {
                Ok(a) => info!(
                    "proactively refreshed {} token for {}",
                    provider.as_str(),
                    a.account_label
                ),
                Err(e) => warn!(
                    "proactive {} refresh failed for {}: {}",
                    provider.as_str(),
                    account.account_label,
                    e
                ),
            }
        }
    }
}

#[cfg(test)]
mod server_tool_tests {
    use super::*;

    #[test]
    fn strips_declarations_and_repoints_tool_choice() {
        let mut payload = json!({
            "model": "claude-sonnet-4-5",
            "tools": [
                { "type": "web_search_20250305", "name": "web_search", "max_uses": 8 },
                { "name": "Read", "input_schema": { "type": "object" } },
                { "type": "custom", "name": "Bash", "input_schema": { "type": "object" } }
            ],
            "tool_choice": { "type": "tool", "name": "web_search" }
        });
        assert_eq!(strip_anthropic_server_tools(&mut payload), 1);

        // Only the server tool is gone; both client shapes (no `type`, and the
        // explicit `type: "custom"`) survive.
        let names: Vec<&str> = payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Read", "Bash"]);
        // tool_choice pointed at the tool we removed, so it falls back to auto.
        assert_eq!(payload["tool_choice"], json!({ "type": "auto" }));
    }

    #[test]
    fn drops_tools_key_entirely_when_only_server_tools_were_declared() {
        let mut payload = json!({
            "tools": [{ "type": "web_search_20250305", "name": "web_search" }],
            "tool_choice": { "type": "auto" }
        });
        assert_eq!(strip_anthropic_server_tools(&mut payload), 1);
        // An empty `tools` (and a tool_choice with nothing to choose) is worse
        // than no tools at all.
        assert!(payload.get("tools").is_none());
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn strips_server_tool_blocks_from_the_transcript() {
        // A session that used web search on real Claude and then fell back.
        let mut payload = json!({
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "search it" }] },
                { "role": "assistant", "content": [
                    { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} },
                    { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] },
                    { "type": "text", "text": "found it" }
                ]},
                { "role": "assistant", "content": [
                    { "type": "server_tool_use", "id": "srvtoolu_2", "name": "web_search", "input": {} }
                ]}
            ]
        });
        assert_eq!(strip_anthropic_server_tools(&mut payload), 3);

        assert_eq!(payload["messages"][1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(payload["messages"][1]["content"][0]["text"], "found it");
        // A turn made entirely of server-tool blocks keeps a placeholder rather
        // than becoming an empty (invalid) content array or vanishing and
        // breaking role alternation.
        assert_eq!(payload["messages"].as_array().unwrap().len(), 3);
        assert_eq!(payload["messages"][2]["content"][0]["type"], "text");
    }

    #[test]
    fn leaves_client_tools_and_client_tool_results_alone() {
        // Client-side MCP tools arrive as plain custom tools, and their results
        // are the bare `tool_result` block — the whole substitute-for-web-search
        // story depends on these surviving.
        let mut payload = json!({
            "tools": [{ "name": "mcp__MiniMax__web_search", "input_schema": { "type": "object" } }],
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "mcp__MiniMax__web_search", "input": {} }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
                ]}
            ]
        });
        assert_eq!(strip_anthropic_server_tools(&mut payload), 0);
        assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
        assert_eq!(payload["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(payload["messages"][1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn only_third_party_anthropic_upstreams_get_stripped() {
        for p in ["glm", "kimi", "trae", "minimax", "deepseek"] {
            assert!(is_third_party_anthropic(p), "{} should be stripped", p);
        }
        // Real Anthropic executes these tools, and Codex's Responses-format
        // function tools would be destroyed by the same rule.
        for p in ["claude", "codex", "cursor", "ollama"] {
            assert!(!is_third_party_anthropic(p), "{} must not be stripped", p);
        }
    }
}
