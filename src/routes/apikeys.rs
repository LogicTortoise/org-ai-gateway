//! Management endpoints for gateway-issued API keys (the "open API" path).
//!
//! All three require the caller to be authenticated (edge identity, an existing
//! API key, or a `user:<id>` bearer when no edge is configured). A caller only
//! ever sees and revokes keys they own.

use crate::apikey;
use crate::auth::extract_user_id;
use crate::prelude::*;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateApiKeyRequest {
    #[serde(default)]
    pub(crate) label: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ApiKeySummary {
    pub(crate) id: String,
    pub(crate) display: String,
    pub(crate) label: String,
    pub(crate) owner_user_id: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) revoked: bool,
}

impl From<apikey::ApiKeyRecord> for ApiKeySummary {
    fn from(r: apikey::ApiKeyRecord) -> Self {
        ApiKeySummary {
            id: r.id,
            display: r.display,
            label: r.label,
            owner_user_id: r.owner_user_id,
            created_at: r.created_at,
            revoked: r.revoked,
        }
    }
}

fn unauthorized(err: String) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": err }))).into_response()
}

/// `POST /v1/apikeys` — mint a key for the authenticated caller. The plaintext
/// `key` is returned exactly once here and never again.
pub(crate) async fn create_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };
    let label = {
        let l = payload.label.trim();
        if l.is_empty() {
            format!("{}-key", user_id)
        } else {
            l.to_string()
        }
    };

    let created = apikey::create(user_id, label);
    if let Err(e) = apikey::persist(&state).await {
        // Roll the in-memory addition back so a failed persist doesn't leave a
        // key that vanishes on restart (and whose plaintext we just handed out).
        apikey::revoke_owned(&created.record.id, &created.record.owner_user_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed persisting api key: {}", e) })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "key": created.plaintext,
            "note": "请立即复制保存：密钥明文只在此处显示这一次。",
            "api_key": ApiKeySummary::from(created.record),
        })),
    )
        .into_response()
}

/// `GET /v1/apikeys` — list the caller's own keys (newest first). No plaintext.
pub(crate) async fn list_api_keys(
    State(_state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };

    let mut keys: Vec<ApiKeySummary> = apikey::snapshot()
        .into_iter()
        .filter(|r| r.owner_user_id == user_id)
        .map(ApiKeySummary::from)
        .collect();
    keys.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    (
        StatusCode::OK,
        Json(json!({ "current_user_id": user_id, "api_keys": keys })),
    )
        .into_response()
}

/// `DELETE /v1/apikeys/:id` — revoke one of the caller's own keys.
pub(crate) async fn delete_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let user_id = match extract_user_id(&headers) {
        Ok(uid) => uid,
        Err(err) => return unauthorized(err),
    };

    if !apikey::revoke_owned(&id, &user_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "api key not found, already revoked, or not yours" })),
        )
            .into_response();
    }
    if let Err(e) = apikey::persist(&state).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("revoked in memory but failed persisting: {}", e) })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "revoked": true, "id": id }))).into_response()
}
