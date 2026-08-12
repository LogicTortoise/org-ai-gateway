use crate::prelude::*;
use crate::pool::PROMPT_CACHE_BINDING_TTL_SECS;
use crate::auth::identify_caller;
use crate::pool::account_visible_to_user;
use crate::pool::note_account_pick;
use crate::pool::remember_affinity_account;
use crate::pool::resolve_affinity_account;
use crate::pool::select_account_for_request;
use crate::pool::select_account_for_request_with_preference;
use crate::pool::storage::append_audit;
use crate::pool::transient_prompt_cache_key;
use crate::provider::claude::sanitize_claude_messages_payload;
use crate::provider::claude::send_claude_upstream_with_refresh;
use crate::provider::codex::ensure_codex_payload_defaults;
use crate::provider::codex::send_codex_upstream_with_refresh;
use crate::provider::chains::ordered_attempts;
use crate::provider::chains::ChainSlot;
use crate::provider::cursor::CursorFormat;
use crate::provider::deepseek;
use crate::provider::glm;
use crate::provider::kimi;
use crate::provider::minimax;
use crate::provider::ollama::ollama_canonical_model;
use crate::provider::ollama::ollama_http_client;
use crate::provider::ollama::send_ollama_upstream;
use crate::provider::trae;
use crate::retry::ErrorClass;
use crate::retry::apply_account_failure;
use crate::retry::eligible_accounts;
use crate::retry::is_claude_organization_disabled;
use crate::retry::is_cloudflare_challenge;
use crate::retry::is_codex_model_unavailable;
use crate::retry::is_deactivated_workspace;
use crate::retry::parse_retry_after;
use crate::retry::provider_attempt_budget;
use crate::retry::reset_backoff;
use crate::retry::sync_usage_cooldown;
use crate::sse::aggregate_codex_sse_to_response_json;
use crate::sse::extract_output_text_from_sse;
use crate::usage::parse_rate_limit_headers;
use std::collections::HashSet;

/// The result of trying to serve a request via ONE provider in a priority chain.
enum ProviderOutcome {
    /// The provider served the request; this is the client response, verbatim.
    Served(Response),
    /// This provider couldn't serve the request — move on to the next provider
    /// in the chain. Carries the error response to surface to the client *if*
    /// this turns out to be the last provider; `None` means "no usable account"
    /// (a softer condition that a later provider's real error should outrank).
    NextProvider(Option<Response>),
}

impl ProviderOutcome {
    /// Collapse an outcome into a client response for the DIRECT entrypoints
    /// (explicit `ollama/*` / cursor models), which have no further providers to
    /// fall through to. `no_account` is the 4xx used when nothing was selected.
    fn into_response(self, no_account: Response) -> Response {
        match self {
            ProviderOutcome::Served(resp) => resp,
            ProviderOutcome::NextProvider(Some(resp)) => resp,
            ProviderOutcome::NextProvider(None) => no_account,
        }
    }
}

/// Serve a request through the configured priority chain for `slot`, degrading
/// to the next provider on exhaustion / failure (failover), optionally rotating
/// the starting provider each request (round-robin). Returns the first
/// provider's success, or — if every provider is unavailable — the most
/// informative error gathered along the way.
async fn serve_with_chain(
    state: &AppState,
    slot: ChainSlot,
    client_format: CursorFormat,
    user_id: &str,
    payload: &Value,
    client_wants_stream: bool,
    shared_only: bool,
) -> Response {
    let cfg = state.chains.read().await.for_slot(slot).clone();
    // Round-robin rotates the starting offset once per request; failover ignores it.
    let rr_offset = if matches!(cfg.mode, crate::provider::chains::ChainMode::RoundRobin) {
        let mut rr = state.chain_rr.lock().await;
        let counter = rr.entry(slot.as_str().to_string()).or_insert(0);
        let v = *counter;
        *counter = counter.wrapping_add(1);
        v
    } else {
        0
    };
    let order = ordered_attempts(&cfg, rr_offset);

    let mut last_error: Option<Response> = None;
    for provider in &order {
        let outcome = match provider.as_str() {
            "codex" | "claude" => {
                serve_native_provider(
                    state.clone(),
                    provider,
                    client_format,
                    user_id.to_string(),
                    payload.clone(),
                    client_wants_stream,
                    shared_only,
                )
                .await
            }
            "glm" => {
                // Claude-format traffic → GLM's Anthropic-compatible endpoint as
                // a raw buffered passthrough (high fidelity, tool calls survive),
                // served by the native loop. Everything else (Codex / OpenAI
                // format) → GLM's OpenAI-compatible endpoint via the
                // Responses↔Chat adapter (also tool-call preserving).
                if matches!(client_format, CursorFormat::Claude) {
                    serve_native_provider(
                        state.clone(),
                        "glm",
                        client_format,
                        user_id.to_string(),
                        payload.clone(),
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                } else {
                    serve_openai_tool_compat(
                        state,
                        "glm",
                        client_format,
                        user_id,
                        payload,
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                }
            }
            "kimi" => {
                // Same split as GLM: Claude-format traffic → Kimi's Anthropic-
                // compatible endpoint as a raw buffered passthrough (this is the
                // Claude Code fallback path), everything else → Kimi's OpenAI-
                // compatible endpoint via the Responses↔Chat adapter.
                if matches!(client_format, CursorFormat::Claude) {
                    serve_native_provider(
                        state.clone(),
                        "kimi",
                        client_format,
                        user_id.to_string(),
                        payload.clone(),
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                } else {
                    serve_openai_tool_compat(
                        state,
                        "kimi",
                        client_format,
                        user_id,
                        payload,
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                }
            }
            "trae" => {
                // Trae is Claude-only: its sidecar speaks Anthropic `/v1/messages`
                // and nothing OpenAI-shaped, so there is no adapter path. The
                // chain validator already restricts `trae` to the Claude slot, so
                // the else branch is belt-and-suspenders — skip to the next
                // provider rather than mangling the request into a format Trae
                // can't serve.
                if matches!(client_format, CursorFormat::Claude) {
                    serve_native_provider(
                        state.clone(),
                        "trae",
                        client_format,
                        user_id.to_string(),
                        payload.clone(),
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                } else {
                    ProviderOutcome::NextProvider(None)
                }
            }
            "minimax" | "deepseek" => {
                // Dual-protocol providers: Claude-format traffic goes to the
                // Anthropic-compatible surface (raw buffered passthrough — tool
                // calls survive), Codex/OpenAI-format traffic goes to the
                // OpenAI-compatible surface via the Responses↔Chat adapter
                // (also tool-call preserving). The split mirrors GLM / Kimi.
                if matches!(client_format, CursorFormat::Claude) {
                    serve_native_provider(
                        state.clone(),
                        provider,
                        client_format,
                        user_id.to_string(),
                        payload.clone(),
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                } else {
                    serve_openai_tool_compat(
                        state,
                        provider,
                        client_format,
                        user_id,
                        payload,
                        client_wants_stream,
                        shared_only,
                    )
                    .await
                }
            }
            "ollama" => {
                serve_ollama(state, client_format, user_id, payload, client_wants_stream, shared_only).await
            }
            "cursor" => {
                serve_cursor(state, client_format, user_id, payload, shared_only).await
            }
            _ => ProviderOutcome::NextProvider(None),
        };
        match outcome {
            ProviderOutcome::Served(resp) => {
                if provider != order.first().map(|s| s.as_str()).unwrap_or(provider) {
                    info!("chain[{}] served via fallback provider {}", slot.as_str(), provider);
                }
                return resp;
            }
            ProviderOutcome::NextProvider(err) => {
                // Keep the most informative error: a real upstream error (Some)
                // outranks a bare "no account" (None) from another provider.
                if err.is_some() {
                    last_error = err;
                } else if last_error.is_none() {
                    last_error = None;
                }
            }
        }
    }

    last_error.unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": format!("{} 链路上所有 provider 均不可用", slot.as_str()),
                "chain": order,
                "hint": "请连接对应 provider 的账号，或在 UI「优先级链路」中调整顺序",
            })),
        )
            .into_response()
    })
}

pub(crate) async fn proxy_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let origin = infer_request_origin(&headers, &payload, ChainSlot::Codex);
    crate::auth::with_request_origin(origin, async move {
        proxy_responses_inner(state, headers, payload).await
    })
    .await
}

async fn proxy_responses_inner(
    state: AppState,
    headers: HeaderMap,
    mut payload: Value,
) -> Response {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;
    // Cursor models are served by the Cursor upstream (api2.cursor.sh), not Codex.
    if payload_is_cursor(&payload) {
        return serve_cursor(&state, CursorFormat::Responses, &user_id, &payload, shared_only)
            .await
            .into_response(cursor_no_account_response());
    }
    // `ollama/*` models route to a local ollama, peer to the paid providers.
    if payload_is_ollama(&payload) {
        let wants_stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let outcome = serve_ollama(&state, CursorFormat::Responses, &user_id, &payload, wants_stream, shared_only).await;
        return outcome.into_response(ollama_no_account_response());
    }
    // The local client now sends its REAL ChatGPT token (we no longer rewrite
    // auth.json). We don't authenticate it as `user:<id>`; we just identify the
    // caller for audit and route to a shared pool account. Never return 401 here
    // or Codex's auth-recovery flow would fire on the user's real account.
    //
    // Upstream is always called with stream=true (ensure_codex_payload_defaults),
    // so remember what the CLIENT asked for: a non-streaming client must get the
    // aggregated JSON back, not a buffered SSE body.
    let client_wants_stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    ensure_codex_payload_defaults(&mut payload);
    // Route through the Codex priority chain (default `[codex]`; may degrade to
    // GLM / ollama / cursor per the gateway's `/v1/provider/chains` config).
    serve_with_chain(&state, ChainSlot::Codex, CursorFormat::Responses, &user_id, &payload, client_wants_stream, shared_only).await
}

pub(crate) async fn proxy_claude_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let origin = infer_request_origin(&headers, &payload, ChainSlot::Claude);
    crate::auth::with_request_origin(origin, async move {
        proxy_claude_messages_inner(state, headers, payload).await
    })
    .await
}

