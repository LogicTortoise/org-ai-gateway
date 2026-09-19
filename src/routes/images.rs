//! OpenAI-compatible image-generation proxy.
//!
//! Codex's built-in image extension sends `POST /v1/images/generations` to
//! the active model-provider base URL. MiniMax exposes equivalent capability
//! through a different endpoint and schema, so this route translates MiniMax
//! attempts and falls back to the native ChatGPT Codex image bridge.
//!
//! `/v1/images/edits` is a byte-for-byte passthrough to Codex's matching
//! image-edit endpoint. OpenAI supports both JSON image references and
//! multipart file uploads; keeping either original body intact also keeps
//! future fields working without gateway changes. There is no
//! MiniMax fallback: MiniMax documents only a single-character reference for
//! `image-01`, not OpenAI-compatible generic edits or mask/inpainting.

use crate::auth::identify_caller;
use crate::pool::account_visible_to_user;
use crate::pool::note_account_pick;
use crate::pool::select_account_for_request;
use crate::pool::storage::append_audit;
use crate::prelude::*;
use crate::provider::chains::{ordered_attempts, ChainMode, ChainSlot};
use crate::provider::{codex, minimax};
use crate::retry::{
    apply_account_failure, eligible_accounts, parse_retry_after, prefer_near_expiry,
    provider_attempt_budget, reset_backoff, ErrorClass,
};
use std::collections::HashSet;

pub(crate) async fn proxy_image_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let prompt = match image_prompt(&payload) {
        Ok(prompt) => prompt,
        Err(message) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    };
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;
    let requested_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-image-2")
        .to_string();

    let order = image_provider_order(&state, &requested_model).await;
    let mut last_error: Option<Response> = None;
    for provider in &order {
        let result = match provider.as_str() {
            "minimax" => {
                serve_minimax_images(
                    &state,
                    &user_id,
                    shared_only,
                    &payload,
                    prompt,
                    &requested_model,
                )
                .await
            }
            "codex" => {
                serve_codex_images(
                    &state,
                    &user_id,
                    shared_only,
                    &payload,
                    prompt,
                    &requested_model,
                )
                .await
            }
            _ => continue,
        };
        match result {
            ImageProviderOutcome::Served(response) => return response,
            ImageProviderOutcome::NextProvider(error) => {
                if error.is_some() {
                    last_error = error;
                }
            }
        }
    }

    last_error.unwrap_or_else(|| {
        openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "image_provider_unavailable",
            "no usable MiniMax or Codex image provider account",
        )
    })
}

/// The standard extractor rejects a malformed `Content-Type` before the
/// handler runs. Some image clients send a valid multipart body while omitting
/// or corrupting that header's boundary, so recover the delimiter from the
/// first body line when it is safe to do so.
fn multipart_boundary_from_body(body: &[u8]) -> Option<String> {
    let line_end = body.iter().position(|byte| *byte == b'\n')?;
    let line = body[..line_end]
        .strip_suffix(b"\r")
        .unwrap_or(&body[..line_end]);
    let boundary = line.strip_prefix(b"--")?;
    if boundary.is_empty() || boundary.len() > 70 || !boundary.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    std::str::from_utf8(boundary).ok().map(str::to_string)
}

fn image_edit_boundary(headers: &HeaderMap, body: &[u8]) -> Result<String, String> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "Missing Content-Type: multipart/form-data".to_string())?;
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err("Content-Type must be multipart/form-data".to_string());
    }

    let header_boundary = multer::parse_boundary(content_type).ok();
    let body_boundary = multipart_boundary_from_body(body);
    match (header_boundary, body_boundary) {
        (Some(header), Some(body)) if header == body => Ok(header),
        (Some(_), Some(body)) => {
            warn!(
                "image edits multipart boundary disagreed with body; recovered boundary from body"
            );
            Ok(body)
        }
        (None, Some(body)) => {
            warn!("image edits multipart request had an invalid boundary header; recovered boundary from body");
            Ok(body)
        }
        (Some(header), None) => Ok(header),
        (None, None) => Err("Invalid boundary for multipart/form-data request".to_string()),
    }
}

