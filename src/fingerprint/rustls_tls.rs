//! TLS/JA3 ClientHello pinning for the Codex/OpenAI upstream, ported from the Go
//! `rustls_fingerprint.go`.
//!
//! The Go proxy used `utls` to *forge* a ClientHello mimicking reqwest/rustls —
//! the TLS stack of the real Codex client — because Go's stdlib TLS has a
//! different JA3. This gateway already *is* reqwest/rustls, so instead of forging
//! anything we pin the rustls `ClientConfig` to a fixed cipher-suite order, TLS
//! 1.2/1.3, and ALPN that match the Go `rustlsSpec()`. That keeps the handshake
//! fingerprint stable even if a future rustls bump changes the default suite set
//! or ordering. Wired into the Codex HTTP client only — the Go transport was a
//! hybrid that forged TLS for `chatgpt.com` / `auth.openai.com` and left Anthropic
//! on the standard stack.
use std::sync::Arc;

use rustls::crypto::ring::cipher_suite as cs;
use rustls::crypto::ring::default_provider;
use rustls::crypto::CryptoProvider;
use rustls::version::{TLS12, TLS13};
use rustls::{ClientConfig, RootCertStore, SupportedCipherSuite};

/// Cipher suites in the exact order of Go `rustlsSpec()`: the three TLS 1.3
/// AEADs first, then ECDHE-ECDSA, then ECDHE-RSA. rustls emits TLS 1.2 renegotiation
/// signaling itself, so the Go `FAKE_TLS_EMPTY_RENEGOTIATION_INFO_SCSV` pseudo-cipher
/// has no analog here.
fn ordered_cipher_suites() -> Vec<SupportedCipherSuite> {
    vec![
        cs::TLS13_AES_256_GCM_SHA384,
        cs::TLS13_AES_128_GCM_SHA256,
        cs::TLS13_CHACHA20_POLY1305_SHA256,
        cs::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        cs::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        cs::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        cs::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        cs::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        cs::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ]
}

/// Build the pinned rustls `ClientConfig` for the Codex upstream. Certificate
/// verification stays on (Go used `InsecureSkipVerify: false`); roots come from
/// the compiled-in Mozilla set (`webpki-roots`). ALPN advertises `h2` then
/// `http/1.1` — reqwest/rustls' authentic order (the Go file's top note flags
/// h2-first as the real reqwest fingerprint, vs. the old proxy that wrongly forced
/// http/1.1).
pub(crate) fn codex_client_config() -> ClientConfig {
    let base = default_provider();
    let provider = CryptoProvider {
        cipher_suites: ordered_cipher_suites(),
        ..base
    };
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&TLS13, &TLS12])
        .expect("codex rustls config: protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_pins_nine_suites_in_order() {
        let suites = ordered_cipher_suites();
        assert_eq!(suites.len(), 9);
        // TLS 1.3 AEADs lead, matching the Go spec ordering.
        assert_eq!(
            suites[0].suite(),
            rustls::CipherSuite::TLS13_AES_256_GCM_SHA384
        );
    }

    #[test]
    fn config_builds_with_alpn() {
        let cfg = codex_client_config();
        assert_eq!(cfg.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }
}
