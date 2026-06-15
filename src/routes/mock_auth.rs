//! Mock ChatGPT auth endpoints for Codex clients pointed at the gateway. The
//! client never sees a real upstream credential — we echo its gateway bearer
//! back as a never-expiring "session" so its auth-recovery flow stays dormant.
use crate::prelude::*;

fn mock_session_body(headers: &HeaderMap) -> Value {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("Bearer user:anonymous")
        .strip_prefix("Bearer ")
        .unwrap_or("user:anonymous");

    json!({
        "user": {
            "id": token,
            "name": token,
            "email": format!("{}@org-ai-gateway.local", token),
            "image": "",
            "picture": "",
            "groups": []
        },
        "expires": "2099-12-31T23:59:59.000Z",
        "accessToken": token,
        "authProvider": "org-ai-gateway"
    })
}

/// `GET /backend-api/auth/session`
pub(crate) async fn mock_auth_session(headers: HeaderMap) -> impl IntoResponse {
    (StatusCode::OK, Json(mock_session_body(&headers))).into_response()
}

/// `POST /backend-api/auth/refresh` — same shape as the session endpoint.
pub(crate) async fn mock_auth_refresh(headers: HeaderMap) -> impl IntoResponse {
    (StatusCode::OK, Json(mock_session_body(&headers))).into_response()
}
