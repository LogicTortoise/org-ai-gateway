use crate::prelude::*;
use crate::pool::WS_SESSION_BINDING_TTL_SECS;
use crate::auth::identify_caller;
use crate::pool::remember_affinity_account;
use crate::pool::resolve_affinity_account;
use crate::pool::select_healthy_account;
use crate::pool::websocket_session_key;
use crate::provider::codex::codex_ws_upstream_url;
use std::collections::HashSet;

/// Concrete upstream WebSocket stream type (rustls over TCP).
type CodexUpstreamWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) async fn proxy_codex_ws(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;
    // Quota gate at connect time. The WS relay doesn't parse token usage, so
    // an established session doesn't add to the budgets — but a user already
    // over budget (via the HTTP paths) is kept off borrowed accounts here too.
    let owned_only = match crate::quota::enforce_user_quota(&state, "codex", &user_id, caller.owner_trusted).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let session_key = websocket_session_key(&headers, &query);
    let preferred_account_id = match session_key.as_deref() {
        Some(key) => {
            resolve_affinity_account(&state.ws_session_bindings, key, &user_id, "codex").await
        }
        None => None,
    };
    let account = select_healthy_account(
        &state,
        "codex",
        &user_id,
        preferred_account_id.as_deref(),
        owned_only,
        shared_only,
    )
    .await;
    let account = match account {
        Some(a) => a,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"no codex account available for websocket session"})),
            )
                .into_response();
        }
    };
    if let Some(key) = session_key.as_deref() {
        remember_affinity_account(
            &state.ws_session_bindings,
            key.to_string(),
            &account.id,
            "codex",
            &user_id,
            WS_SESSION_BINDING_TTL_SECS,
        )
        .await;
    }
    let upstream_url = codex_ws_upstream_url();
    ws.on_upgrade(move |client_ws| async move {
        if let Err(e) =
            proxy_codex_ws_tunnel(state, client_ws, account, upstream_url, session_key, user_id).await
        {
            warn!("codex websocket tunnel ended with error: {}", e);
        }
    })
}

/// Connect to the Codex realtime WebSocket upstream with the given account's
/// credentials + Codex Desktop identity headers.
async fn connect_codex_ws(account: &UpstreamAccount, url: &str) -> Result<CodexUpstreamWs, String> {
    let bearer = account.bearer();
    if bearer.is_empty() {
        return Err("connected codex account has empty access token".to_string());
    }
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("invalid upstream websocket url: {}", e))?;
    let bearer_value = HeaderValue::from_str(&format!("Bearer {}", bearer))
        .map_err(|e| format!("invalid bearer header for websocket: {}", e))?;
    req.headers_mut().insert(AUTHORIZATION, bearer_value);
    // Identity comes from the fingerprint module (same UA the HTTP path sends),
    // so a version bump there automatically covers the WS handshake too.
    let user_agent = HeaderValue::from_str(&crate::fingerprint::codex::codex_user_agent())
        .map_err(|e| format!("invalid websocket User-Agent: {}", e))?;
    req.headers_mut().insert("User-Agent", user_agent);
    let originator = HeaderValue::from_str(&crate::fingerprint::codex::codex_originator())
        .map_err(|e| format!("invalid websocket originator: {}", e))?;
    req.headers_mut().insert("originator", originator);
    if !account.account_id.trim().is_empty() {
        let account_id = HeaderValue::from_str(account.account_id.trim())
            .map_err(|e| format!("invalid ChatGPT-Account-ID header: {}", e))?;
        req.headers_mut().insert("ChatGPT-Account-ID", account_id);
    }
    let (ws, _) = connect_async(req)
        .await
        .map_err(|e| format!("failed connecting codex websocket upstream: {}", e))?;
    Ok(ws)
}