async fn proxy_claude_messages_inner(
    state: AppState,
    headers: HeaderMap,
    mut payload: Value,
) -> Response {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;
    if payload_is_cursor(&payload) {
        return serve_cursor(&state, CursorFormat::Claude, &user_id, &payload, shared_only)
            .await
            .into_response(cursor_no_account_response());
    }
    if payload_is_ollama(&payload) {
        let wants_stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let outcome = serve_ollama(&state, CursorFormat::Claude, &user_id, &payload, wants_stream, shared_only).await;
        return outcome.into_response(ollama_no_account_response());
    }
    sanitize_claude_messages_payload(&mut payload);
    // Claude passes `stream` through untouched, so the upstream response format
    // already matches the client's request; no aggregation needed.
    let client_wants_stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Route through the Claude priority chain (default `[claude]`; may degrade to
    // GLM (Anthropic-compatible) / ollama / cursor per the gateway config).
    serve_with_chain(&state, ChainSlot::Claude, CursorFormat::Claude, &user_id, &payload, client_wants_stream, shared_only).await
}

/// OpenAI Chat Completions entrypoint. Reserved for cursor-backed models;
/// other models are directed to the native Responses/Messages endpoints.
pub(crate) async fn proxy_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let origin = infer_request_origin(&headers, &payload, ChainSlot::Claude);
    crate::auth::with_request_origin(origin, async move {
        proxy_chat_completions_inner(state, headers, payload).await
    })
    .await
}

async fn proxy_chat_completions_inner(
    state: AppState,
    headers: HeaderMap,
    payload: Value,
) -> Response {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;
    if payload_is_cursor(&payload) {
        return serve_cursor(&state, CursorFormat::OpenAI, &user_id, &payload, shared_only)
            .await
            .into_response(cursor_no_account_response());
    }
    if payload_is_ollama(&payload) {
        let wants_stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let outcome = serve_ollama(&state, CursorFormat::OpenAI, &user_id, &payload, wants_stream, shared_only).await;
        return outcome.into_response(ollama_no_account_response());
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "/v1/chat/completions only serves cursor/* and ollama/* models; use /v1/responses (Codex) or /v1/messages (Claude) for other providers",
        })),
    )
        .into_response()
}

/// Returns true if the request's `model` field selects a Cursor model.
fn payload_is_cursor(payload: &Value) -> bool {
    payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(crate::provider::cursor::is_cursor_model)
        .unwrap_or(false)
}

/// Pick the right audit origin for an entry handler. Cursor / ollama models
/// route to their own upstreams regardless of which endpoint the client hit,
/// so they get their own origin label even when entered through Codex or
/// Claude slot; otherwise the slot determines whether this is Codex CLI or
/// Claude Code traffic (with API-key auth overriding as `api_key`).
fn infer_request_origin(
    headers: &HeaderMap,
    payload: &Value,
    slot: ChainSlot,
) -> String {
    if payload_is_ollama(payload) {
        return crate::auth::ORIGIN_OLLAMA.to_string();
    }
    if payload_is_cursor(payload) {
        return crate::auth::ORIGIN_CURSOR.to_string();
    }
    crate::auth::infer_origin(headers, slot).to_string()
}

/// Returns true if the request's `model` field selects an ollama model
/// (`ollama/<name>` or a bare `ollama`).
fn payload_is_ollama(payload: &Value) -> bool {
    payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(crate::provider::ollama::is_ollama_model)
        .unwrap_or(false)
}

/// Serves a chat request via the Cursor upstream (`api2.cursor.sh`, Connect-RPC).
/// Mirrors the account-swap retry shape of `proxy_provider`: select a cursor
/// account, POST the protobuf-encoded request, classify the result, and on
/// failure penalize + swap to the next account. The reply is rendered back in
/// the client's request format.
async fn serve_cursor(
    state: &AppState,
    format: CursorFormat,
    user_id: &str,
    payload: &Value,
    shared_only: bool,
) -> ProviderOutcome {
    use crate::provider::cursor;

    let owned_only = match crate::quota::enforce_user_quota(state, "cursor", user_id, !shared_only).await {
        Ok(v) => v,
        Err(resp) => return ProviderOutcome::NextProvider(Some(resp)),
    };

    let raw_model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("cursor");
    let upstream_model = cursor::cursor_canonical_model(raw_model);

    let req = match cursor::extract_request(payload) {
        Ok(v) => v,
        Err(e) => {
            return ProviderOutcome::NextProvider(Some(
                (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
            ));
        }
    };
    let prompt_chars: usize = req.instruction.chars().count()
        + req.turns.iter().map(|t| t.content.chars().count()).sum::<usize>();
    let input_token_estimate = cursor::estimate_request_tokens(&req);
    let stream = req.stream;
    let request_id = Uuid::new_v4().to_string();

    let client = cursor::cursor_http_client();

    let max_attempts = provider_attempt_budget(state, "cursor").await;
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;
    // Final-failure audit data; intermediate retried attempts are only traced
    // (one client request, one audit record).
    let mut pending_failure_audit: Option<(UpstreamAccount, String)> = None;

    for _ in 0..max_attempts {
        let now = Utc::now();
        let selected = {
            let accounts = state.accounts.read().await;
            let rate_limits = state.rate_limits.read().await;
            let owner_usage = state.owner_usage.read().await;
            let mut warm = eligible_accounts(&accounts, "cursor", user_id, &excluded, now, false);
            if owned_only {
                warm.retain(|a| a.owner_user_id == user_id);
            }
            if shared_only {
                warm.retain(|a| a.share_enabled);
            }
            let mut sel = select_account_for_request(&warm, user_id, "cursor", &rate_limits, &owner_usage);
            if sel.is_none() {
                let mut cooling = eligible_accounts(&accounts, "cursor", user_id, &excluded, now, true);
                if owned_only {
                    cooling.retain(|a| a.owner_user_id == user_id);
                }
                if shared_only {
                    cooling.retain(|a| a.share_enabled);
                }
                let cooling = crate::retry::prefer_near_expiry(cooling, now);
                sel = select_account_for_request(&cooling, user_id, "cursor", &rate_limits, &owner_usage);
            }
            sel
        };
        let Some(account) = selected else { break };
        selected_any = true;
        excluded.insert(account.id.clone());
        note_account_pick(state, &account.id).await;

        let now_ms = Utc::now().timestamp_millis();
        let result = match cursor::send_cursor_upstream(client, &account, &upstream_model, &req, now_ms).await {
            Ok(r) => r,
            Err(err) => {
                // Transport error — penalize lightly and try the next account.
                apply_account_failure(state, &account.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": "cursor" })));
                continue;
            }
        };

        // Upstream-reported failure (HTTP error status or a JSON control frame).
        if !result.status.is_success() || (result.text.is_empty() && result.error.is_some()) {
            let detail = result.error.clone().unwrap_or_else(|| {
                format!("cursor upstream returned {}", result.status)
            });
            let rate_limited = cursor::looks_rate_limited(&detail);
            let class = if rate_limited {
                ErrorClass::RateLimit
            } else {
                ErrorClass::from_status(result.status.as_u16())
            };
            apply_account_failure(state, &account.id, class, None, None, false).await;
            info!(
                "cursor_error_{} on {} ({})",
                result.status.as_u16(),
                account.account_label,
                if class.is_retryable() { "retrying on next account" } else { "final" },
            );
            pending_failure_audit = Some((
                account.clone(),
                format!("cursor_error_{}", result.status.as_u16()),
            ));
            let status = if result.status.is_success() {
                StatusCode::BAD_GATEWAY
            } else {
                result.status
            };
            last_error = Some((status, json!({ "error": detail, "provider": "cursor" })));
            if class.is_retryable() {
                continue;
            }
            break;
        }

        // Success: clear backoff, audit, and render the reply.
        reset_backoff(state, &account.id).await;
        let output_chars = result.text.chars().count();
        write_proxy_audit(
            state, user_id, &account, "cursor", &upstream_model, prompt_chars, output_chars,
            "success", TokenUsage::default(),
        )
        .await;

        if stream {
            let sse = cursor::build_sse_body(format, &request_id, raw_model, &result.text, input_token_estimate);
            let mut response = Response::new(sse.into());
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream; charset=utf-8"),
            );
            return ProviderOutcome::Served(response);
        }
        let body = cursor::build_buffered_body(format, &request_id, raw_model, &result.text, input_token_estimate);
        return ProviderOutcome::Served((StatusCode::OK, Json(body)).into_response());
    }

    // The request is finally failing — write the single failure audit record.
    if last_error.is_some() {
        if let Some((account, status_label)) = pending_failure_audit {
            write_proxy_audit(
                state, user_id, &account, "cursor", &upstream_model, prompt_chars, 0,
                &status_label, TokenUsage::default(),
            )
            .await;
        }
    }

    match last_error {
        Some((status, body)) => ProviderOutcome::NextProvider(Some((status, Json(body)).into_response())),
        None if !selected_any => ProviderOutcome::NextProvider(None),
        None => ProviderOutcome::NextProvider(Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "all cursor accounts exhausted", "provider": "cursor" })),
            )
                .into_response(),
        )),
    }
}

/// The 4xx returned when an explicit `cursor` model is requested but no cursor
/// account is connected (the direct entrypoint has nothing to fall through to).
fn cursor_no_account_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "no cursor account available for this user",
            "hint": "先连接一个 Cursor 账号 (POST /v1/provider/connect/cursor)",
        })),
    )
        .into_response()
}

/// The 4xx returned when an explicit `ollama/*` model is requested but no ollama
/// account is connected (the direct entrypoint has nothing to fall through to).
fn ollama_no_account_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "no ollama account available for this user",
            "hint": "先连接一个本地 ollama (POST /v1/provider/connect/ollama)，或设置 OLLAMA_BASE_URL",
        })),
    )
        .into_response()
}