fn image_edit_request_limit() -> usize {
    std::env::var("GATEWAY_MAX_REQUEST_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(64 * 1024 * 1024)
}

fn image_edit_upstream_content_type(boundary: &str) -> Result<HeaderValue, String> {
    let escaped = boundary.replace('\\', "\\\\").replace('"', "\\\"");
    HeaderValue::from_str(&format!(
        "multipart/form-data; boundary=\"{}\"",
        escaped
    ))
    .map_err(|e| format!("invalid multipart boundary: {}", e))
}

struct ImageEditMetadata {
    prompt_length: usize,
    requested_model: String,
}

/// Read only the fields needed for routing/audit from a cheap clone of the
/// buffered body. The original bytes are never rebuilt and are what the
/// upstream receives. Malformed fields are left for the upstream API to
/// validate so the gateway remains transport-transparent.
async fn multipart_image_edit_metadata(
    body: axum::body::Bytes,
    boundary: String,
) -> ImageEditMetadata {
    let stream = futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(body)
    });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut prompt_length = 0;
    let mut requested_model = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                warn!("could not inspect image edits multipart metadata: {}", error);
                break;
            }
        };
        match field.name() {
            Some("prompt") => {
                if let Ok(text) = field.text().await {
                    prompt_length = text.chars().count();
                }
            }
            Some("model") => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        requested_model = Some(trimmed.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    ImageEditMetadata {
        prompt_length,
        requested_model: requested_model.unwrap_or_else(|| "gpt-image-2".to_string()),
    }
}

fn json_image_edit_metadata(body: &[u8]) -> ImageEditMetadata {
    let payload = serde_json::from_slice::<Value>(body).ok();
    let prompt_length = payload
        .as_ref()
        .and_then(|value| value.get("prompt"))
        .and_then(Value::as_str)
        .map(|prompt| prompt.chars().count())
        .unwrap_or(0);
    let requested_model = payload
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("gpt-image-2")
        .to_string();
    ImageEditMetadata {
        prompt_length,
        requested_model,
    }
}

/// Preserve the caller's media type. Multipart requests get one narrow repair:
/// if the declared boundary is missing or disagrees with the body, forward the
/// effective body boundary. JSON and future media types pass through untouched.
fn image_edit_transport(
    headers: &HeaderMap,
    body: &[u8],
) -> (Option<HeaderValue>, Option<String>) {
    let Some(content_type) = headers.get(CONTENT_TYPE).cloned() else {
        return (None, None);
    };
    let is_multipart = content_type
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .map(|media_type| media_type.eq_ignore_ascii_case("multipart/form-data"))
        .unwrap_or(false);
    if !is_multipart {
        return (Some(content_type), None);
    }
    match image_edit_boundary(headers, body) {
        Ok(boundary) => match image_edit_upstream_content_type(&boundary) {
            Ok(content_type) => (Some(content_type), Some(boundary)),
            Err(error) => {
                warn!("could not normalize image edits multipart Content-Type: {}", error);
                (Some(content_type), None)
            }
        },
        Err(error) => {
            warn!("forwarding malformed image edits Content-Type for upstream validation: {}", error);
            (Some(content_type), None)
        }
    }
}

pub(crate) async fn proxy_image_edits(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let caller = identify_caller(&headers);
    let user_id = caller.id;
    let shared_only = !caller.owner_trusted;

    let body = match axum::body::to_bytes(body, image_edit_request_limit()).await {
        Ok(body) => body,
        Err(error) => {
            return openai_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                &format!("failed reading image edits body: {}", error),
            )
        }
    };
    let (upstream_content_type, multipart_boundary) = image_edit_transport(&headers, &body);
    let metadata = match multipart_boundary {
        Some(boundary) => multipart_image_edit_metadata(body.clone(), boundary).await,
        None => json_image_edit_metadata(&body),
    };
    let requested_model = metadata.requested_model;
    let prompt_length = metadata.prompt_length;

    let owner_trusted = !shared_only;
    let owned_only =
        match crate::quota::enforce_user_quota(&state, "codex", &user_id, owner_trusted).await {
            Ok(value) => value,
            Err(response) => return response,
        };

    let budget = provider_attempt_budget(&state, "codex").await;
    let mut excluded = HashSet::new();
    let mut last_error: Option<Response> = None;
    for _ in 0..budget {
        let Some(account) = select_image_account(
            &state,
            "codex",
            &user_id,
            owned_only,
            shared_only,
            &excluded,
        )
        .await
        else {
            break;
        };
        excluded.insert(account.id.clone());
        let (response, account) = match codex::send_codex_image_edits_upstream_with_refresh(
            &state,
            &account,
            upstream_content_type.as_ref(),
            &body,
        )
        .await
        {
            Ok(result) => result,
            Err(message) => {
                warn!("Codex image edits transport failed: {}", message);
                last_error = Some(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Codex image edits provider transport failed",
                ));
                continue;
            }
        };
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let response_content_type = response.headers().get(CONTENT_TYPE).cloned();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                warn!("failed reading Codex image edits response: {}", error);
                last_error = Some(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "failed reading Codex image edits response",
                ));
                continue;
            }
        };
        if status.is_success() {
            reset_backoff(&state, &account.id).await;
            write_image_audit(
                &state,
                &user_id,
                &account,
                "codex",
                &requested_model,
                &requested_model,
                prompt_length,
                "success",
            )
            .await;
            return image_edit_upstream_response(
                status,
                response_content_type,
                request_id.as_deref(),
                body,
            );
        }

        let class = ErrorClass::from_status(status.as_u16());
        warn!(
            "Codex image edits upstream returned {}: {}",
            status.as_u16(),
            crate::util::truncate_text(&String::from_utf8_lossy(&body), 500)
        );
        apply_account_failure(&state, &account.id, class, None, retry_after, false).await;
        last_error = Some(image_edit_upstream_response(
            status,
            response_content_type,
            request_id.as_deref(),
            body,
        ));
        if !class.is_retryable() {
            break;
        }
    }

    last_error.unwrap_or_else(|| {
        openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "image_provider_unavailable",
            "no usable Codex image edits provider account",
        )
    })
}

