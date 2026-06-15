//! Codex Desktop HTTP fingerprint headers (ported from `codex_fingerprint.go`).
//! The appcast auto-updater (which periodically bumps the version from
//! persistent.oaistatic.com) is not ported; the constants below match the Go
//! defaults and can be overridden via env vars.
use crate::prelude::*;

const ORIGINATOR: &str = "Codex Desktop";
const APP_VERSION: &str = "26.318.11754";
const CHROMIUM_VERSION: &str = "144";
const RESIDENCY: &str = "us";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Codex "platform" token (`darwin` / `win32` / `linux`).
fn platform() -> String {
    let default = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "linux"
    };
    env_or("CODEX_PLATFORM", default)
}

/// Codex "arch" token (`x64` / `arm64`).
fn arch() -> String {
    let default = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    env_or("CODEX_ARCH", default)
}

/// sec-ch-ua-platform value derived from the platform token.
fn ch_ua_platform(platform: &str) -> &'static str {
    match platform {
        "darwin" => "macOS",
        "win32" => "Windows",
        _ => "Linux",
    }
}

/// The Codex Desktop User-Agent. Shared by the HTTP fingerprint and the
/// realtime WebSocket handshake (`routes::websocket`) so the two identities
/// can't drift when the version constant is bumped.
pub(crate) fn codex_user_agent() -> String {
    format!(
        "{}/{} ({}; {})",
        env_or("CODEX_ORIGINATOR", ORIGINATOR),
        env_or("CODEX_APP_VERSION", APP_VERSION),
        platform(),
        arch()
    )
}

/// The Codex `originator` header value (env-overridable, same as the UA).
pub(crate) fn codex_originator() -> String {
    env_or("CODEX_ORIGINATOR", ORIGINATOR)
}

/// Apply the Codex Desktop request fingerprint headers.
pub(crate) fn apply_codex_fingerprint(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let chromium = env_or("CODEX_CHROMIUM_VERSION", CHROMIUM_VERSION);
    let platform = platform();
    req.header("originator", codex_originator())
        .header("x-openai-internal-codex-residency", RESIDENCY)
        .header("x-client-request-id", Uuid::new_v4().to_string())
        .header("OpenAI-Beta", "responses_websockets=2026-02-06")
        .header("User-Agent", codex_user_agent())
        .header(
            "sec-ch-ua",
            format!("\"Chromium\";v=\"{}\", \"Not:A-Brand\";v=\"24\"", chromium),
        )
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", format!("\"{}\"", ch_ua_platform(&platform)))
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
}
