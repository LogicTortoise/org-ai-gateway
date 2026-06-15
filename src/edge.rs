//! Trusted-edge identity: lets ANY external auth system (company SSO, OAuth,
//! homegrown login) sit in front of the gateway with zero SDK/protocol
//! coupling. The edge authenticates the end user however it likes, then
//! forwards the request with an identity header (default `X-User-Id`) naming
//! who the caller is. The gateway never does login itself — it just trusts
//! that header, same model as oauth2-proxy / Envoy ext_authz / nginx
//! auth_request.
//!
//! Trust is gated: the identity header is honored ONLY when the edge proves
//! itself with a shared secret header (default `X-Gateway-Auth`, value of env
//! `GATEWAY_EDGE_SECRET`). Without that gate anyone with network reach could
//! forge `X-User-Id` and impersonate. For fully isolated networks,
//! `GATEWAY_TRUST_USER_HEADER=true` skips the secret — startup logs a loud
//! warning because the only protection left is network reachability.
//!
//! Requests without the trusted headers fall through to the existing bearer
//! logic (`user:<id>`), so local development and current deployments are
//! unaffected.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

/// Resolved edge-trust configuration. Read from env per request; header names
/// are normalized to lowercase.
pub(crate) struct EdgeConfig {
    /// Shared secret the edge must present. None + `allow_unauthenticated`
    /// false ⇒ feature off.
    pub(crate) secret: Option<String>,
    /// `GATEWAY_TRUST_USER_HEADER`: trust the identity header with no secret.
    pub(crate) allow_unauthenticated: bool,
    pub(crate) secret_header: String,
    pub(crate) user_id_header: String,
}

impl EdgeConfig {
    pub(crate) fn from_env() -> Self {
        EdgeConfig {
            secret: std::env::var("GATEWAY_EDGE_SECRET")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            allow_unauthenticated: env_truthy("GATEWAY_TRUST_USER_HEADER"),
            secret_header: header_name_from_env("GATEWAY_EDGE_SECRET_HEADER", "x-gateway-auth"),
            user_id_header: header_name_from_env("GATEWAY_USER_ID_HEADER", "x-user-id"),
        }
    }

    fn enabled(&self) -> bool {
        self.secret.is_some() || self.allow_unauthenticated
    }
}

fn header_name_from_env(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// The user id asserted by a trusted edge, if this request carries one.
/// None ⇒ not an edge request (or proof failed); caller falls through to the
/// normal bearer logic.
pub(crate) fn trusted_user_id(headers: &HeaderMap) -> Option<String> {
    trusted_user_id_with(headers, &EdgeConfig::from_env())
}

/// Whether trusted-edge identity is configured at all (a secret is set, or
/// open mode is on). When true, the self-asserted `user:<id>` bearer must no
/// longer be honored as a fallback — otherwise a caller could bypass the edge
/// entirely by simply omitting the edge headers.
pub(crate) fn edge_enabled() -> bool {
    EdgeConfig::from_env().enabled()
}

pub(crate) fn trusted_user_id_with(headers: &HeaderMap, cfg: &EdgeConfig) -> Option<String> {
    if !cfg.enabled() {
        return None;
    }
    if let Some(secret) = &cfg.secret {
        let presented = headers
            .get(cfg.secret_header.as_str())
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .unwrap_or("");
        if !constant_time_eq(presented, secret) {
            return None;
        }
    }
    headers
        .get(cfg.user_id_header.as_str())
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

/// Compare via SHA-256 digests so the comparison cost doesn't leak how many
/// leading bytes of the secret matched.
fn constant_time_eq(a: &str, b: &str) -> bool {
    Sha256::digest(a.as_bytes()) == Sha256::digest(b.as_bytes())
}

/// One-shot startup announcement of the edge-trust mode, so a misconfigured
/// (or dangerously open) deployment is visible in the first lines of the log.
pub(crate) fn log_startup() {
    let cfg = EdgeConfig::from_env();
    if cfg.secret.is_some() {
        tracing::info!(
            "trusted-edge identity enabled: honoring `{}` when `{}` matches GATEWAY_EDGE_SECRET",
            cfg.user_id_header,
            cfg.secret_header
        );
    } else if cfg.allow_unauthenticated {
        tracing::warn!(
            "trusted-edge identity enabled WITHOUT a secret (GATEWAY_TRUST_USER_HEADER): anyone \
             who can reach this gateway can impersonate any user via `{}`. Only safe when the \
             gateway is reachable exclusively from the trusted edge.",
            cfg.user_id_header
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn cfg(secret: Option<&str>, open: bool) -> EdgeConfig {
        EdgeConfig {
            secret: secret.map(|s| s.to_string()),
            allow_unauthenticated: open,
            secret_header: "x-gateway-auth".to_string(),
            user_id_header: "x-user-id".to_string(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn disabled_by_default() {
        let h = headers(&[("x-user-id", "koltyu")]);
        assert_eq!(trusted_user_id_with(&h, &cfg(None, false)), None);
    }

    #[test]
    fn secret_gates_trust() {
        let c = cfg(Some("s3cret"), false);
        let ok = headers(&[("x-gateway-auth", "s3cret"), ("x-user-id", "koltyu")]);
        assert_eq!(trusted_user_id_with(&ok, &c), Some("koltyu".to_string()));

        let wrong = headers(&[("x-gateway-auth", "nope"), ("x-user-id", "koltyu")]);
        assert_eq!(trusted_user_id_with(&wrong, &c), None);

        let missing = headers(&[("x-user-id", "koltyu")]);
        assert_eq!(trusted_user_id_with(&missing, &c), None);
    }

    #[test]
    fn open_mode_trusts_header_directly() {
        let c = cfg(None, true);
        let h = headers(&[("x-user-id", " bob ")]);
        assert_eq!(trusted_user_id_with(&h, &c), Some("bob".to_string()));
        // No identity header ⇒ still falls through.
        assert_eq!(trusted_user_id_with(&headers(&[]), &c), None);
    }

    #[test]
    fn empty_user_id_not_trusted() {
        let c = cfg(Some("s"), false);
        let h = headers(&[("x-gateway-auth", "s"), ("x-user-id", "  ")]);
        assert_eq!(trusted_user_id_with(&h, &c), None);
    }
}