fn image_edit_upstream_response(
    status: StatusCode,
    content_type: Option<HeaderValue>,
    request_id: Option<&str>,
    body: axum::body::Bytes,
) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    if let Some(request_id) = request_id {
        if let Ok(value) = HeaderValue::from_str(request_id) {
            builder = builder.header("x-request-id", value);
        }
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed building image edits response",
            )
        })
}

enum ImageProviderOutcome {
    Served(Response),
    NextProvider(Option<Response>),
}

async fn image_provider_order(state: &AppState, requested_model: &str) -> Vec<String> {
    let cfg = state.chains.read().await.for_slot(ChainSlot::Codex).clone();
    let rr_offset = if matches!(cfg.mode, ChainMode::RoundRobin) {
        let mut rr = state.chain_rr.lock().await;
        let counter = rr.entry("images".to_string()).or_insert(0);
        let current = *counter;
        *counter = counter.wrapping_add(1);
        current
    } else {
        0
    };
    ordered_image_providers(&cfg, requested_model, rr_offset)
}

fn ordered_image_providers(
    cfg: &crate::provider::chains::ChainCfg,
    requested_model: &str,
    rr_offset: usize,
) -> Vec<String> {
    if requested_model.eq_ignore_ascii_case("image-01")
        || requested_model
            .to_ascii_lowercase()
            .starts_with("minimax/image-01")
    {
        return vec!["minimax".to_string(), "codex".to_string()];
    }
    let mut order: Vec<String> = ordered_attempts(cfg, rr_offset)
        .into_iter()
        .filter(|provider| matches!(provider.as_str(), "minimax" | "codex"))
        .collect();
    if !order.iter().any(|provider| provider == "codex") {
        order.push("codex".to_string());
    }
    order
}

