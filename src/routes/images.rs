//! OpenAI-compatible image-generation proxy.
//!
//! Codex's built-in image extension sends `POST /v1/images/generations` to
//! the active model-provider base URL. MiniMax exposes equivalent capability
//! through a different endpoint and schema, so this route translates MiniMax
//! attempts and falls back to the native ChatGPT Codex image bridge.
//!
//! `/v1/images/edits` (OpenAI multipart shape) is also routed here. The
//! Codex backend exposes `/backend-api/codex/images/edits` as a JSON endpoint
//! that takes `{prompt, images:[{image_url}], mask?, ...}`, so the multipart
//! fields are unpacked into that shape. There is no minimax fallback: MiniMax
//! has no mask/inpainting capability and its `subject_reference` is a
//! single-character reference, not a generic image edit.

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
use base64::Engine;
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

enum ImageEditField {
    Prompt,
    Image,
    Mask,
    N,
    Size,
    Model,
    ResponseFormat,
    Quality,
    Background,
}

fn classify_image_edit_field(name: &str) -> Option<ImageEditField> {
    match name {
        "prompt" => Some(ImageEditField::Prompt),
        "image" | "image[]" => Some(ImageEditField::Image),
        "mask" => Some(ImageEditField::Mask),
        "n" => Some(ImageEditField::N),
        "size" => Some(ImageEditField::Size),
        "model" => Some(ImageEditField::Model),
        "response_format" => Some(ImageEditField::ResponseFormat),
        "quality" => Some(ImageEditField::Quality),
        "background" => Some(ImageEditField::Background),
        _ => None,
    }
}

