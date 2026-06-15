use crate::prelude::*;

/// The raw bearer credential as presented (no parsing). Used by the client-config
/// "apply" flows to write whatever the caller authenticated with into the local
/// client config.
pub(crate) fn raw_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|t| t.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Authenticate a management request and return the owning user id.
///
/// Resolution order: a trusted edge (external auth in front of the gateway)
/// wins outright; otherwise the `user:<id>` bearer names the caller. The
/// `user:<id>` form is self-asserted — deploy behind a trusted edge (or keep
/// the gateway on an isolated network) when impersonation matters.
pub(crate) fn extract_user_id(headers: &HeaderMap) -> Result<String, String> {
    if let Some(user) = crate::edge::trusted_user_id(headers) {
        return Ok(user);
    }

    // A gateway-issued API key is a deliberately minted secret, so possessing it
    // IS authorization: honor it as its owner even when an edge is configured
    // (this is the external-integration path that can't perform browser SSO).
    if let Some(bearer) = raw_bearer(headers) {
        if let Some(owner) = crate::apikey::resolve(&bearer) {
            return Ok(owner);
        }
    }

    // Once a trusted edge is configured, the self-asserted `user:<id>` bearer is
    // no longer an accepted fallback: honoring it would let a caller bypass the
    // edge by omitting the edge headers and impersonate anyone via the bearer.
    if crate::edge::edge_enabled() {
        return Err("request must be authenticated by the trusted edge".to_string());
    }

    let value = headers
        .get(AUTHORIZATION)
        .ok_or_else(|| "missing Authorization header".to_string())?;

    let token = value
        .to_str()
        .map_err(|_| "invalid Authorization header".to_string())?;

    let bearer = token
        .strip_prefix("Bearer ")
        .ok_or_else(|| "Authorization must be Bearer token".to_string())?
        .trim();

    if let Some(user) = bearer.strip_prefix("user:") {
        if user.trim().is_empty() {
            return Err("user_id cannot be empty".to_string());
        }
        return Ok(user.trim().to_string());
    }

    Err("token format must be user:<user_id>".to_string())
}

/// Resolved caller identity for the non-rejecting proxy paths.
pub(crate) struct Caller {
    /// Identity used for audit, usage attribution, and shared-pool scoping.
    pub(crate) id: String,
    /// Whether this identity may be trusted to OWN accounts — i.e. select its
    /// own private, non-shared upstream accounts. True only for a trusted edge
    /// identity, or a `user:<id>` bearer when no trusted edge is configured.
    ///
    /// JWT-derived and anonymous identities are NEVER owner-trusted: the JWT is
    /// accepted without signature verification, so an attacker could otherwise
    /// forge an `email` claim to borrow a victim's private account. Such callers
    /// may only use the shared pool.
    pub(crate) owner_trusted: bool,
}

/// Identify the caller for audit/routing WITHOUT failing. The local client now
/// sends its real ChatGPT token; we never reject it (a 401 here would trigger
/// Codex's auth-recovery on the user's real account). Routing then falls back to
/// a shared pool account in `select_account_for_request`.
pub(crate) fn identify_caller(headers: &HeaderMap) -> Caller {
    // Trusted-edge identity first — same precedence as `extract_user_id`.
    if let Some(user) = crate::edge::trusted_user_id(headers) {
        return Caller { id: user, owner_trusted: true };
    }

    let token = headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|t| t.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();

    // Gateway-issued API key: owner-trusted, honored even behind an edge. Same
    // rationale as `extract_user_id` — the key itself proves authorization.
    if let Some(owner) = crate::apikey::resolve(token) {
        return Caller { id: owner, owner_trusted: true };
    }

    if let Some(id) = token.strip_prefix("user:") {
        if !id.trim().is_empty() {
            // A self-asserted bearer only confers ownership when no trusted edge
            // is configured (local/single-tenant). Behind an edge, ownership
            // must arrive through the edge identity header.
            return Caller {
                id: id.trim().to_string(),
                owner_trusted: !crate::edge::edge_enabled(),
            };
        }
    }
    // Real ChatGPT access token: pull the email claim for a friendlier audit id.
    // The signature is unverified, so this identity is audit-only and may never
    // own accounts — it is restricted to the shared pool.
    if let Some(email) = jwt_email(token) {
        return Caller { id: email, owner_trusted: false };
    }
    Caller { id: "codex-client".to_string(), owner_trusted: false }
}

/// Best-effort extraction of the email claim from a (possibly real) JWT.
pub(crate) fn jwt_email(jwt: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/profile")
        .and_then(|p| p.get("email"))
        .or_else(|| claims.get("email"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}


/// Best-effort extraction of the `exp` (expiry) claim from a JWT access token,
/// as a UTC timestamp. Codex/ChatGPT access tokens are JWTs carrying `exp`, so
/// this lets the proactive refresh loop and health heartbeat know when a token
/// is about to lapse without a network round-trip.
pub(crate) fn jwt_exp(jwt: &str) -> Option<DateTime<Utc>> {
    use base64::Engine;

    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = claims.get("exp").and_then(|v| v.as_i64())?;
    DateTime::<Utc>::from_timestamp(exp, 0)
}


/// Best-effort extraction of an arbitrary string claim from a JWT payload.
pub(crate) fn jwt_claim_str(jwt: &str, key: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

pub(crate) fn jwt_chatgpt_account_id(jwt: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|p| p.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}