async fn serve_minimax_images(
    state: &AppState,
    user_id: &str,
    shared_only: bool,
    payload: &Value,
    prompt: &str,
    requested_model: &str,
) -> ImageProviderOutcome {
    let upstream_payload = match minimax::build_minimax_image_request(payload) {
        Ok(payload) => payload,
        Err(message) => {
            tracing::debug!("MiniMax image translation skipped: {}", message);
            return ImageProviderOutcome::NextProvider(None);
        }
    };
    let owner_trusted = !shared_only;
    let owned_only =
        match crate::quota::enforce_user_quota(state, "minimax", user_id, owner_trusted).await {
            Ok(value) => value,
            Err(response) => return ImageProviderOutcome::NextProvider(Some(response)),
        };
    let budget = provider_attempt_budget(state, "minimax").await;
    let mut excluded = HashSet::new();
    let mut last_error = None;
    for _ in 0..budget {
        let Some(account) = select_image_account(
            state,
            "minimax",
            user_id,
            owned_only,
            shared_only,
            &excluded,
        )
        .await
        else {
            break;
        };
        excluded.insert(account.id.clone());
        let response =
            match minimax::send_minimax_image_generation(&account, &upstream_payload).await {
                Ok(response) => response,
                Err(message) => {
                    warn!("MiniMax image transport failed: {}", message);
                    last_error = Some(openai_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        "MiniMax image provider transport failed",
                    ));
                    continue;
                }
            };
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                warn!("failed reading MiniMax image response: {}", error);
                last_error = Some(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "failed reading MiniMax image response",
                ));
                continue;
            }
        };
        if status.is_success() {
            match minimax::normalize_minimax_image_response(&body) {
                Ok(normalized) => {
                    reset_backoff(state, &account.id).await;
                    write_image_audit(
                        state,
                        user_id,
                        &account,
                        "minimax",
                        requested_model,
                        "image-01",
                        prompt.chars().count(),
                        "success",
                    )
                    .await;
                    return ImageProviderOutcome::Served(
                        (StatusCode::OK, Json(normalized)).into_response(),
                    );
                }
                Err(message) => {
                    warn!("MiniMax image response rejected: {}", message);
                    last_error = Some(openai_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        "MiniMax image provider returned an invalid response",
                    ));
                    continue;
                }
            }
        }

        let class = ErrorClass::from_status(status.as_u16());
        warn!(
            "MiniMax image upstream returned {}: {}",
            status.as_u16(),
            crate::util::truncate_text(&String::from_utf8_lossy(&body), 500)
        );
        apply_account_failure(state, &account.id, class, None, retry_after, false).await;
        last_error = Some(openai_error(
            status,
            "upstream_error",
            &format!("MiniMax image provider returned status {}", status.as_u16()),
        ));
        if !class.is_retryable() {
            break;
        }
    }
    ImageProviderOutcome::NextProvider(last_error)
}

async fn serve_codex_images(
    state: &AppState,
    user_id: &str,
    shared_only: bool,
    payload: &Value,
    prompt: &str,
    requested_model: &str,
) -> ImageProviderOutcome {
    let upstream_payload = codex_image_payload(payload);
    let owner_trusted = !shared_only;
    let owned_only =
        match crate::quota::enforce_user_quota(state, "codex", user_id, owner_trusted).await {
            Ok(value) => value,
            Err(response) => return ImageProviderOutcome::NextProvider(Some(response)),
        };
    let budget = provider_attempt_budget(state, "codex").await;
    let mut excluded = HashSet::new();
    let mut last_error = None;
    for _ in 0..budget {
        let Some(account) =
            select_image_account(state, "codex", user_id, owned_only, shared_only, &excluded).await
        else {
            break;
        };
        excluded.insert(account.id.clone());
        let (response, account) = match codex::send_codex_images_upstream_with_refresh(
            state,
            &account,
            &upstream_payload,
        )
        .await
        {
            Ok(result) => result,
            Err(message) => {
                warn!("Codex image transport failed: {}", message);
                last_error = Some(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Codex image provider transport failed",
                ));
                continue;
            }
        };
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                warn!("failed reading Codex image response: {}", error);
                last_error = Some(openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "failed reading Codex image response",
                ));
                continue;
            }
        };
        if status.is_success() {
            reset_backoff(state, &account.id).await;
            write_image_audit(
                state,
                user_id,
                &account,
                "codex",
                requested_model,
                requested_model,
                prompt.chars().count(),
                "success",
            )
            .await;
            let mut client_response = Response::builder()
                .status(status)
                .header(CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    openai_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "gateway_error",
                        "failed building image response",
                    )
                });
            if let Some(request_id) =
                request_id.and_then(|value| HeaderValue::from_str(&value).ok())
            {
                client_response
                    .headers_mut()
                    .insert("x-request-id", request_id);
            }
            return ImageProviderOutcome::Served(client_response);
        }

        let class = ErrorClass::from_status(status.as_u16());
        warn!(
            "Codex image upstream returned {}: {}",
            status.as_u16(),
            crate::util::truncate_text(&String::from_utf8_lossy(&body), 500)
        );
        apply_account_failure(state, &account.id, class, None, retry_after, false).await;
        last_error = Some(openai_error(
            status,
            "upstream_error",
            &format!("Codex image provider returned status {}", status.as_u16()),
        ));
        if !class.is_retryable() {
            break;
        }
    }
    ImageProviderOutcome::NextProvider(last_error)
}