/// Bidirectional Codex realtime relay with cyber_policy hot-swap. The client
/// reader stays alive across swaps; only the upstream half is reconnected.
pub(crate) async fn proxy_codex_ws_tunnel(
    state: AppState,
    client_ws: WebSocket,
    account: UpstreamAccount,
    upstream_url: String,
    session_key: Option<String>,
    user_id: String,
) -> Result<(), String> {
    let upstream_ws = connect_codex_ws(&account, &upstream_url).await?;

    let (mut client_sender, mut client_receiver) = client_ws.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream_ws.split();

    // Once on a cyber account (or after one swap attempt), stop swapping.
    let mut swap_done = account.runtime.cyber_access;
    let mut active_account_id = account.id.clone();
    let mut excluded: HashSet<String> = HashSet::new();
    excluded.insert(account.id.clone());
    let mut last_response_create: Option<String> = None;

    // Per-turn token accounting. The Codex realtime stream carries usage in
    // `token_count` and terminal `response.*` frames; we accumulate the latest
    // usage seen and flush one audit record per turn (on the next turn's
    // `response.create`, and once more at session end). Without this, WS traffic
    // was invisible to the dashboard and quota ledgers — the HTTP path's
    // `parse_usage` never ran here.
    let mut turn_usage = TokenUsage::default();
    let mut turn_model = String::new();

    loop {
        tokio::select! {
            client_msg = client_receiver.next() => {
                let Some(msg) = client_msg else { break; };
                let msg = msg.map_err(|e| format!("failed reading client websocket: {}", e))?;
                // Privacy guard (same rule as the HTTP path): never let a turn be
                // stored server-side under the shared pool account.
                let msg = match msg {
                    ClientWsMessage::Text(t) => {
                        let text = crate::cyber::enforce_response_create_privacy(&t)
                            .unwrap_or_else(|| t.to_string());
                        if crate::cyber::is_codex_response_create(&text) {
                            // A new turn begins: flush the turn that just ended
                            // (its usage is complete) before the prompt context
                            // is overwritten.
                            if turn_usage.input_tokens > 0 || turn_usage.output_tokens > 0 {
                                write_ws_audit(
                                    &state,
                                    &user_id,
                                    &active_account_id,
                                    &turn_model,
                                    last_response_create.as_deref(),
                                    std::mem::take(&mut turn_usage),
                                )
                                .await;
                                turn_model.clear();
                            }
                            last_response_create = Some(text.clone());
                        }
                        ClientWsMessage::Text(text)
                    }
                    other => other,
                };
                if matches!(msg, ClientWsMessage::Close(_)) {
                    // Forward the real close frame, then flush a graceful close
                    // so the upstream TLS/TCP socket isn't left half-open (FD
                    // leak under session churn).
                    if let Some(outgoing) = client_ws_to_upstream(msg) {
                        let _ = upstream_sender.send(outgoing).await;
                    }
                    let _ = upstream_sender.close().await;
                    break;
                }
                if let Some(outgoing) = client_ws_to_upstream(msg) {
                    upstream_sender
                        .send(outgoing)
                        .await
                        .map_err(|e| format!("failed writing upstream websocket: {}", e))?;
                }
            }
            up_msg = upstream_receiver.next() => {
                let Some(msg) = up_msg else {
                    let _ = client_sender.send(ClientWsMessage::Close(None)).await;
                    let _ = client_sender.close().await;
                    break;
                };
                let msg = msg.map_err(|e| format!("failed reading upstream websocket: {}", e))?;

                // Intercept cyber_policy frames for a transparent hot-swap.
                if let UpstreamWsMessage::Text(t) = &msg {
                    if !swap_done && crate::retry::is_cyber_policy_error(t) {
                        info!("cyber_policy ws frame on account {} (action=suppressed_ws)", active_account_id);
                        swap_done = true;
                        let candidate = {
                            let accounts = state.accounts.read().await;
                            crate::cyber::cyber_access_candidate(&accounts, "codex", &user_id, &excluded)
                        };
                        match (candidate, last_response_create.clone()) {
                            (Some(cand), Some(replay_src)) => {
                                // Bound the reconnect: it runs inline in the
                                // select loop, so a hanging dial would stall the
                                // whole tunnel (client frames can't be pumped).
                                let dial = tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    connect_codex_ws(&cand, &upstream_url),
                                )
                                .await;
                                match dial.unwrap_or_else(|_| {
                                    Err("cyber swap dial timed out".to_string())
                                }) {
                                    Ok(new_ws) => {
                                        let replay = crate::cyber::strip_previous_response_id(&replay_src);
                                        let (new_sender, new_receiver) = new_ws.split();
                                        upstream_sender = new_sender;
                                        upstream_receiver = new_receiver;
                                        if let Err(e) = upstream_sender
                                            .send(UpstreamWsMessage::Text(replay))
                                            .await
                                        {
                                            return Err(format!("replay to swapped upstream failed: {}", e));
                                        }
                                        info!(
                                            "silently swapped codex ws upstream to cyber account {} (was {}) (action=swap_succeeded)",
                                            cand.id, active_account_id
                                        );
                                        excluded.insert(cand.id.clone());
                                        active_account_id = cand.id.clone();
                                        if let Some(key) = session_key.as_deref() {
                                            remember_affinity_account(
                                                &state.ws_session_bindings,
                                                key.to_string(),
                                                &cand.id,
                                                "codex",
                                                &user_id,
                                                WS_SESSION_BINDING_TTL_SECS,
                                            )
                                            .await;
                                        }
                                        // Swallow the cyber_policy frame; continue relaying.
                                        continue;
                                    }
                                    Err(e) => {
                                        warn!("cyber swap dial failed: {}; forwarding cyber_policy frame", e);
                                    }
                                }
                            }
                            _ => {
                                info!("cyber_policy ws frame but no swap candidate (action=swap_no_candidate)");
                            }
                        }
                        // Fall through: forward the original frame to the client.
                    }
                }

                // Accumulate token usage from this frame before forwarding it.
                if let UpstreamWsMessage::Text(t) = &msg {
                    if let Ok(ev) = serde_json::from_str::<Value>(t) {
                        if let Some(usage) = crate::usage::tokens::parse_codex_event_usage(&ev) {
                            // A terminal frame occasionally omits the cached count
                            // an earlier `token_count` already reported — keep it.
                            let prev_cached = turn_usage.cached_input_tokens;
                            let mut next = usage;
                            if next.cached_input_tokens == 0 && prev_cached > 0 {
                                next.cached_input_tokens = prev_cached;
                                next.billable_tokens =
                                    (next.input_tokens - prev_cached + next.output_tokens).max(0);
                            }
                            turn_usage = next;
                        }
                        if let Some(m) = ev.pointer("/response/model").and_then(|v| v.as_str()) {
                            turn_model = m.to_string();
                        }
                    }
                }

                match upstream_ws_to_client(msg) {
                    Some(close @ ClientWsMessage::Close(_)) => {
                        // Forward the upstream's close (code + reason preserved),
                        // then flush a graceful close to the client.
                        let _ = client_sender.send(close).await;
                        let _ = client_sender.close().await;
                        break;
                    }
                    Some(outgoing) => {
                        client_sender
                            .send(outgoing)
                            .await
                            .map_err(|e| format!("failed writing client websocket: {}", e))?;
                    }
                    None => {}
                }
            }
        }
    }

    // Flush the final turn: its terminal frame arrived but no further
    // `response.create` followed to trigger the in-loop flush.
    if turn_usage.input_tokens > 0 || turn_usage.output_tokens > 0 {
        write_ws_audit(
            &state,
            &user_id,
            &active_account_id,
            &turn_model,
            last_response_create.as_deref(),
            turn_usage,
        )
        .await;
    }
    Ok(())
}