/// Serve a chat request via a local ollama (`/api/chat`). Used both as the
/// direct entry for `model=ollama/<name>` and as a member of a priority chain.
/// ollama is free and non-metered, so (unlike the paid providers) there is NO
/// per-user token-budget gate here — the only access constraint kept is
/// `shared_only` (untrusted callers may touch shared accounts only). Token usage
/// IS still audited (real counts from the response) so the dashboard shows local
/// consumption.
async fn serve_ollama(
    state: &AppState,
    format: CursorFormat,
    user_id: &str,
    payload: &Value,
    client_wants_stream: bool,
    shared_only: bool,
) -> ProviderOutcome {
    use crate::provider::cursor::{build_buffered_body, build_sse_body, estimate_request_tokens, extract_request};

    let req = match extract_request(payload) {
        Ok(v) => v,
        Err(e) => {
            return ProviderOutcome::NextProvider(Some(
                (StatusCode::BAD_REQUEST, Json(json!({ "error": e, "provider": "ollama" }))).into_response(),
            ));
        }
    };

    let raw_model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("ollama");
    // Direct `ollama/<name>` → that model; fallback (a paid model name) → the
    // configured default ollama model.
    let upstream_model = if crate::provider::ollama::is_ollama_model(raw_model) {
        ollama_canonical_model(raw_model)
    } else {
        ollama_canonical_model("ollama")
    };
    let request_id = Uuid::new_v4().to_string();
    let request_json_chars = payload.to_string().chars().count();
    let estimated_input = estimate_request_tokens(&req);
    let client = ollama_http_client();

    let max_attempts = provider_attempt_budget(state, "ollama").await;
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;

    for _ in 0..max_attempts {
        let now = Utc::now();
        let selected = {
            let accounts = state.accounts.read().await;
            let rate_limits = state.rate_limits.read().await;
            let owner_usage = state.owner_usage.read().await;
            let mut warm = eligible_accounts(&accounts, "ollama", user_id, &excluded, now, true);
            if shared_only {
                warm.retain(|a| a.share_enabled);
            }
            select_account_for_request(&warm, user_id, "ollama", &rate_limits, &owner_usage)
        };
        let Some(account) = selected else { break };
        selected_any = true;
        excluded.insert(account.id.clone());
        note_account_pick(state, &account.id).await;

        let result = match send_ollama_upstream(client, &account, &upstream_model, &req).await {
            Ok(r) => r,
            Err(err) => {
                // Transport error (ollama down / unreachable): penalize lightly
                // and try the next endpoint, if any.
                apply_account_failure(state, &account.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": "ollama" })));
                continue;
            }
        };

        if !result.status.is_success() || (result.text.is_empty() && result.error.is_some()) {
            let detail = result
                .error
                .clone()
                .unwrap_or_else(|| format!("ollama upstream returned {}", result.status));
            let class = ErrorClass::from_status(result.status.as_u16());
            apply_account_failure(state, &account.id, class, None, None, false).await;
            info!(
                "ollama_error_{} on {} ({})",
                result.status.as_u16(),
                account.account_label,
                if class.is_retryable() { "retrying on next account" } else { "final" },
            );
            let status = if result.status.is_success() {
                StatusCode::BAD_GATEWAY
            } else {
                result.status
            };
            last_error = Some((status, json!({ "error": detail, "provider": "ollama" })));
            if class.is_retryable() {
                continue;
            }
            break;
        }

        // Success: clear backoff, audit with REAL token usage, render the reply.
        reset_backoff(state, &account.id).await;
        let input_tokens = if result.usage.input_tokens > 0 {
            result.usage.input_tokens as u64
        } else {
            estimated_input
        };
        write_proxy_audit(
            state, user_id, &account, "ollama", &upstream_model, request_json_chars, result.text.len(),
            "success", result.usage,
        )
        .await;

        if client_wants_stream {
            let sse = build_sse_body(format, &request_id, raw_model, &result.text, input_tokens);
            let mut response = Response::new(sse.into());
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream; charset=utf-8"),
            );
            return ProviderOutcome::Served(response);
        }
        let body = build_buffered_body(format, &request_id, raw_model, &result.text, input_tokens);
        return ProviderOutcome::Served((StatusCode::OK, Json(body)).into_response());
    }

    match last_error {
        Some((status, body)) => ProviderOutcome::NextProvider(Some((status, Json(body)).into_response())),
        None if !selected_any => ProviderOutcome::NextProvider(None),
        None => ProviderOutcome::NextProvider(Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "all ollama accounts exhausted", "provider": "ollama" })),
            )
                .into_response(),
        )),
    }
}

/// Serve a request via one of the OpenAI-compatible adapters
/// (`glm` / `kimi` / `minimax` / `deepseek`) — these all share the same
/// shape: a provider-local Responses↔Chat Completions adapter that preserves
/// `function_call` / `function_call_output` round-trips, real token usage
/// parsed from the upstream `usage` block, and per-provider account-swap retry
/// with the metered per-user token-budget quota gate. Streaming is real
/// per-token SSE translation via the shared `stream_openai_to_responses_sse`
/// helper. The four providers differ only in (a) which `send_*_openai` /
/// `send_*_openai_streaming` module is called and (b) how the model id is
/// rewritten before the upstream call — handled by the per-provider `match`
/// ladders inside.
async fn serve_openai_tool_compat(
    state: &AppState,
    provider: &str,
    format: CursorFormat,
    user_id: &str,
    payload: &Value,
    client_wants_stream: bool,
    shared_only: bool,
) -> ProviderOutcome {
    debug_assert!(matches!(provider, "glm" | "kimi" | "minimax" | "deepseek"));
    // Only the Codex / Responses path is wired through the OpenAI surface.
    // Chat Completions format never reaches here (the `/v1/chat/completions`
    // entrypoint rejects non-cursor / non-ollama models with 400); Claude
    // format is handled by `serve_native_provider`. Belt-and-suspenders for
    // the same reason `trae` skips: don't try to render Responses-shaped
    // output through a Chat Completions client, etc.
    if !matches!(format, CursorFormat::Responses) {
        return ProviderOutcome::NextProvider(None);
    }

    let owned_only = match crate::quota::enforce_user_quota(state, provider, user_id, !shared_only).await {
        Ok(v) => v,
        Err(resp) => return ProviderOutcome::NextProvider(Some(resp)),
    };

    let raw_model = payload.get("model").and_then(|v| v.as_str()).unwrap_or(provider);
    // Model rewriting per provider: each provider's `canonical_model` knows
    // its own tier rewrite (GLM / Kimi fall back to a single built-in id,
    // minimax fixes case + maps claude tiers, deepseek maps Anthropic tier
    // names). The OpenAI path sends this id verbatim to the upstream — the
    // converters don't touch `model`.
    let upstream_model = match provider {
        "glm" => glm::glm_canonical_model(raw_model),
        "kimi" => kimi::kimi_canonical_model(raw_model),
        "minimax" => minimax::minimax_canonical_model(raw_model),
        "deepseek" => deepseek_openai_model_for(raw_model),
        _ => raw_model.to_string(),
    };

    let max_attempts = provider_attempt_budget(state, provider).await;
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;
    let request_json_chars = payload.to_string().chars().count();

    for _ in 0..max_attempts {
        let now = Utc::now();
        let selected = {
            let accounts = state.accounts.read().await;
            let rate_limits = state.rate_limits.read().await;
            let owner_usage = state.owner_usage.read().await;
            let mut warm = eligible_accounts(&accounts, provider, user_id, &excluded, now, true);
            if owned_only {
                warm.retain(|a| a.owner_user_id == user_id);
            }
            if shared_only {
                warm.retain(|a| a.share_enabled);
            }
            // Only accounts that expose the OpenAI-compatible endpoint can
            // serve this adapter path — same filter every provider's
            // adapter uses for the same reason.
            match provider {
                "glm" => warm.retain(glm::supports_openai),
                "kimi" => warm.retain(kimi::supports_openai),
                "minimax" => warm.retain(minimax::supports_openai),
                "deepseek" => warm.retain(deepseek::supports_openai),
                _ => {}
            }
            select_account_for_request(&warm, user_id, provider, &rate_limits, &owner_usage)
        };
        let Some(account) = selected else { break };
        selected_any = true;
        excluded.insert(account.id.clone());
        note_account_pick(state, &account.id).await;

        // Dispatch into the provider's own sender + error parser. Each one
        // accepts the ORIGINAL payload (no `extract_request` rewriting) and
        // returns parsed text + tool_calls + real usage.
        let send_outcome = match provider {
            "glm" => glm::send_glm_openai(&account, &upstream_model, payload)
                .await
                .map(|r| (
                    r.status,
                    r.text,
                    r.error,
                    r.usage,
                    r.tool_calls.into_iter().map(glm_tool_to_common).collect::<Vec<_>>(),
                )),
            "kimi" => kimi::send_kimi_openai(&account, &upstream_model, payload)
                .await
                .map(|r| (
                    r.status,
                    r.text,
                    r.error,
                    r.usage,
                    r.tool_calls.into_iter().map(kimi_tool_to_common).collect::<Vec<_>>(),
                )),
            "minimax" => minimax::send_minimax_openai(&account, &upstream_model, payload)
                .await
                .map(|r| (
                    r.status,
                    r.text,
                    r.error,
                    r.usage,
                    r.tool_calls.into_iter().map(minimax_tool_to_common).collect::<Vec<_>>(),
                )),
            "deepseek" => deepseek::send_deepseek_openai(&account, &upstream_model, payload)
                .await
                .map(|r| (
                    r.status,
                    r.text,
                    r.error,
                    r.usage,
                    r.tool_calls.into_iter().map(deepseek_tool_to_common).collect::<Vec<_>>(),
                )),
            _ => unreachable!("debug_assert above"),
        };
        let (status, text, error, usage, tool_calls): (
            reqwest::StatusCode,
            String,
            Option<String>,
            TokenUsage,
            Vec<CommonToolCall>,
        ) = match send_outcome {
            Ok(v) => v,
            Err(err) => {
                apply_account_failure(state, &account.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": provider })));
                continue;
            }
        };

        if !status.is_success() || (text.is_empty() && error.is_some()) {
            let detail = error.unwrap_or_else(|| format!("{} upstream returned {}", provider, status));
            let class = ErrorClass::from_status(status.as_u16());
            apply_account_failure(state, &account.id, class, None, None, false).await;
            info!(
                "{}_error_{} on {} ({})",
                provider,
                status.as_u16(),
                account.account_label,
                if class.is_retryable() { "retrying on next account" } else { "final" },
            );
            let resp_status = if status.is_success() { StatusCode::BAD_GATEWAY } else { status };
            last_error = Some((resp_status, json!({ "error": detail, "provider": provider })));
            if class.is_retryable() {
                continue;
            }
            break;
        }

        // Success: clear backoff, audit with REAL token usage, render the
        // reply back in the Codex Responses shape so tool calls survive.
        reset_backoff(state, &account.id).await;
        // Audit the model that ANSWERED (the upstream echoed it back in its
        // own response body in some cases) — fall back to the rewritten
        // upstream model otherwise.
        let effective_model = raw_model.to_string();
        let input_tokens = usage.input_tokens;
        let cached_input_tokens = usage.cached_input_tokens;
        let output_tokens = usage.output_tokens;
        let reasoning_tokens = usage.reasoning_tokens;
        write_proxy_audit(
            state, user_id, &account, provider, &effective_model,
            request_json_chars, text.len(), "success", usage,
        )
        .await;

        if client_wants_stream {
            // Per-delta translation: open the upstream Chat Completions
            // stream and pipe each chunk through
            // `translate_openai_sse_to_responses`, which emits real Responses
            // SSE events (`response.output_text.delta`,
            // `response.function_call_arguments.delta`, …) so a streaming
            // Codex client sees incremental token-by-token delivery. Audit is
            // written by the spawned task once translation completes (or
            // fails); this path bypasses the aggregated JSON body entirely.
            let send_stream_outcome = match provider {
                "glm" => {
                    glm::send_glm_openai_streaming(&account, &upstream_model, payload).await
                }
                "kimi" => {
                    kimi::send_kimi_openai_streaming(&account, &upstream_model, payload).await
                }
                "minimax" => {
                    minimax::send_minimax_openai_streaming(&account, &upstream_model, payload).await
                }
                "deepseek" => {
                    deepseek::send_deepseek_openai_streaming(&account, &upstream_model, payload).await
                }
                _ => unreachable!("debug_assert above"),
            };
            match send_stream_outcome {
                Ok(upstream_resp) => {
                    let status = upstream_resp.status();
                    if !status.is_success() {
                        // Drain the upstream body so the connection can be
                        // reused (reqwest keeps it in the pool until consumed).
                        let body = upstream_resp.text().await.unwrap_or_default();
                        let detail = parse_openai_error_message(&body)
                            .unwrap_or_else(|| format!("{} upstream returned {}", provider, status));
                        let class = ErrorClass::from_status(status.as_u16());
                        apply_account_failure(state, &account.id, class, None, None, false).await;
                        info!(
                            "{}_stream_error_{} on {} ({})",
                            provider,
                            status.as_u16(),
                            account.account_label,
                            if class.is_retryable() { "retrying on next account" } else { "final" },
                        );
                        let resp_status = if status.is_success() { StatusCode::BAD_GATEWAY } else { status };
                        last_error = Some((resp_status, json!({ "error": detail, "provider": provider })));
                        if class.is_retryable() {
                            continue;
                        }
                        break;
                    }
                    // Upstream accepted the stream — clear backoff (already
                    // done above), hand the live `reqwest::Response` to the
                    // translator, and return the streaming body. The
                    // translator writes its own audit record on completion.
                    let response = stream_openai_to_responses_sse(
                        state.clone(),
                        account.clone(),
                        user_id.to_string(),
                        provider.to_string(),
                        raw_model.to_string(),
                        upstream_resp,
                        request_json_chars,
                    )
                    .await;
                    return ProviderOutcome::Served(response);
                }
                Err(err) => {
                    // Transport error opening the stream — same penalty as
                    // the buffered path's transport error.
                    apply_account_failure(state, &account.id, ErrorClass::Transient, None, None, false).await;
                    last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": provider })));
                    continue;
                }
            }
        }

        // Non-streaming path: render the aggregated Responses JSON.
        let request_id = format!("resp_{}", uuid::Uuid::new_v4());
        let body = json!({
            "id": request_id,
            "object": "response",
            "created_at": Utc::now().timestamp(),
            "model": effective_model,
            "status": "completed",
            "output": build_common_output_items(&text, &tool_calls),
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": { "cached_tokens": cached_input_tokens },
                "output_tokens": output_tokens,
                "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
                "total_tokens": input_tokens + output_tokens,
            },
        });
        return ProviderOutcome::Served((StatusCode::OK, Json(body)).into_response());
    }

    match last_error {
        Some((status, body)) => ProviderOutcome::NextProvider(Some((status, Json(body)).into_response())),
        None if !selected_any => ProviderOutcome::NextProvider(None),
        None => ProviderOutcome::NextProvider(Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("all {} accounts exhausted", provider), "provider": provider })),
            )
                .into_response(),
        )),
    }
}