async fn select_image_account(
    state: &AppState,
    provider: &str,
    user_id: &str,
    owned_only: bool,
    shared_only: bool,
    excluded: &HashSet<String>,
) -> Option<UpstreamAccount> {
    let now = Utc::now();
    let selected = {
        let accounts = state.accounts.read().await;
        let rate_limits = state.rate_limits.read().await;
        let owner_usage = state.owner_usage.read().await;
        let mut candidates = eligible_accounts(&accounts, provider, user_id, excluded, now, false);
        if owned_only {
            candidates.retain(|account| account.owner_user_id == user_id);
        }
        if shared_only {
            candidates.retain(|account| {
                account.share_enabled && account_visible_to_user(account, user_id)
            });
        }
        let warm =
            select_account_for_request(&candidates, user_id, provider, &rate_limits, &owner_usage);
        if warm.is_some() {
            warm
        } else {
            let mut cooling = eligible_accounts(&accounts, provider, user_id, excluded, now, true);
            if owned_only {
                cooling.retain(|account| account.owner_user_id == user_id);
            }
            if shared_only {
                cooling.retain(|account| {
                    account.share_enabled && account_visible_to_user(account, user_id)
                });
            }
            let cooling = prefer_near_expiry(cooling, now);
            select_account_for_request(&cooling, user_id, provider, &rate_limits, &owner_usage)
        }
    };
    if let Some(account) = &selected {
        note_account_pick(state, &account.id).await;
    }
    selected
}

fn image_prompt(payload: &Value) -> Result<&str, String> {
    payload
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| "Missing required parameter: 'prompt'.".to_string())
}

fn codex_image_payload(payload: &Value) -> Value {
    let mut payload = payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        let model = obj.get("model").and_then(Value::as_str).unwrap_or("");
        if model.is_empty()
            || model.eq_ignore_ascii_case("image-01")
            || model.to_ascii_lowercase().starts_with("minimax/image-01")
        {
            obj.insert(
                "model".to_string(),
                Value::String("gpt-image-2".to_string()),
            );
        }
        obj.remove("seed");
    }
    payload
}

fn openai_error(status: StatusCode, error_type: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": Value::Null,
                "code": Value::Null,
            }
        })),
    )
        .into_response()
}

