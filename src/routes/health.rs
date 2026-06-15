use crate::prelude::*;

pub(crate) async fn fallback_handler(req: Request<axum::body::Body>) -> impl IntoResponse {
    let method = req.method().clone();
    let uri = req.uri().clone();
    // warn, not error: unmatched requests are mostly scanners/probes and would
    // otherwise flood the error log (and could be used to spam it).
    warn!("Unhandled request: {} {}", method, uri);
    (StatusCode::NOT_FOUND, format!("Not found: {} {}", method, uri)).into_response()
}

pub(crate) async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "org-ai-gateway",
        time: Utc::now(),
    })
}

/// Report the identity the gateway resolves for THIS request, so the dashboard
/// can show the real logged-in user instead of a manually-typed id. Never
/// rejects — uses the same non-failing resolution as the proxy paths.
///
/// `edge_authenticated` is the key flag for the UI: when true, the identity came
/// from the trusted edge (the team's own auth) and the dashboard should lock the
/// "current user" field to it; otherwise the deployment is local/self-asserted
/// and the field stays editable.
pub(crate) async fn whoami(headers: HeaderMap) -> Json<WhoamiResponse> {
    let caller = crate::auth::identify_caller(&headers);
    Json(WhoamiResponse {
        user_id: caller.id,
        owner_trusted: caller.owner_trusted,
        edge_enabled: crate::edge::edge_enabled(),
        edge_authenticated: crate::edge::trusted_user_id(&headers).is_some(),
    })
}

/// The admin dashboard, compiled into the binary so a single-file deploy needs
/// no static-file serving. NB: the page itself pulls Tailwind from a CDN.
pub(crate) async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../assets/ui.html"))
}

/// The remote-donation helper script (`curl https://gateway/donate.sh | sh`).
/// Compiled into the binary; the gateway base URL is injected from the request
/// host so the one-liner needs no extra env. Honors `X-Forwarded-Proto`/`-Host`
/// so it produces the public URL when behind a reverse proxy / trusted edge.
pub(crate) async fn donate_script(headers: HeaderMap) -> impl IntoResponse {
    const SCRIPT: &str = include_str!("../../assets/donate.sh");

    let header_str = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let proto = header_str("x-forwarded-proto")
        .map(|p| p.split(',').next().unwrap_or("http").trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = header_str("x-forwarded-host")
        .or_else(|| header_str("host"))
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let base_url = format!("{}://{}", proto, host);

    // replacen(.., 1): only the default-assignment line carries the injected URL.
    // The later guard keeps the literal sentinel so a script run without injection
    // (and without a GATEWAY env override) still fails closed.
    let body = SCRIPT.replacen("__GATEWAY_BASE_URL__", &base_url, 1);
    (
        [(CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        body,
    )
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    pub(crate) status: &'static str,
    pub(crate) service: &'static str,
    pub(crate) time: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WhoamiResponse {
    pub(crate) user_id: String,
    pub(crate) owner_trusted: bool,
    pub(crate) edge_enabled: bool,
    pub(crate) edge_authenticated: bool,
}