/// Resolve the upstream model id for the DeepSeek OpenAI path. The two
/// surfaces publish different ids (`deepseek-v4-pro` ↛ `deepseek-chat`), so
/// the OpenAI path routes everything to a single configurable id; the input
/// name is intentionally not consulted.
fn deepseek_openai_model_for(_raw: &str) -> String {
    deepseek::deepseek_openai_canonical_model("deepseek")
}

/// Parse OpenAI Chat Completions / Responses-style error bodies, shared by the
/// minimax and deepseek streaming paths (both their OpenAI surfaces return the
/// standard `{"error":{"message":"…","type":"…"}}` shape on non-success).
/// Accepts either a flat `"error": "msg"` (some non-success SSE prelude bodies)
/// or the nested Anthropic-flavored shape; returns `None` when neither form is
/// present so the caller can fall back to a generic status-coded message.
fn parse_openai_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_string());
    }
    err.get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

/// Provider-agnostic tool-call record produced by the adapter, so the
/// rendering code below doesn't care which provider produced it.
#[derive(Clone)]
struct CommonToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn minimax_tool_to_common(t: minimax::MinimaxToolCall) -> CommonToolCall {
    CommonToolCall { id: t.id, name: t.name, arguments: t.arguments }
}

fn deepseek_tool_to_common(t: deepseek::DeepseekToolCall) -> CommonToolCall {
    CommonToolCall { id: t.id, name: t.name, arguments: t.arguments }
}

fn glm_tool_to_common(t: glm::GlmToolCall) -> CommonToolCall {
    CommonToolCall { id: t.id, name: t.name, arguments: t.arguments }
}

fn kimi_tool_to_common(t: kimi::KimiToolCall) -> CommonToolCall {
    CommonToolCall { id: t.id, name: t.name, arguments: t.arguments }
}

/// Build the `output` array of a Responses-shaped response: one assistant
/// `message` block carrying any text, then one `function_call` block per
/// parsed tool call. Empty input gets a synthesized placeholder so the array
/// is never empty.
fn build_common_output_items(text: &str, tool_calls: &[CommonToolCall]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    if !text.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }]
        }));
    }
    for tc in tool_calls {
        items.push(json!({
            "type": "function_call",
            "id": format!("fc_{}", tc.id),
            "call_id": tc.id,
            "name": tc.name,
            "arguments": tc.arguments,
        }));
    }
    if items.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "" }]
        }));
    }
    items
}

/// Build the `usage` block of a Responses-shaped response from a parsed
/// `TokenUsage`. Matches the shape the non-streaming path emits so the client
/// sees identical billing telemetry regardless of which transport served it.
fn build_usage_for_responses(usage: &TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": { "cached_tokens": usage.cached_input_tokens },
        "output_tokens": usage.output_tokens,
        "output_tokens_details": { "reasoning_tokens": usage.reasoning_tokens },
        "total_tokens": usage.input_tokens + usage.output_tokens,
    })
}

/// Take ownership of an open `reqwest::Response` carrying the upstream
/// Chat Completions SSE stream, return an `axum::body::Body` that pulls
/// translated Responses SSE events from an mpsc channel, and spawn the
/// translator task that feeds it. Audit is written by the spawned task
/// once translation completes (success or failure) so the streaming path
/// keeps parity with the buffered path's `write_proxy_audit` call. The
/// response status mirrors the upstream's (already validated to be 2xx by
/// the caller) so 5xx is impossible here; the channel carries only 200 OK
/// content.
async fn stream_openai_to_responses_sse(
    state: AppState,
    account: UpstreamAccount,
    user_id: String,
    provider: String,
    raw_model: String,
    upstream: reqwest::Response,
    request_json_chars: usize,
) -> Response {
    let upstream_status = upstream.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);

    // Move everything the spawned task needs (state, account, identity) into
    // the closure — `upstream` is consumed by the translator and can no
    // longer be borrowed by `serve_openai_tool_compat`'s loop.
    let provider_for_task = provider.clone();
    let raw_model_for_task = raw_model.clone();
    let user_id_for_task = user_id.clone();
    let account_for_task = account.clone();
    // `tokio::task_local!` does NOT propagate into spawned tasks by default,
    // so explicitly wrap the spawn in `with_request_origin` with the current
    // request's origin — otherwise the streaming translate's audit row
    // would land with an empty origin (legacy "unknown" bucket).
    let spawn_origin = crate::auth::current_origin().unwrap_or_default();
    tokio::spawn(crate::auth::with_request_origin(spawn_origin, async move {
        match translate_openai_sse_to_responses(upstream, &raw_model_for_task, tx).await {
            Ok((text, tool_calls, usage)) => {
                write_proxy_audit(
                    &state,
                    &user_id_for_task,
                    &account_for_task,
                    &provider_for_task,
                    &raw_model_for_task,
                    request_json_chars,
                    text.len(),
                    "success",
                    usage,
                )
                .await;
                // Drop tool_calls explicitly to make the no-op intentional
                // (the streaming audit only needs byte counts + usage — the
                // translated events already carry every tool-call detail to
                // the client).
                let _ = tool_calls;
            }
            Err(e) => {
                error!(
                    "{} streaming translate failed on {}: {}",
                    provider_for_task, account_for_task.account_label, e
                );
                write_proxy_audit(
                    &state,
                    &user_id_for_task,
                    &account_for_task,
                    &provider_for_task,
                    &raw_model_for_task,
                    request_json_chars,
                    0,
                    "stream_translate_error",
                    TokenUsage::default(),
                )
                .await;
            }
        }
    }));

    // Bridge the mpsc receiver into a `Stream<Item = Result<Bytes, io::Error>>`
    // for `Body::from_stream`. `futures_util::stream::unfold` is the
    // tokio-stream-free equivalent of `ReceiverStream`.
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let body = axum::body::Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = upstream_status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
}