#[allow(clippy::too_many_arguments)]
async fn write_image_audit(
    state: &AppState,
    user_id: &str,
    account: &UpstreamAccount,
    provider: &str,
    requested_model: &str,
    upstream_model: &str,
    prompt_length: usize,
    status: &str,
) {
    let audit = AuditRecord {
        request_id: Uuid::new_v4().to_string(),
        user_id: user_id.to_string(),
        requested_model: requested_model.to_string(),
        upstream_model: upstream_model.to_string(),
        routing_rule: None,
        model: upstream_model.to_string(),
        routed_provider: provider.to_string(),
        upstream_account_id: account.id.clone(),
        upstream_owner_user_id: account.owner_user_id.clone(),
        prompt_length,
        output_length: 0,
        status: status.to_string(),
        created_at: Utc::now(),
        tokens: TokenUsage::default(),
        origin: crate::auth::current_origin().unwrap_or_else(|| "codex-imagegen".to_string()),
    };
    if let Err(error) = append_audit(state, &audit).await {
        error!("failed writing image audit record: {}", error);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        codex_image_payload, image_edit_boundary, image_edit_transport,
        image_edit_upstream_content_type, image_prompt, json_image_edit_metadata,
        multipart_image_edit_metadata, ordered_image_providers,
    };
    use axum::body::Bytes;
    use crate::provider::chains::{ChainCfg, ChainMode};
    use axum::http::{header::CONTENT_TYPE, HeaderMap, HeaderValue};
    use serde_json::json;

    #[test]
    fn prompt_validation_matches_openai_shape() {
        assert_eq!(
            image_prompt(&json!({"prompt":"  draw a cat  "})).unwrap(),
            "draw a cat"
        );
        assert!(image_prompt(&json!({"prompt":"  "})).is_err());
        assert!(image_prompt(&json!({})).is_err());
    }

    #[test]
    fn image_order_uses_capable_chain_members_and_always_falls_back_to_codex() {
        let cfg = ChainCfg {
            mode: ChainMode::Failover,
            providers: vec!["minimax".into(), "deepseek".into()],
        };
        assert_eq!(
            ordered_image_providers(&cfg, "gpt-image-2", 0),
            vec!["minimax", "codex"]
        );
        assert_eq!(
            ordered_image_providers(&cfg, "minimax/image-01-live", 0),
            vec!["minimax", "codex"]
        );
    }

    #[test]
    fn image_order_honors_round_robin_for_capable_providers() {
        let cfg = ChainCfg {
            mode: ChainMode::RoundRobin,
            providers: vec!["minimax".into(), "codex".into()],
        };
        assert_eq!(
            ordered_image_providers(&cfg, "gpt-image-2", 0),
            vec!["minimax", "codex"]
        );
        assert_eq!(
            ordered_image_providers(&cfg, "gpt-image-2", 1),
            vec!["codex", "minimax"]
        );
    }

    #[test]
    fn codex_fallback_restores_the_native_image_model() {
        let payload = codex_image_payload(&json!({
            "model": "minimax/image-01",
            "prompt": "draw",
            "seed": 42,
        }));
        assert_eq!(payload["model"], "gpt-image-2");
        assert!(payload.get("seed").is_none());
        assert_eq!(
            codex_image_payload(&json!({"prompt":"draw"}))["model"],
            "gpt-image-2"
        );
    }

    #[tokio::test]
    async fn image_edit_metadata_ignores_unknown_fields_without_rebuilding_body() {
        let body = Bytes::from_static(
            b"--test-boundary\r\nContent-Disposition: form-data; name=\"future_field\"\r\n\r\nkeep-me\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nadd a red hat\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-next\r\n--test-boundary--\r\n",
        );
        let original = body.clone();
        let metadata = multipart_image_edit_metadata(body, "test-boundary".to_string()).await;
        assert_eq!(metadata.prompt_length, "add a red hat".chars().count());
        assert_eq!(metadata.requested_model, "gpt-image-next");
        assert!(String::from_utf8_lossy(&original).contains("future_field"));
        assert!(String::from_utf8_lossy(&original).contains("keep-me"));
    }

    #[test]
    fn json_image_edit_metadata_reads_openai_reference_shape() {
        let body = br#"{"images":[{"image_url":"data:image/png;base64,AAAA"}],"prompt":"paint it blue","model":"gpt-image-2"}"#;
        let metadata = json_image_edit_metadata(body);
        assert_eq!(metadata.prompt_length, "paint it blue".chars().count());
        assert_eq!(metadata.requested_model, "gpt-image-2");
    }

    #[test]
    fn image_edit_transport_preserves_json_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        let (content_type, boundary) = image_edit_transport(&headers, b"{}");
        assert_eq!(
            content_type.unwrap(),
            HeaderValue::from_static("application/json; charset=utf-8")
        );
        assert!(boundary.is_none());
    }

    #[test]
    fn image_edit_content_type_keeps_the_effective_boundary() {
        let content_type = image_edit_upstream_content_type("client-boundary").unwrap();
        assert_eq!(
            content_type.to_str().unwrap(),
            "multipart/form-data; boundary=\"client-boundary\""
        );
    }

    #[test]
    fn image_edit_recovers_missing_boundary_from_a_valid_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data"),
        );
        let body = b"--client-boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ndraw\r\n--client-boundary--\r\n";
        assert_eq!(
            image_edit_boundary(&headers, body).unwrap(),
            "client-boundary"
        );
    }

    #[test]
    fn image_edit_recovers_a_boundary_that_disagrees_with_the_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("multipart/form-data; boundary=incorrect"),
        );
        let body = b"--actual-boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ndraw\r\n--actual-boundary--\r\n";
        assert_eq!(
            image_edit_boundary(&headers, body).unwrap(),
            "actual-boundary"
        );
    }

    #[test]
    fn multipart_boundary_parser_rejects_non_multipart_requests() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(image_edit_boundary(&headers, b"{}").is_err());
    }
}