async fn read_field_data_url(field: multer::Field<'_>) -> Result<String, String> {
    let mime = field
        .content_type()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let bytes = field
        .bytes()
        .await
        .map_err(|e| format!("failed reading multipart field: {}", e))?;
    if bytes.is_empty() {
        return Err("multipart field has no bytes".to_string());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{};base64,{}", mime, encoded))
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
                &format!("failed reading multipart body: {}", error),
            )
        }
    };
    let boundary = match image_edit_boundary(&headers, &body) {
        Ok(boundary) => boundary,
        Err(message) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
    };
    let stream = futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary);

    let mut prompt: Option<String> = None;
    let mut image_data_urls: Vec<String> = Vec::new();
    let mut mask_data_url: Option<String> = None;
    let mut n_value: Option<u32> = None;
    let mut size_value: Option<String> = None;
    let mut model_value: Option<String> = None;
    let mut response_format_value: Option<String> = None;
    let mut quality_value: Option<String> = None;
    let mut background_value: Option<String> = None;

    while let Some(field) = match multipart.next_field().await {
        Ok(field) => field,
        Err(error) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("malformed multipart body: {}", error),
            )
        }
    } {
        let name = field.name().unwrap_or("").to_string();
        let kind = match classify_image_edit_field(&name) {
            Some(kind) => kind,
            None => continue,
        };
        match kind {
            ImageEditField::Prompt => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        prompt = Some(trimmed.to_string());
                    }
                }
            }
            ImageEditField::Image => match read_field_data_url(field).await {
                Ok(url) => image_data_urls.push(url),
                Err(message) => {
                    return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
                }
            },
            ImageEditField::Mask => match read_field_data_url(field).await {
                Ok(url) => mask_data_url = Some(url),
                Err(message) => {
                    return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
                }
            },
            ImageEditField::N => {
                if let Ok(text) = field.text().await {
                    if let Ok(parsed) = text.trim().parse::<u32>() {
                        n_value = Some(parsed);
                    }
                }
            }
            ImageEditField::Size => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        size_value = Some(trimmed.to_string());
                    }
                }
            }
            ImageEditField::Model => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        model_value = Some(trimmed.to_string());
                    }
                }
            }
            ImageEditField::ResponseFormat => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        response_format_value = Some(trimmed.to_string());
                    }
                }
            }
            ImageEditField::Quality => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        quality_value = Some(trimmed.to_string());
                    }
                }
            }
            ImageEditField::Background => {
                if let Ok(text) = field.text().await {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        background_value = Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    let prompt = match prompt {
        Some(p) => p,
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Missing required parameter: 'prompt'.",
            )
        }
    };
    if image_data_urls.is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Missing required parameter: 'image'.",
        );
    }
    let requested_model = model_value.unwrap_or_else(|| "gpt-image-2".to_string());

    let payload = build_codex_edits_payload(
        &prompt,
        &image_data_urls,
        mask_data_url.as_deref(),
        n_value,
        size_value.as_deref(),
        &requested_model,
        response_format_value.as_deref(),
        quality_value.as_deref(),
        background_value.as_deref(),
    );

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
        let (response, account) =
            match codex::send_codex_image_edits_upstream_with_refresh(&state, &account, &payload)
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
                        "failed building image edits response",
                    )
                });
            if let Some(request_id) =
                request_id.and_then(|value| HeaderValue::from_str(&value).ok())
            {
                client_response
                    .headers_mut()
                    .insert("x-request-id", request_id);
            }
            return client_response;
        }

        let class = ErrorClass::from_status(status.as_u16());
        warn!(
            "Codex image edits upstream returned {}: {}",
            status.as_u16(),
            crate::util::truncate_text(&String::from_utf8_lossy(&body), 500)
        );
        apply_account_failure(&state, &account.id, class, None, retry_after, false).await;
        last_error = Some(openai_error(
            status,
            "upstream_error",
            &format!(
                "Codex image edits provider returned status {}",
                status.as_u16()
            ),
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

fn build_codex_edits_payload(
    prompt: &str,
    images: &[String],
    mask: Option<&str>,
    n: Option<u32>,
    size: Option<&str>,
    model: &str,
    response_format: Option<&str>,
    quality: Option<&str>,
    background: Option<&str>,
) -> Value {
    let images_value: Vec<Value> = images
        .iter()
        .map(|url| json!({ "image_url": url }))
        .collect();
    let mut payload = json!({
        "prompt": prompt,
        "model": model,
        "images": images_value,
    });
    if let Some(obj) = payload.as_object_mut() {
        if let Some(n_value) = n {
            obj.insert("n".to_string(), json!(n_value));
        }
        if let Some(size_value) = size {
            obj.insert("size".to_string(), Value::String(size_value.to_string()));
        }
        if let Some(mask_value) = mask {
            obj.insert("mask".to_string(), Value::String(mask_value.to_string()));
        }
        if let Some(format_value) = response_format {
            obj.insert(
                "response_format".to_string(),
                Value::String(format_value.to_string()),
            );
        }
        if let Some(quality_value) = quality {
            obj.insert(
                "quality".to_string(),
                Value::String(quality_value.to_string()),
            );
        }
        if let Some(background_value) = background {
            obj.insert(
                "background".to_string(),
                Value::String(background_value.to_string()),
            );
        }
    }
    payload
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
        build_codex_edits_payload, codex_image_payload, image_edit_boundary, image_prompt,
        ordered_image_providers,
    };
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

    #[test]
    fn build_codex_edits_payload_serializes_single_image_as_images_array() {
        let payload = build_codex_edits_payload(
            "add a red hat",
            &["data:image/png;base64,AAAA".to_string()],
            None,
            None,
            None,
            "gpt-image-2",
            None,
            None,
            None,
        );
        assert_eq!(payload["prompt"], "add a red hat");
        assert_eq!(payload["model"], "gpt-image-2");
        let images = payload["images"].as_array().expect("images array");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["image_url"], "data:image/png;base64,AAAA");
        assert!(payload.get("mask").is_none());
        assert!(payload.get("n").is_none());
        assert!(payload.get("size").is_none());
    }

    #[test]
    fn build_codex_edits_payload_omits_mask_when_absent() {
        let payload = build_codex_edits_payload(
            "draw",
            &["data:image/png;base64,AAAA".to_string()],
            None,
            Some(2),
            Some("1024x1024"),
            "gpt-image-2",
            Some("b64_json"),
            None,
            None,
        );
        assert_eq!(payload["n"], 2);
        assert_eq!(payload["size"], "1024x1024");
        assert_eq!(payload["response_format"], "b64_json");
        assert!(payload.get("mask").is_none());
    }

    #[test]
    fn build_codex_edits_payload_keeps_mask_data_url_when_present() {
        let payload = build_codex_edits_payload(
            "draw",
            &["data:image/png;base64,AAAA".to_string()],
            Some("data:image/png;base64,BBBB"),
            None,
            None,
            "gpt-image-2",
            None,
            None,
            None,
        );
        assert_eq!(payload["mask"], "data:image/png;base64,BBBB");
    }

    #[test]
    fn build_codex_edits_payload_defaults_model_to_gpt_image_2() {
        let payload = build_codex_edits_payload(
            "draw",
            &["data:image/png;base64,AAAA".to_string()],
            None,
            None,
            None,
            "gpt-image-2",
            None,
            None,
            None,
        );
        assert_eq!(payload["model"], "gpt-image-2");
        assert!(payload.get("n").is_none());
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
    fn image_edit_rejects_non_multipart_requests() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(image_edit_boundary(&headers, b"{}").is_err());
    }
}