/// Translate a live Chat Completions SSE stream into Responses SSE events.
/// Reads upstream chunks incrementally (no full buffering), parses each
/// `data: {…}` frame, and emits:
///   - `response.created` once, at the start
///   - per output item:
///     - `response.output_item.added`
///     - text → repeated `response.output_text.delta`, then
///       `response.output_text.done` + `response.content_part.done`
///     - tool_call → repeated `response.function_call_arguments.delta`, then
///       `response.function_call_arguments.done`
///     - `response.output_item.done`
///   - `response.completed` + `data: [DONE]` at the end
///
/// Tracks current-item state (message ↔ function_call) so an item is closed
/// (`output_item.done`) when the next delta starts a new one, when
/// `finish_reason` fires, or when the upstream stream ends. The accumulated
/// text + final tool_calls + parsed usage are returned so the spawned task
/// can write the audit record.
///
/// Returns Err on upstream read failure (the mpsc will close naturally,
/// axum will end the body); on client disconnect, every subsequent `tx.send`
/// fails and the translator returns Err immediately to unblock the task.
async fn translate_openai_sse_to_responses(
    upstream: reqwest::Response,
    raw_model: &str,
    tx: tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
) -> Result<(String, Vec<CommonToolCall>, TokenUsage), String> {
    let response_id = format!("resp_{}", uuid::Uuid::new_v4());
    send_sse_event(
        &tx,
        "response.created",
        &json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": Utc::now().timestamp(),
                "status": "in_progress",
                "model": raw_model,
                "output": [],
            }
        }),
    )
    .await?;

    let mut bytes_stream = upstream.bytes_stream();
    let mut buf = String::new();
    let mut accumulated_text = String::new();
    let mut final_tool_calls: Vec<CommonToolCall> = Vec::new();
    let mut final_usage = TokenUsage::default();

    // Per-output-item state. Resets each time `output_item.done` is emitted.
    let mut current_output_index: usize = 0;
    let mut current_item_id: Option<String> = None;
    let mut current_item_type: Option<String> = None;
    let mut current_item_started: bool = false;
    let mut current_text: String = String::new();
    let mut current_tool_call: Option<CommonToolCall> = None;

    loop {
        let chunk_result = bytes_stream.next().await;
        let chunk = match chunk_result {
            Some(Ok(c)) => c,
            Some(Err(e)) => return Err(format!("upstream read error: {}", e)),
            None => break,
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl_pos) = buf.find('\n') {
            let line: String = buf[..nl_pos].to_string();
            buf = buf[nl_pos + 1..].to_string();
            let line = line.trim_end_matches('\r').to_string();
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("data:") else { continue };
            let rest = rest.trim_start();

            if rest == "[DONE]" {
                // Terminal event. Close whatever item is still open.
                finalize_output_item(
                    &tx,
                    &mut current_item_id,
                    &mut current_item_type,
                    &mut current_item_started,
                    &mut current_text,
                    &mut current_tool_call,
                    &mut final_tool_calls,
                    current_output_index,
                )
                .await?;

                let response_obj = json!({
                    "id": response_id,
                    "object": "response",
                    "status": "completed",
                    "model": raw_model,
                    "output": build_common_output_items(&accumulated_text, &final_tool_calls),
                    "usage": build_usage_for_responses(&final_usage),
                });
                send_sse_event(
                    &tx,
                    "response.completed",
                    &json!({ "type": "response.completed", "response": response_obj }),
                )
                .await?;
                let _ = tx.send(Ok(axum::body::Bytes::from("data: [DONE]\n\n"))).await;
                return Ok((accumulated_text, final_tool_calls, final_usage));
            }
            if rest.is_empty() {
                continue;
            }

            let v: Value = match serde_json::from_str(rest) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // `usage` is often attached to the last delta (or arrives as a
            // standalone trailing chunk on OpenAI-compatible APIs). Apply it
            // progressively so the terminal `response.completed` already has
            // the right counts.
            if let Some(usage) = v.get("usage") {
                if let Some(input) = usage.get("prompt_tokens").and_then(|x| x.as_i64()) {
                    final_usage.input_tokens = input;
                }
                if let Some(output) = usage.get("completion_tokens").and_then(|x| x.as_i64()) {
                    final_usage.output_tokens = output;
                }
                if let Some(cached) = usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(|x| x.as_i64())
                {
                    final_usage.cached_input_tokens = cached;
                }
                if let Some(reasoning) = usage
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(|x| x.as_i64())
                {
                    final_usage.reasoning_tokens = reasoning;
                }
            }

            let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else { continue };
            for choice in choices {
                let Some(delta) = choice.get("delta") else { continue };

                // ─── text delta ───
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        if current_item_type.as_deref() != Some("message") {
                            finalize_output_item(
                                &tx,
                                &mut current_item_id,
                                &mut current_item_type,
                                &mut current_item_started,
                                &mut current_text,
                                &mut current_tool_call,
                                &mut final_tool_calls,
                                current_output_index,
                            )
                            .await?;
                            current_output_index += 1;
                            current_item_id = Some(format!("msg_{}", uuid::Uuid::new_v4()));
                            current_item_type = Some("message".to_string());
                            current_item_started = false;
                            current_text.clear();
                        }
                        if !current_item_started {
                            send_sse_event(
                                &tx,
                                "response.output_item.added",
                                &json!({
                                    "type": "response.output_item.added",
                                    "output_index": current_output_index,
                                    "item": {
                                        "id": current_item_id.clone().unwrap(),
                                        "type": "message",
                                        "role": "assistant",
                                        "content": [],
                                    }
                                }),
                            )
                            .await?;
                            current_item_started = true;
                        }
                        current_text.push_str(content);
                        accumulated_text.push_str(content);
                        send_sse_event(
                            &tx,
                            "response.output_text.delta",
                            &json!({
                                "type": "response.output_text.delta",
                                "item_id": current_item_id.clone().unwrap(),
                                "output_index": current_output_index,
                                "content_index": 0,
                                "delta": content,
                            }),
                        )
                        .await?;
                    }
                }

                // ─── tool_calls delta ───
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc_delta in tcs {
                        // New tool call: a non-empty `id` marks a new
                        // function_call item — close whatever's open first.
                        if let Some(new_id) =
                            tc_delta.get("id").and_then(|x| x.as_str())
                        {
                            if !new_id.is_empty() {
                                finalize_output_item(
                                    &tx,
                                    &mut current_item_id,
                                    &mut current_item_type,
                                    &mut current_item_started,
                                    &mut current_text,
                                    &mut current_tool_call,
                                    &mut final_tool_calls,
                                    current_output_index,
                                )
                                .await?;
                                current_output_index += 1;
                                current_item_id = Some(format!("fc_{}", uuid::Uuid::new_v4()));
                                current_item_type = Some("function_call".to_string());
                                current_item_started = false;
                                current_tool_call = Some(CommonToolCall {
                                    id: new_id.to_string(),
                                    name: String::new(),
                                    arguments: String::new(),
                                });
                            }
                        }
                        if let Some(name) = tc_delta
                            .pointer("/function/name")
                            .and_then(|x| x.as_str())
                        {
                            if !name.is_empty() {
                                if let Some(ref mut tc) = current_tool_call {
                                    tc.name = name.to_string();
                                }
                            }
                        }
                        if let Some(args) = tc_delta
                            .pointer("/function/arguments")
                            .and_then(|x| x.as_str())
                        {
                            if !args.is_empty() {
                                if !current_item_started {
                                    let tc = current_tool_call.clone().unwrap();
                                    send_sse_event(
                                        &tx,
                                        "response.output_item.added",
                                        &json!({
                                            "type": "response.output_item.added",
                                            "output_index": current_output_index,
                                            "item": {
                                                "id": current_item_id.clone().unwrap(),
                                                "type": "function_call",
                                                "call_id": tc.id,
                                                "name": tc.name,
                                                "arguments": "",
                                            }
                                        }),
                                    )
                                    .await?;
                                    current_item_started = true;
                                }
                                if let Some(ref mut tc) = current_tool_call {
                                    tc.arguments.push_str(args);
                                }
                                send_sse_event(
                                    &tx,
                                    "response.function_call_arguments.delta",
                                    &json!({
                                        "type": "response.function_call_arguments.delta",
                                        "item_id": current_item_id.clone().unwrap(),
                                        "output_index": current_output_index,
                                        "delta": args,
                                    }),
                                )
                                .await?;
                            }
                        }
                    }
                }

                // ─── finish_reason closes the current item ───
                if let Some(reason) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                    if !reason.is_empty() && reason != "null" {
                        finalize_output_item(
                            &tx,
                            &mut current_item_id,
                            &mut current_item_type,
                            &mut current_item_started,
                            &mut current_text,
                            &mut current_tool_call,
                            &mut final_tool_calls,
                            current_output_index,
                        )
                        .await?;
                        current_output_index += 1;
                        current_item_id = None;
                        current_item_type = None;
                        current_item_started = false;
                    }
                }
            }
        }
    }

    // Stream ended without an explicit `[DONE]` (idle timeout, abrupt close).
    // Still emit the terminal events so a Codex client doesn't hang on
    // `response.completed`.
    finalize_output_item(
        &tx,
        &mut current_item_id,
        &mut current_item_type,
        &mut current_item_started,
        &mut current_text,
        &mut current_tool_call,
        &mut final_tool_calls,
        current_output_index,
    )
    .await?;

    let response_obj = json!({
        "id": response_id,
        "object": "response",
        "status": "completed",
        "model": raw_model,
        "output": build_common_output_items(&accumulated_text, &final_tool_calls),
        "usage": build_usage_for_responses(&final_usage),
    });
    send_sse_event(
        &tx,
        "response.completed",
        &json!({ "type": "response.completed", "response": response_obj }),
    )
    .await?;
    let _ = tx.send(Ok(axum::body::Bytes::from("data: [DONE]\n\n"))).await;
    Ok((accumulated_text, final_tool_calls, final_usage))
}

