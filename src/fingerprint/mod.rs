//! Client fingerprint spoofing (HTTP layer), ported from `claude_fingerprint.go`,
//! `codex_fingerprint.go`, and the tool-name obfuscation in `claude_sdk_compat.go`.
//!
//! TLS/JA3 ClientHello: the Go `rustls_fingerprint.go` used `utls` to forge a
//! ClientHello mimicking reqwest/rustls (the real Codex client's TLS stack). This
//! gateway already *is* reqwest/rustls, so `rustls_tls` instead pins the rustls
//! `ClientConfig` (cipher-suite order, TLS 1.2/1.3, ALPN) to keep that handshake
//! fingerprint stable rather than forging one. Note this is rustls-level control,
//! not arbitrary ClientHello manipulation: forging a *non-rustls* fingerprint
//! (e.g. a Chrome JA3) would still require a uTLS-style stack like BoringSSL.
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod rustls_tls;
