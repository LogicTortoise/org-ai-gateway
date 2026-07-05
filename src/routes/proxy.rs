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
use crate::provider::glm;
use crate::provider::ollama::ollama_canonical_model;
use crate::provider::ollama::ollama_http_client;
use crate::provider::ollama::send_ollama_upstream;
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
                // format) → GLM's OpenAI-compatible endpoint via the adapter.
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
                    serve_glm(state, client_format, user_id, payload, client_wants_stream, shared_only).await
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
    Json(mut payload): Json<Value>,
) -> impl IntoResponse {
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
    Json(mut payload): Json<Value>,
) -> impl IntoResponse {
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
            state, user_id, &account, "cursor", payload, prompt_chars, output_chars,
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
                state, user_id, &account, "cursor", payload, prompt_chars, 0,
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
            state, user_id, &account, "ollama", payload, request_json_chars, result.text.len(),
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

/// Serve a request via GLM's OpenAI-compatible `/chat/completions` through the
/// shared format adapter. This is the path for Codex/OpenAI-format traffic (the
/// Claude-format path is the native Anthropic passthrough in
/// `serve_native_provider`). Token usage is REAL (parsed from GLM's `usage`),
/// so audited consumption is exact. Account-swap retry mirrors `serve_ollama`,
/// but GLM IS metered, so the per-user token-budget quota gate applies.
async fn serve_glm(
    state: &AppState,
    format: CursorFormat,
    user_id: &str,
    payload: &Value,
    client_wants_stream: bool,
    shared_only: bool,
) -> ProviderOutcome {
    use crate::provider::cursor::{build_buffered_body, build_sse_body, estimate_request_tokens, extract_request};

    let owned_only = match crate::quota::enforce_user_quota(state, "glm", user_id, !shared_only).await {
        Ok(v) => v,
        Err(resp) => return ProviderOutcome::NextProvider(Some(resp)),
    };

    let req = match extract_request(payload) {
        Ok(v) => v,
        Err(e) => {
            return ProviderOutcome::NextProvider(Some(
                (StatusCode::BAD_REQUEST, Json(json!({ "error": e, "provider": "glm" }))).into_response(),
            ));
        }
    };

    let raw_model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("glm");
    // Direct `glm/<name>` / `glm-*` → that model; a paid model name routed here
    // by the chain → the configured GLM default model.
    let upstream_model = if glm::is_glm_model(raw_model) {
        glm::glm_canonical_model(raw_model)
    } else {
        glm::glm_canonical_model("glm")
    };
    let request_id = Uuid::new_v4().to_string();
    let request_json_chars = payload.to_string().chars().count();
    let estimated_input = estimate_request_tokens(&req);
    let client = glm::glm_http_client();

    let max_attempts = provider_attempt_budget(state, "glm").await;
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;

    for _ in 0..max_attempts {
        let now = Utc::now();
        let selected = {
            let accounts = state.accounts.read().await;
            let rate_limits = state.rate_limits.read().await;
            let owner_usage = state.owner_usage.read().await;
            let mut warm = eligible_accounts(&accounts, "glm", user_id, &excluded, now, true);
            if owned_only {
                warm.retain(|a| a.owner_user_id == user_id);
            }
            if shared_only {
                warm.retain(|a| a.share_enabled);
            }
            // Only accounts that expose the OpenAI-compatible endpoint can serve
            // this adapter path.
            warm.retain(glm::supports_openai);
            select_account_for_request(&warm, user_id, "glm", &rate_limits, &owner_usage)
        };
        let Some(account) = selected else { break };
        selected_any = true;
        excluded.insert(account.id.clone());
        note_account_pick(state, &account.id).await;

        let result = match glm::send_glm_openai(client, &account, &upstream_model, &req).await {
            Ok(r) => r,
            Err(err) => {
                apply_account_failure(state, &account.id, ErrorClass::Transient, None, None, false).await;
                last_error = Some((StatusCode::BAD_GATEWAY, json!({ "error": err, "provider": "glm" })));
                continue;
            }
        };

        if !result.status.is_success() || (result.text.is_empty() && result.error.is_some()) {
            let detail = result
                .error
                .clone()
                .unwrap_or_else(|| format!("glm upstream returned {}", result.status));
            let class = ErrorClass::from_status(result.status.as_u16());
            apply_account_failure(state, &account.id, class, None, None, false).await;
            info!(
                "glm_error_{} on {} ({})",
                result.status.as_u16(),
                account.account_label,
                if class.is_retryable() { "retrying on next account" } else { "final" },
            );
            let status = if result.status.is_success() { StatusCode::BAD_GATEWAY } else { result.status };
            last_error = Some((status, json!({ "error": detail, "provider": "glm" })));
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
            state, user_id, &account, "glm", payload, request_json_chars, result.text.len(),
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
                Json(json!({ "error": "all glm accounts exhausted", "provider": "glm" })),
            )
                .into_response(),
        )),
    }
}

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

    let max_attempts = provider_attempt_budget(&state, provider).await;
    let mut excluded: HashSet<String> = HashSet::new();
    let mut selected_any = false;
    let mut last_error: Option<(StatusCode, Value)> = None;
    // Audit data for the most recent failed attempt. Only written when the
    // request FINALLY fails: auditing every retried attempt made one client
    // request show up as N records, inflating the dashboard's error counts by
    // the retry factor. Intermediate failures are traced instead.
    let mut pending_failure_audit: Option<(UpstreamAccount, usize, String)> = None;
    // Set after a cyber_policy hit to force the next attempt onto a cyber account.
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
        note_account_pick(&state, &account.id).await;

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
            write_proxy_audit(
                &state,
                &user_id,
                &account_for_request,
                provider,
                &payload,
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
        info!(
            "upstream_error_{} on {} ({} attempt failed{})",
            upstream_status.as_u16(),
            account_for_request.account_label,
            provider,
            if class.is_retryable() { ", retrying on next account" } else { "" },
        );
        pending_failure_audit = Some((
            account_for_request.clone(),
            body.len(),
            format!("upstream_error_{}", upstream_status.as_u16()),
        ));
        last_error = Some(build_error_payload(upstream_status, provider, &account_for_request.account_label, &body));

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
            write_proxy_audit(
                &state,
                &user_id,
                &account,
                provider,
                &payload,
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

// The params map one-to-one onto AuditRecord fields; a builder would only
// restate them.
#[allow(clippy::too_many_arguments)]
async fn write_proxy_audit(
    state: &AppState,
    user_id: &str,
    account: &UpstreamAccount,
    provider: &str,
    payload: &Value,
    prompt_length: usize,
    output_length: usize,
    status_label: &str,
    tokens: TokenUsage,
) {
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let audit = AuditRecord {
        request_id: Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        model,
        routed_provider: provider.to_string(),
        upstream_account_id: account.id.clone(),
        upstream_owner_user_id: account.owner_user_id.clone(),
        prompt_length,
        output_length,
        status: status_label.to_string(),
        created_at: Utc::now(),
        tokens,
    };
    if let Err(e) = append_audit(state, &audit).await {
        error!("failed writing proxy audit record: {}", e);
    }
}