/// Close out the currently-open output item (if any) by emitting its
/// `*.done` events followed by `response.output_item.done`, then push any
/// completed `function_call` into `tool_calls` so the audit (via the
/// streaming return tuple) records it. No-op when no item is currently
/// open; safe to call repeatedly.
async fn finalize_output_item(
    tx: &tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
    current_item_id: &mut Option<String>,
    current_item_type: &mut Option<String>,
    current_item_started: &mut bool,
    current_text: &mut String,
    current_tool_call: &mut Option<CommonToolCall>,
    tool_calls: &mut Vec<CommonToolCall>,
    output_index: usize,
) -> Result<(), String> {
    // Edge case: a function_call arrived (with id) but produced no argument
    // delta before the next item started. Still record it so the audit
    // doesn't lose the call.
    if !*current_item_started {
        if current_item_type.as_deref() == Some("function_call") {
            if let Some(tc) = current_tool_call.take() {
                tool_calls.push(tc);
            }
        }
        return Ok(());
    }
    let Some(item_id) = current_item_id.take() else { return Ok(()) };
    let Some(item_type) = current_item_type.take() else { return Ok(()) };

    match item_type.as_str() {
        "message" => {
            let full_text = current_text.clone();
            send_sse_event(
                tx,
                "response.output_text.done",
                &json!({
                    "type": "response.output_text.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "text": full_text,
                }),
            )
            .await?;
            send_sse_event(
                tx,
                "response.content_part.done",
                &json!({
                    "type": "response.content_part.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": full_text },
                }),
            )
            .await?;
            let item = json!({
                "type": "message",
                "id": item_id,
                "role": "assistant",
                "content": [{ "type": "output_text", "text": full_text }],
            });
            send_sse_event(
                tx,
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                }),
            )
            .await?;
            current_text.clear();
        }
        "function_call" => {
            let tc = current_tool_call.take().unwrap_or(CommonToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            let full_args = tc.arguments.clone();
            send_sse_event(
                tx,
                "response.function_call_arguments.done",
                &json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "arguments": full_args,
                }),
            )
            .await?;
            let item = json!({
                "type": "function_call",
                "id": item_id,
                "call_id": tc.id.clone(),
                "name": tc.name.clone(),
                "arguments": full_args,
            });
            send_sse_event(
                tx,
                "response.output_item.done",
                &json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                }),
            )
            .await?;
            tool_calls.push(tc);
        }
        _ => return Ok(()),
    }
    *current_item_started = false;
    Ok(())
}

/// Serialize one Responses SSE event into the wire format
/// `event: <name>\ndata: <json>\n\n` and push it through the mpsc. Returns
/// Err when the client has disconnected (the channel is closed), which the
/// translator propagates so the spawned task can finalize promptly.
async fn send_sse_event(
    tx: &tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
    event: &str,
    data: &Value,
) -> Result<(), String> {
    let payload = serde_json::to_string(data).map_err(|e| format!("encode event: {}", e))?;
    let sse = format!("event: {}\ndata: {}\n\n", event, payload);
    tx.send(Ok(axum::body::Bytes::from(sse)))
        .await
        .map_err(|e| format!("client dropped: {}", e))
}

/// Backoff schedule for retrying a `Transient` failure on the SAME account
/// before giving up on it and swapping.
///
/// Anthropic's 529 `overloaded_error` is a momentary capacity signal, not an
/// account-level verdict: the same account usually serves the same request fine
/// seconds later. Swapping immediately is actively counterproductive, because
/// the new account has none of this conversation's prompt cache — a 1.2M-char
/// transcript that cost ~1k `cache_creation` tokens on the bound account has to
/// be re-cached in full on the new one, turning a cheap request into exactly the
/// oversized kind that upstream sheds first. So: wait, retry the same account,
/// keep the cache.
///
/// The budget is per REQUEST, not per account, so the worst case stays bounded
/// at 150s of waiting regardless of pool size (well inside the 600s
/// `GATEWAY_HTTP_TIMEOUT_SECS` per-attempt ceiling and client timeouts).
const TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS: [u64; 3] = [15, 45, 90];