/// Write one audit record for a completed Codex WebSocket turn. Resolves the
/// active account's owner for the owner/borrower split, derives the model and a
/// prompt-length proxy from the client's `response.create` frame when the
/// upstream event didn't name the model, and routes through `append_audit` so
/// the live quota ledgers (`note_request_usage`) account for WS consumption too.
async fn write_ws_audit(
    state: &AppState,
    user_id: &str,
    account_id: &str,
    model_from_event: &str,
    response_create: Option<&str>,
    tokens: TokenUsage,
) {
    let owner_user_id = {
        let accounts = state.accounts.read().await;
        accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.owner_user_id.clone())
            .unwrap_or_default()
    };
    let model = if !model_from_event.is_empty() {
        model_from_event.to_string()
    } else {
        response_create
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .as_ref()
            .and_then(|v| {
                v.pointer("/response/model")
                    .or_else(|| v.get("model"))
                    .and_then(|m| m.as_str())
                    .map(|m| m.to_string())
            })
            .unwrap_or_default()
    };
    let record = AuditRecord {
        request_id: Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        model,
        routed_provider: "codex".to_string(),
        upstream_account_id: account_id.to_string(),
        upstream_owner_user_id: owner_user_id,
        prompt_length: response_create.map(|s| s.chars().count()).unwrap_or(0),
        output_length: 0,
        status: "success".to_string(),
        created_at: Utc::now(),
        tokens,
    };
    if let Err(e) = crate::pool::storage::append_audit(state, &record).await {
        error!("failed writing codex ws audit record: {}", e);
    }
}

pub(crate) fn client_ws_to_upstream(msg: ClientWsMessage) -> Option<UpstreamWsMessage> {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    match msg {
        ClientWsMessage::Text(v) => Some(UpstreamWsMessage::Text(v.to_string())),
        ClientWsMessage::Binary(v) => Some(UpstreamWsMessage::Binary(v.to_vec())),
        ClientWsMessage::Ping(v) => Some(UpstreamWsMessage::Ping(v.to_vec())),
        ClientWsMessage::Pong(v) => Some(UpstreamWsMessage::Pong(v.to_vec())),
        // Preserve the close code + reason instead of flattening to None, so the
        // upstream sees the client's intended shutdown reason.
        ClientWsMessage::Close(frame) => Some(UpstreamWsMessage::Close(frame.map(|f| CloseFrame {
            code: CloseCode::from(f.code),
            reason: f.reason.into_owned().into(),
        }))),
    }
}

pub(crate) fn upstream_ws_to_client(msg: UpstreamWsMessage) -> Option<ClientWsMessage> {
    use axum::extract::ws::CloseFrame;
    match msg {
        UpstreamWsMessage::Text(v) => Some(ClientWsMessage::Text(v)),
        UpstreamWsMessage::Binary(v) => Some(ClientWsMessage::Binary(v)),
        UpstreamWsMessage::Ping(v) => Some(ClientWsMessage::Ping(v)),
        UpstreamWsMessage::Pong(v) => Some(ClientWsMessage::Pong(v)),
        // Preserve the upstream's close code + reason for the client.
        UpstreamWsMessage::Close(frame) => Some(ClientWsMessage::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.to_string().into(),
        }))),
        _ => None,
    }
}