/// Serve a request via ONE native (account-pooled, raw-payload) provider —
/// `codex` (Responses) or `claude`/`glm` (Anthropic) — with account-swap retry,
/// ported from the Go `proxyRequest` loop. Selects an account, sends, classifies
/// the result, and on a retryable failure applies penalty/cooldown and swaps to
/// the next account up to a per-provider attempt budget. Because the whole
/// response is buffered, a retry is always safe while attempts remain.
///
/// Returns a `ProviderOutcome` so the priority-chain executor can fall through to
/// the next provider when this one's whole pool is unavailable; success is
/// returned immediately.
///
/// KNOWN TRADEOFF — full buffering: streaming clients on this HTTP path (most
/// visibly Claude Code on `/v1/messages`) see their first byte only after the
/// upstream finishes generating, and the whole response sits in memory. In
/// exchange, account-swap retry and cyber_policy hot-swap stay trivially safe
/// (no half-sent stream to splice), and tool-name restoration sees the full
/// body. Codex traffic gets true streaming via the WS relay
/// (`routes::websocket`); a streaming Claude path would need first-event
/// retry-cutoff semantics — revisit if interactive Claude latency matters.
async fn serve_native_provider(
    state: AppState,
    provider: &str,
    // The native providers' upstream response format already matches the client
    // (Responses for codex, Anthropic for claude/glm), so no re-rendering is
    // needed here; kept for call-site uniformity with the adapter servers.
    _client_format: CursorFormat,
    user_id: String,
    payload: Value,
    client_wants_stream: bool,
    shared_only: bool,
) -> ProviderOutcome {
    // Anthropic server tools (`web_search_20250305`, `code_execution_*`, …) only
    // work on first-party Anthropic. Strip them before anything else touches the
    // payload, so the audited request size and every retry reflect what actually
    // goes on the wire. Gated on an explicit provider list rather than "not
    // claude": Codex speaks the Responses format, whose ordinary function tools
    // legitimately carry `type: "function"` and must not be stripped.
    let mut payload = payload;
    if crate::provider::is_third_party_anthropic(provider) {
        let removed = crate::provider::strip_anthropic_server_tools(&mut payload);
        if removed > 0 {
            info!(
                "stripped {} anthropic server-tool item(s) before sending to {}",
                removed, provider
            );
        }
    }

    // Per-user quota gate. `owned_only` = over a token budget but the user has
    // their own accounts: keep serving, but never on borrowed capacity.
    let owned_only = match crate::quota::enforce_user_quota(&state, provider, &user_id, !shared_only).await {
        Ok(v) => v,
        Err(resp) => return ProviderOutcome::NextProvider(Some(resp)),
    };

    let prompt_cache_key = transient_prompt_cache_key(&payload);
    let preferred_account_id = match prompt_cache_key.as_deref() {
        Some(key) => resolve_affinity_account(&state.prompt_cache_bindings, key, &user_id, provider).await,
        None => None,
    };
    // NB: this is the char count of the ENTIRE request JSON (transcript, tools,
    // system, ...), not just the new prompt — it feeds AuditRecord.prompt_length
    // purely as a magnitude indicator. Stats never count it as tokens (see
    // `audit_token_counts`).
    let request_json_chars = payload.to_string().chars().count();

    // Attempt budget = one pass over the pool, plus the same-account backoff
    // retries (which deliberately do NOT consume an account swap).
    let account_budget = provider_attempt_budget(&state, provider).await;
    let max_attempts = account_budget + TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS.len();
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;
    // How many same-account backoff retries this request has already spent.
    let mut transient_retries = 0usize;
    // The account the previous attempt used, so a same-account retry doesn't
    // re-charge selection-spread pressure and push itself out of the pool.
    let mut last_picked: Option<String> = None;
    // Audit data for the most recent failed attempt. Only written when the
    // request FINALLY fails: auditing every retried attempt made one client
    // request show up as N records, inflating the dashboard's error counts by
    // the retry factor. Intermediate failures are traced instead.
    let mut pending_failure_audit: Option<(UpstreamAccount, usize, String)> = None;
    // Pins the next attempt to a specific account, bypassing scoring: set after a
    // cyber_policy hit (to force a cyber account) and by the same-account
    // transient backoff (to force the cache-warm account it just waited on).
    let mut forced_account: Option<String> = None;

    for _ in 0..max_attempts {
        let now = Utc::now();
        let forced = forced_account.take();
        let selected = {
            let accounts = state.accounts.read().await;
            let rate_limits = state.rate_limits.read().await;
            let owner_usage = state.owner_usage.read().await;
            let outlooks = state.capacity_outlooks.read().await;
            // A forced (cyber) account takes precedence when still eligible.
            let forced_pick = forced.as_deref().and_then(|fid| {
                accounts
                    .iter()
                    .find(|a| {
                        a.id == fid
                            && a.provider == provider
                            && account_visible_to_user(a, &user_id)
                            && (!owned_only || a.owner_user_id == user_id)
                            && (!shared_only || a.share_enabled)
                            && !a.runtime.dead
                            && !a.runtime.disabled
                            && !excluded.contains(&a.id)
                    })
                    .cloned()
            });
            if forced_pick.is_some() {
                forced_pick
            } else {
                let mut warm = eligible_accounts(&accounts, provider, &user_id, &excluded, now, false);
                if owned_only {
                    warm.retain(|a| a.owner_user_id == user_id);
                }
                if shared_only {
                    warm.retain(|a| a.share_enabled);
                }
                let mut sel = select_account_for_request_with_preference(
                    &warm,
                    &user_id,
                    provider,
                    &rate_limits,
                    &owner_usage,
                    &outlooks,
                    preferred_account_id.as_deref(),
                );
                if sel.is_none() {
                    // Everything warm is exhausted; fall back to cooling-down accounts.
                    let mut cooling = eligible_accounts(&accounts, provider, &user_id, &excluded, now, true);
                    if owned_only {
                        cooling.retain(|a| a.owner_user_id == user_id);
                    }
                    if shared_only {
                        cooling.retain(|a| a.share_enabled);
                    }
                    let cooling = crate::retry::prefer_near_expiry(cooling, now);
                    sel = select_account_for_request_with_preference(
                        &cooling,
                        &user_id,
                        provider,
                        &rate_limits,
                        &owner_usage,
                        &outlooks,
                        preferred_account_id.as_deref(),
                    );
                }
                sel
            }
        };
        let Some(account) = selected else {
            break;
        };
        selected_any = true;
        excluded.insert(account.id.clone());
        if last_picked.as_deref() != Some(account.id.as_str()) {
            note_account_pick(&state, &account.id).await;
        }
        last_picked = Some(account.id.clone());

        // Claude OAuth (sk-ant-oat) tokens require the full Claude Code fingerprint:
        // system-block injection + metadata + tool-name obfuscation (restored on
        // the buffered response below).
        let mut attempt_payload = payload.clone();
        let mut tool_reverse: HashMap<String, String> = HashMap::new();
        if provider == "claude" && account.access_token.trim().starts_with("sk-ant-oat") {
            crate::fingerprint::claude::inject_request(&mut attempt_payload, &account, &user_id);
            tool_reverse = crate::fingerprint::claude::obfuscate_tool_names(&mut attempt_payload);
        }

        let send_result: Result<(reqwest::Response, UpstreamAccount), String> = match provider {
            "codex" => send_codex_upstream_with_refresh(&state, &account, &attempt_payload).await,
            // GLM rides its Anthropic-compatible endpoint for Claude-format
            // traffic: raw passthrough, no OAuth refresh, no Claude fingerprint.
            "glm" => glm::send_glm_anthropic(&account, &attempt_payload)
                .await
                .map(|resp| (resp, account.clone())),
            // Kimi rides its Anthropic-compatible endpoint for Claude-format
            // traffic, exactly like GLM: raw passthrough, no OAuth refresh, no
            // Claude fingerprint.
            "kimi" => kimi::send_kimi_anthropic(&account, &attempt_payload)
                .await
                .map(|resp| (resp, account.clone())),
            // Trae rides the local trae2anthropic sidecar's `/v1/messages` — the
            // only path it has. Same raw passthrough, and deliberately no Claude
            // fingerprint (the upstream is Trae's agent API, not Anthropic).
            "trae" => trae::send_trae_anthropic(&account, &attempt_payload)
                .await
                .map(|resp| (resp, account.clone())),
            // MiniMax / DeepSeek: same raw Anthropic passthrough, and deliberately
            // no Claude fingerprint — neither is Anthropic, so injecting the Claude
            // Code system blocks / obfuscated tool names would only corrupt the
            // request. `send_*_anthropic` rewrites `model` to a real upstream id.
            "minimax" => minimax::send_minimax_anthropic(&account, &attempt_payload)
                .await
                .map(|resp| (resp, account.clone())),
            "deepseek" => deepseek::send_deepseek_anthropic(&account, &attempt_payload)
                .await
                .map(|resp| (resp, account.clone())),
            _ => send_claude_upstream_with_refresh(&state, &account, &attempt_payload).await,
        };

        let (upstream, account_for_request) = match send_result {
            Ok(v) => v,
            Err(err) => {
                // Network/transport error: penalize lightly and try the next account.
                apply_account_failure(&state, &account.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": provider })));
                continue;
            }
        };

        let upstream_status = upstream.status();
        let content_type = upstream
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        // Capture client-meaningful headers before the body consumes the
        // response: rate-limit feedback for adaptive clients and the upstream
        // request id for support correlation.
        let passthrough_headers = collect_passthrough_headers(upstream.headers());
        let snapshot = parse_rate_limit_headers(upstream.headers());
        let retry_after = parse_retry_after(upstream.headers());
        let cf_mitigated = upstream
            .headers()
            .get("cf-mitigated")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());
        let server_hdr = upstream
            .headers()
            .get("server")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        if let Some(s) = snapshot.clone() {
            crate::capacity::store_rate_limit(&state, &account_for_request.id, s).await;
        }

        let body = match crate::util::read_body_capped(upstream, crate::util::max_response_bytes()).await {
            Ok(v) => v,
            Err(e) => {
                // The response headers already arrived, so the upstream very
                // likely began (or finished) generating before the body read
                // failed — retrying on another account would re-run a
                // non-idempotent, billable generation. Penalize and surface the
                // error instead of swapping.
                apply_account_failure(&state, &account_for_request.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": format!("failed reading upstream body: {}", e), "provider": provider }),
                ));
                break;
            }
        };
        // Restore obfuscated tool names in the buffered response before any
        // parsing / return so the client sees its real tool names.
        let body = if !tool_reverse.is_empty() {
            axum::body::Bytes::from(crate::fingerprint::claude::restore_tool_names(&body, &tool_reverse))
        } else {
            body
        };
        let body_str = String::from_utf8_lossy(&body);

        // cyber_policy hot swap (HTTP/SSE path): if a non-cyber Codex account hit
        // cyber_policy and a cyber_access candidate exists, pin the conversation
        // and retry on it (the buffered analogue of Go's SSE suppress + retry).
        if provider == "codex"
            && !account_for_request.runtime.cyber_access
            && crate::retry::is_cyber_policy_error(&body_str)
        {
            let candidate = {
                let accounts = state.accounts.read().await;
                crate::cyber::cyber_access_candidate(&accounts, "codex", &user_id, &excluded)
            };
            if let Some(cand) = candidate {
                info!(
                    "cyber_policy http/sse on {} -> retrying on cyber account {} (action=retry_buffered)",
                    account_for_request.id, cand.id
                );
                if let Some(key) = prompt_cache_key.as_deref() {
                    remember_affinity_account(
                        &state.prompt_cache_bindings,
                        key.to_string(),
                        &cand.id,
                        "codex",
                        &user_id,
                        PROMPT_CACHE_BINDING_TTL_SECS,
                    )
                    .await;
                }
                forced_account = Some(cand.id.clone());
                continue;
            }
            info!(
                "cyber_policy http/sse on {} but no cyber candidate (action=suppressed_sse)",
                account_for_request.id
            );
        }

        // Refine the raw status classification using the response body/headers.
        let mut class = ErrorClass::from_status(upstream_status.as_u16());
        if class == ErrorClass::Invalid && is_codex_model_unavailable(&body_str) {
            class = ErrorClass::NotFound;
        }
        if class == ErrorClass::Auth
            && is_cloudflare_challenge(&body_str, cf_mitigated.as_deref(), server_hdr.as_deref())
        {
            class = ErrorClass::Transient;
        }

        if class == ErrorClass::None {
            // Success: clear backoff, sync any 100% Claude cooldown, remember
            // affinity, audit, and return the buffered response verbatim.
            reset_backoff(&state, &account_for_request.id).await;
            if let Some(s) = snapshot.as_ref() {
                sync_usage_cooldown(&state, &account_for_request.id, s).await;
            }
            if let Some(key) = prompt_cache_key.as_deref() {
                remember_affinity_account(
                    &state.prompt_cache_bindings,
                    key.to_string(),
                    &account_for_request.id,
                    provider,
                    &user_id,
                    PROMPT_CACHE_BINDING_TTL_SECS,
                )
                .await;
            }
            let tokens = crate::usage::tokens::parse_usage(provider, &body_str);
            // Audit the model that ANSWERED, not the one the client asked for.
            // The two differ constantly on this path: the chain degrades a
            // `claude-*` request onto DeepSeek/MiniMax/Trae (whose senders
            // rewrite `model` on their own copy of the payload, so `payload`
            // here still says `claude-*`), and even first-party Anthropic
            // resolves a floating alias to the dated snapshot that ran. The
            // response body is the only place that truth appears; the derived
            // mapping is the fallback for bodies that don't echo it.
            let effective_model = crate::usage::tokens::parse_response_model(&body_str)
                .unwrap_or_else(|| effective_model_fallback(&payload, provider));
            write_proxy_audit(
                &state,
                &user_id,
                &account_for_request,
                provider,
                &effective_model,
                request_json_chars,
                body.len(),
                "success",
                tokens,
            )
            .await;

            // Non-streaming client + SSE upstream (Codex is always called with
            // stream=true): aggregate the stream into the terminal `response`
            // object so the client gets plain JSON instead of an SSE body.
            //
            // Detect SSE by BODY shape, not just the upstream content-type: some
            // upstream responses stream an `event:`/`data:` body without a
            // `text/event-stream` content-type (it defaults to application/json
            // above), which would otherwise skip aggregation and hand the client
            // a raw SSE body mislabeled as JSON.
            let body_looks_sse = {
                let head = body_str.trim_start();
                head.starts_with("event:") || head.starts_with("data:") || body_str.contains("\ndata:")
            };
            let is_sse = content_type.to_ascii_lowercase().contains("text/event-stream")
                || body_looks_sse;
            if !client_wants_stream && is_sse {
                if let Some(aggregated) = aggregate_codex_sse_to_response_json(&body_str) {
                    let mut response = (upstream_status, Json(aggregated)).into_response();
                    apply_passthrough_headers(&mut response, &passthrough_headers);
                    return ProviderOutcome::Served(response);
                }
                // No terminal `response.*` event (truncated/odd stream): synthesize
                // a minimal Responses-shaped JSON from whatever output text we can
                // recover, so a non-streaming client never receives a raw
                // text/event-stream body it didn't ask for.
                let text = extract_output_text_from_sse(&body_str).unwrap_or_default();
                let mut response = (
                    upstream_status,
                    Json(json!({
                        "output_text": text,
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}]
                        }]
                    })),
                )
                    .into_response();
                apply_passthrough_headers(&mut response, &passthrough_headers);
                return ProviderOutcome::Served(response);
            }
            let mut response = Response::new(body.into());
            *response.status_mut() = upstream_status;
            if let Ok(v) = HeaderValue::from_str(&content_type) {
                response.headers_mut().insert(CONTENT_TYPE, v);
            }
            apply_passthrough_headers(&mut response, &passthrough_headers);
            return ProviderOutcome::Served(response);
        }

        // Failure path. Detect permanently-fatal account states first.
        let org_disabled = provider == "claude" && is_claude_organization_disabled(&body_str);
        let deactivated = class == ErrorClass::Payment && is_deactivated_workspace(&body_str);
        let dead = org_disabled || deactivated;
        apply_account_failure(&state, &account_for_request.id, class, snapshot.as_ref(), retry_after, dead).await;

        // Remember what we'd audit/return if this turns out to be the request's
        // final outcome; an intermediate retried failure is only traced.
        pending_failure_audit = Some((
            account_for_request.clone(),
            body.len(),
            format!("upstream_error_{}", upstream_status.as_u16()),
        ));
        last_error = Some(build_error_payload(upstream_status, provider, &account_for_request.account_label, &body));

        // Transient (529 overloaded, 5xx, Cloudflare challenge): wait it out on
        // the SAME account first — see TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS for
        // why swapping straight away makes the next attempt more likely to fail.
        // 429 is excluded on purpose: RateLimit has its own reset-aware cooldown
        // and a swap there is the correct move.
        if class == ErrorClass::Transient && transient_retries < TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS.len() {
            let wait_secs = TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS[transient_retries];
            transient_retries += 1;
            info!(
                "upstream_error_{} on {} ({} attempt failed, backing off {}s and retrying the SAME account — retry {}/{}, keeps prompt cache)",
                upstream_status.as_u16(),
                account_for_request.account_label,
                provider,
                wait_secs,
                transient_retries,
                TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS.len(),
            );
            // Re-admit the account for selection and pin the next attempt to it,
            // so the retry can't be scored onto a cache-cold peer.
            excluded.remove(&account_for_request.id);
            forced_account = Some(account_for_request.id.clone());
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            continue;
        }

        info!(
            "upstream_error_{} on {} ({} attempt failed{})",
            upstream_status.as_u16(),
            account_for_request.account_label,
            provider,
            if class.is_retryable() { ", retrying on next account" } else { "" },
        );

        if class.is_retryable() {
            continue;
        }
        // Non-retryable (Invalid/Fatal): return immediately.
        break;
    }

    // This provider's whole pool couldn't serve the request. The priority-chain
    // executor (`serve_with_chain`) decides what happens next — it degrades to
    // the next provider in the chain (e.g. GLM or a local ollama), or surfaces
    // the error below if this was the last provider. Per-account failures were
    // already penalized in-memory during the loop.
    //
    // Write the single failure audit record (one client request, one record).
    if last_error.is_some() {
        if let Some((account, output_len, status_label)) = pending_failure_audit {
            // No usable response body to read the served model out of, so the
            // derived mapping is all there is — which is still the right answer:
            // it names the model this provider WOULD have run.
            write_proxy_audit(
                &state,
                &user_id,
                &account,
                provider,
                &effective_model_fallback(&payload, provider),
                request_json_chars,
                output_len,
                &status_label,
                TokenUsage::default(),
            )
            .await;
        }
    }

    match last_error {
        Some((status, body)) => {
            ProviderOutcome::NextProvider(Some((status, Json(body)).into_response()))
        }
        None if !selected_any => ProviderOutcome::NextProvider(None),
        None => ProviderOutcome::NextProvider(Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("all {} accounts exhausted", provider), "provider": provider })),
            )
                .into_response(),
        )),
    }
}

/// Map an upstream failure into the client-facing response. A 401 is rewritten
/// to 400 with a friendly message so the local client never triggers its own
/// auth-recovery flow against the user's real token.
fn build_error_payload(
    status: StatusCode,
    provider: &str,
    account_label: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    if status == StatusCode::UNAUTHORIZED {
        let detail = if provider == "codex" {
            format!(
                "共享池中的账号 ({}) Token 已过期或无效，请联系共享者重新导入 auth.json。",
                account_label
            )
        } else if provider == "trae" {
            // A 401 here is the local sidecar rejecting the gateway's key, NOT a
            // Trae login problem — the Trae accounts themselves live inside the
            // sidecar's admin panel and never reach this gateway.
            format!(
                "Trae sidecar ({}) 拒绝了本网关的 API Key，请在 trae2anthropic 管理面板确认 Key 后重新连接该账号。",
                account_label
            )
        } else if matches!(provider, "glm" | "kimi" | "minimax" | "deepseek") {
            // Key-auth metered providers: a 401 is a bad/expired API key, not an
            // OAuth token — so the credentials.json advice doesn't apply.
            let display = match provider {
                "minimax" => "MiniMax".to_string(),
                "deepseek" => "DeepSeek".to_string(),
                other => other.to_uppercase(),
            };
            format!(
                "共享池中的 {} 账号 ({}) API Key 无效或已过期，请重新连接该账号。",
                display, account_label
            )
        } else {
            format!(
                "共享池中的 Claude 账号 ({}) Token 已过期或无效，请联系共享者重新导入 credentials.json。",
                account_label
            )
        };
        return (StatusCode::BAD_REQUEST, json!({ "detail": detail, "error": detail }));
    }
    // Never forward the raw upstream error body to the client: for a shared pool
    // it can carry the serving account's identifiers, org/workspace names, or
    // rate-limit state. Log the full detail server-side and return only the
    // generic error type/code so clients can still branch on the category.
    warn!(
        "upstream {} error {}: {}",
        provider,
        status.as_u16(),
        crate::util::truncate_text(&String::from_utf8_lossy(body), 1000)
    );
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let err_obj = parsed.get("error");
    let etype = err_obj
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("upstream_error");
    let ecode = err_obj
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .or_else(|| parsed.get("code").and_then(|v| v.as_str()));
    let mut error = json!({
        "type": etype,
        "message": format!("upstream {} returned status {}", provider, status.as_u16()),
    });
    if let Some(code) = ecode {
        error["code"] = json!(code);
    }
    (status, json!({ "error": error }))
}

/// Upstream response headers worth forwarding to the client: rate-limit
/// feedback (adaptive clients throttle on these) and the upstream request id
/// (support correlation). Everything else stays gateway-internal.
fn collect_passthrough_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n.starts_with("anthropic-ratelimit-")
                || n.starts_with("x-ratelimit-")
                || n == "retry-after"
                || n == "request-id"
                || n == "x-request-id"
        })
        .filter_map(|(name, value)| {
            HeaderValue::from_bytes(value.as_bytes())
                .ok()
                .map(|v| (name.as_str().to_string(), v))
        })
        .collect()
}

fn apply_passthrough_headers(response: &mut Response, headers: &[(String, HeaderValue)]) {
    for (name, value) in headers {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(name.as_bytes()) {
            response.headers_mut().insert(name, value.clone());
        }
    }
}

/// Derive the model id the request will run on for `provider`, given the
/// payload the client sent. Used as the fallback when no response body is
/// available (the response is what really knows — see
/// `usage::tokens::parse_response_model`). The rewrite happens here, on the
/// audit copy, NOT on the wire copy — every `send_*` helper either forwards
/// the client's model verbatim (claude/codex/GLM/Kimi Anthropic) or mutates
/// its own clone (Trae/MiniMax/DeepSeek), so the caller's `payload` is
/// always the client's raw request.
fn effective_model_fallback(payload: &Value, provider: &str) -> String {
    let raw = payload.get("model").and_then(|v| v.as_str()).unwrap_or("");
    crate::provider::normalize_model_for_provider(raw, provider)
}

// The params map one-to-one onto AuditRecord fields; a builder would only
// restate them.
#[allow(clippy::too_many_arguments)]
async fn write_proxy_audit(
    state: &AppState,
    user_id: &str,
    account: &UpstreamAccount,
    provider: &str,
    model: &str,
    prompt_length: usize,
    output_length: usize,
    status_label: &str,
    tokens: TokenUsage,
) {
    let audit = AuditRecord {
        request_id: Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        model: model.to_string(),
        routed_provider: provider.to_string(),
        upstream_account_id: account.id.clone(),
        upstream_owner_user_id: account.owner_user_id.clone(),
        prompt_length,
        output_length,
        status: status_label.to_string(),
        created_at: Utc::now(),
        tokens,
        // Origin is set once per request by the entry handler via
        // `auth::with_request_origin`; deep callsites just read it from the
        // task local. Empty when written outside a request scope (e.g. a
        // background probe) — `routes::stats` then buckets the row as
        // "unknown" so it still shows up in the totals.
        origin: crate::auth::current_origin().unwrap_or_default(),
    };
    if let Err(e) = append_audit(state, &audit).await {
        error!("failed writing proxy audit record: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_error_message_nested() {
        let body = r#"{"error":{"message":"insufficient balance","type":"upstream_error"}}"#;
        assert_eq!(
            parse_openai_error_message(body).as_deref(),
            Some("insufficient balance")
        );
    }

    #[test]
    fn parse_openai_error_message_flat_string() {
        // Some non-success bodies collapse the error to a flat string.
        let body = r#"{"error":"invalid model id"}"#;
        assert_eq!(
            parse_openai_error_message(body).as_deref(),
            Some("invalid model id")
        );
    }

    #[test]
    fn parse_openai_error_message_returns_none_for_non_json() {
        assert!(parse_openai_error_message("<html>500</html>").is_none());
        assert!(parse_openai_error_message("{}").is_none());
        assert!(parse_openai_error_message("").is_none());
    }

    #[test]
    fn build_usage_for_responses_sums_total_tokens() {
        let usage = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 20,
            output_tokens: 50,
            reasoning_tokens: 5,
            ..Default::default()
        };
        let v = build_usage_for_responses(&usage);
        assert_eq!(v["input_tokens"], 100);
        assert_eq!(v["input_tokens_details"]["cached_tokens"], 20);
        assert_eq!(v["output_tokens"], 50);
        assert_eq!(v["output_tokens_details"]["reasoning_tokens"], 5);
        assert_eq!(v["total_tokens"], 150);
    }

    #[test]
    fn build_common_output_items_text_only() {
        let items = build_common_output_items("hello world", &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn build_common_output_items_tool_calls_only() {
        let tcs = vec![CommonToolCall {
            id: "call_abc".to_string(),
            name: "get_weather".to_string(),
            arguments: r#"{"city":"SF"}"#.to_string(),
        }];
        let items = build_common_output_items("", &tcs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_abc");
        assert_eq!(items[0]["name"], "get_weather");
        assert_eq!(items[0]["arguments"], r#"{"city":"SF"}"#);
    }

    #[test]
    fn build_common_output_items_text_then_tool_call() {
        let tcs = vec![CommonToolCall {
            id: "call_x".to_string(),
            name: "lookup".to_string(),
            arguments: "{}".to_string(),
        }];
        let items = build_common_output_items("here you go", &tcs);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn build_common_output_items_empty_synthesizes_placeholder() {
        // Streaming may finish with neither text nor tool calls (e.g. a pure
        // refusal). The array must still be non-empty so Codex clients that
        // assert `output.length > 0` don't choke.
        let items = build_common_output_items("", &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "");
    }
}
