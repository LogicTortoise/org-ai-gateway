//! Trae provider (ByteDance Trae IDE models). An API-key endpoint provider — no
//! OAuth, no token refresh — reached through a **local `trae2anthropic` sidecar**
//! that speaks the Anthropic Messages API.
//!
//! ## Why a sidecar and not a native upstream
//!
//! Trae has no public chat API. Its IDE talks to `api/agent/v3` — a proprietary
//! *agent* protocol whose request bodies are AES-256-GCM encrypted with a key
//! reverse-engineered out of `ai_agent.dll` (`body = base64(nonce || AES-GCM(...))`,
//! key = a fixed BASE_KEY whose first 8 bytes are XORed with a per-request salt
//! echoed in `x-request-pin`). On top of that sit MCP tool injection, server-side
//! conversation history addressed by `history_id`, and a VeImageX upload pipeline
//! for images. That protocol churns whenever ByteDance ships an IDE build, and
//! the BASE_KEY can rotate.
//!
//! So the gateway does NOT implement it. A separate `trae2anthropic` process owns
//! the reverse-engineered protocol and exposes a plain Anthropic-compatible
//! `/v1/messages`; this provider is a thin client of that. Protocol drift becomes
//! "update the sidecar", not "re-port a crypto stack into Rust".
//!
//! ## Shape
//!
//! Unlike GLM / Kimi (which ride BOTH an Anthropic-compatible and an
//! OpenAI-compatible endpoint), the sidecar exposes `/v1/messages` and
//! `/v1/models` only. **Trae therefore serves the Claude slot exclusively** —
//! there is no OpenAI adapter path, and `ChainSlot::Codex` does not list it. On
//! the Claude path the payload is buffered and returned verbatim, so tool calls
//! survive; no Claude Code fingerprint is injected (Trae is not Anthropic).
//!
//! An "account" carries:
//!   * `base_url` — the sidecar root; defaults to `TRAE_BASE_URL` env, else
//!     `http://127.0.0.1:8788`. `/v1/messages` and `/v1/models` are appended.
//!   * `api_key` / `access_token` — **optional**. The sidecar only requires a key
//!     when one has been generated in its admin panel; with none configured its
//!     API is open, so an empty key is a legitimate local configuration.
//!
//! Trae accounts (the actual Trae logins, their quotas and rotation) are managed
//! inside the sidecar's own admin panel — this provider sees one logical upstream.
//!
//! Token counts are REAL: the sidecar passes Trae's `token_usage` through as
//! Anthropic-shaped `usage`, including `cache_read_input_tokens` /
//! `cache_creation_input_tokens` — see `usage::tokens::parse_usage("trae", ...)`.
use crate::prelude::*;
use crate::util::truncate_text;

/// Built-in default upstream model, used when neither the runtime override nor
/// `TRAE_DEFAULT_MODEL` supplies one. Matches the sidecar's own `default_model`,
/// and is what a `claude-*` request degraded onto the Trae fallback ends up
/// running.
pub(crate) const BUILTIN_DEFAULT_MODEL: &str = "minimax-m3";
pub(crate) const BUILTIN_OPUS_MODEL: &str = "minimax-m3";
pub(crate) const BUILTIN_SONNET_MODEL: &str = "minimax-m3";
pub(crate) const BUILTIN_FABLE_MODEL: &str = "minimax-m3";

/// STATIC FALLBACK model list, used only when the live `/v1/models` fetch fails
/// (e.g. the sidecar isn't running yet). Mirrors the sidecar's `models.json` and
/// can lag behind it — `fetch_trae_models` prefers the live list, and an
/// override (runtime edit or `TRAE_MODELS`) pins the list outright. Any model
/// the sidecar knows also works directly via `trae/<id>` regardless of this list.
pub(crate) const BUILTIN_MODELS: &[&str] = &[
    "minimax-m3",
    "minimax-m2.7",
    "gemini-3.1-pro",
    "gemini-3-flash",
    "gemini-2.5-flash",
    "gpt-5.4",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.1",
    "gpt-5",
    "kimi-k2.5",
    "deepseek-v3.2",
    "doubao-seed-2.0-code",
    "dola-seed-2.0-code",
];

/// This provider's entry in the runtime model-config table.
fn spec() -> &'static crate::provider::model_config::ProviderModelSpec {
    crate::provider::model_config::spec("trae").expect("trae model spec")
}

/// Built-in sidecar endpoint — `trae2anthropic`'s default listen address. Used
/// when neither the account nor `TRAE_BASE_URL` supplies one, so connecting a
/// Trae account on the same host needs no configuration at all.
const DEFAULT_BASE: &str = "http://127.0.0.1:8788";

/// Dedicated HTTP client for Trae. Short connect timeout (fail fast when the
/// sidecar is down — this is a fallback path) and a generous total timeout: the
/// sidecar drives a full agent run upstream, which is slower than a chat API.
///
/// `no_proxy()` is load-bearing, not hygiene: reqwest honors `HTTP_PROXY` /
/// `HTTPS_PROXY` from the environment, and a gateway launched from a shell with a
/// system proxy (Surge / Clash / corporate MITM) would route even
/// `http://127.0.0.1:8788` through it — the sidecar request then comes back as
/// the proxy's own HTML error page (a 503 that looks like the sidecar failing)
/// instead of ever reaching the sidecar. Every Trae upstream is a process the
/// operator runs themselves (loopback by default, a LAN host at most), so there
/// is no case where proxying it is wanted.
pub(crate) fn trae_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("TRAE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(600);
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building trae http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Whether a model name selects the Trae upstream. **Only the explicit `trae` /
/// `trae/<model>` / `trae-<model>` forms are accepted** — deliberately narrower
/// than the GLM / Kimi detectors.
///
/// Trae resells other vendors' models under their own names (`gpt-5`,
/// `gemini-3.1-pro`, `kimi-k2.5`, `deepseek-v3.2`, ...). Matching those bare ids
/// here would silently hijack Codex / Kimi routing for anyone naming a real
/// upstream model, so reaching Trae always requires saying `trae`.
pub(crate) fn is_trae_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "trae" || m.starts_with("trae/") || m.starts_with("trae-")
}

/// Maps a gateway model name to the upstream model id the sidecar expects.
/// `trae/gpt-5.4` -> `gpt-5.4`; `trae-gpt-5.4` -> `gpt-5.4`; a bare `trae` ->
/// the configured default. Claude Code traffic arrives with `claude-*` names,
/// which are rewritten to one of the four slots via the standard tier rewrite:
/// opus → opus slot, sonnet (with haiku folded in) → sonnet slot, fable →
/// fable slot, unrecognised → default slot.
pub(crate) fn trae_canonical_model(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_ascii_lowercase();
    if lower.starts_with("trae/") {
        let rest = m["trae/".len()..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    if lower.starts_with("trae-") {
        let rest = m["trae-".len()..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    if lower == "trae" {
        return trae_default_model();
    }
    // Claude Code's tier rewrite — opus / sonnet (with haiku folded in) /
    // fable each map to their own slot. `contains("haiku")` and
    // `contains("sonnet")` both reach the sonnet slot because haiku ids
    // don't contain the literal "sonnet" substring.
    if lower.contains("opus") {
        return trae_opus_model();
    }
    if lower.contains("haiku") || lower.contains("sonnet") {
        return trae_sonnet_model();
    }
    if lower.contains("fable") {
        return trae_fable_model();
    }
    trae_default_model()
}

/// The configured default upstream model (`TRAE_DEFAULT_MODEL`, else the
/// built-in), used for a bare `trae` and for any foreign model name degraded
/// onto this provider.
fn trae_default_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Default)
}

fn trae_opus_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Opus)
}

fn trae_sonnet_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Sonnet)
}

fn trae_fable_model() -> String {
    spec().resolve(crate::provider::model_config::Slot::Fable)
}

/// The sidecar base prefix for a Trae account: its stored `base_url`, else the
/// `TRAE_BASE_URL` env, else the built-in loopback default. Trailing slash
/// trimmed.
pub(crate) fn trae_base(account: &UpstreamAccount) -> String {
    let raw = if !account.base_url.trim().is_empty() {
        account.base_url.trim().to_string()
    } else {
        std::env::var("TRAE_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
    };
    raw.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// Anthropic-compatible upstream call (passthrough, the only serving path)
// ---------------------------------------------------------------------------

/// Attach the sidecar's optional API key. `trae2anthropic` accepts either
/// `x-api-key` or `Authorization: Bearer`, and leaves its API open when no keys
/// have been generated — so an empty credential is valid and we send no header
/// at all rather than an empty bearer (which some proxies reject outright).
fn with_optional_auth(req: reqwest::RequestBuilder, account: &UpstreamAccount) -> reqwest::RequestBuilder {
    let key = account.bearer();
    if key.is_empty() {
        req
    } else {
        req.header("x-api-key", key).bearer_auth(key)
    }
}

/// Send an Anthropic-shaped payload to the sidecar's `/v1/messages` and return
/// the upstream response for the caller to buffer.
///
/// The payload is forwarded as-is except for `model`: the sidecar resolves ids
/// against its own catalog, so a foreign name (typically `claude-*`, since this
/// provider exists as a Claude fallback) is rewritten to the canonical Trae model
/// first. No fingerprint injection — Trae is not Anthropic, so the Claude Code
/// system blocks / tool obfuscation must NOT be applied.
pub(crate) async fn send_trae_anthropic(
    account: &UpstreamAccount,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let base = trae_base(account);
    if base.is_empty() {
        return Err("trae account has no sidecar base_url".to_string());
    }
    let mut body = payload.clone();
    let requested = body.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let upstream_model = trae_canonical_model(&requested);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(upstream_model));
    }

    let url = format!("{}/v1/messages", base);
    let req = trae_http_client()
        .post(&url)
        .header("anthropic-version", crate::fingerprint::claude::CC_ANTHROPIC_VERSION)
        .header("Accept", "text/event-stream, application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(&body);
    with_optional_auth(req, account)
        .send()
        .await
        .map_err(|e| {
            format!(
                "failed to call trae sidecar ({}): {} — 确认 trae2anthropic 已启动（默认 {}）",
                url, e, DEFAULT_BASE
            )
        })
}

// ---------------------------------------------------------------------------
// Model listing + reachability probe
// ---------------------------------------------------------------------------

/// Build the gateway-facing model list from a set of upstream model ids: a bare
/// `trae` default entry first, then each id as `trae/<id>` (the prefix is
/// stripped before the upstream call).
fn models_from_ids(ids: impl IntoIterator<Item = String>) -> Vec<ModelInfo> {
    let mut out = vec![ModelInfo {
        slug: "trae".to_string(),
        display_name: "trae (default)".to_string(),
    }];
    for id in ids {
        let id = id.trim().to_string();
        if !id.is_empty() {
            out.push(ModelInfo { slug: format!("trae/{}", id), display_name: id });
        }
    }
    out
}

/// The STATIC fallback model catalog: runtime override, else `TRAE_MODELS`,
/// else the built-in list. Used when no live list is available.
pub(crate) fn trae_model_catalog() -> Vec<ModelInfo> {
    models_from_ids(spec().catalog())
}

/// Fetch the LIVE model list from the sidecar's `GET {base}/v1/models` — the
/// authoritative catalog, since the sidecar owns the id → real Trae config-name
/// mapping. Errors (sidecar down, bad key) bubble up so the caller can fall back
/// to the static catalog. A pinned catalog (runtime override or `TRAE_MODELS`)
/// short-circuits the network call — otherwise the live list would immediately
/// overwrite it.
pub(crate) async fn fetch_trae_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    if spec().catalog_pinned() {
        return Ok(trae_model_catalog());
    }
    let base = trae_base(account);
    if base.is_empty() {
        return Err("trae account has no sidecar base_url".to_string());
    }
    let url = format!("{}/v1/models", base);
    let resp = with_optional_auth(trae_http_client().get(&url), account)
        .send()
        .await
        .map_err(|e| format!("failed to reach trae sidecar models api ({}): {}", url, e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("reading trae models body failed: {}", e))?;
    if !status.is_success() {
        return Err(format!("trae models api error {}: {}", status.as_u16(), truncate_text(&body, 200)));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid trae models response: {}", e))?;
    // Anthropic shape: {"data":[{"id":"minimax-m3","type":"model",...}, ...]}.
    let arr = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "trae models response missing `data` array".to_string())?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    if ids.is_empty() {
        return Err("trae models response had no model ids".to_string());
    }
    Ok(models_from_ids(ids))
}

/// Probe reachability of a Trae account at connect time. Unlike the GLM / Kimi
/// probes (which must spend a tiny completion), the sidecar's `/v1/models`
/// validates both liveness and the optional API key without burning any upstream
/// Trae quota — so this probe is free.
pub(crate) async fn probe_trae(account: &UpstreamAccount) -> Result<(), String> {
    let base = trae_base(account);
    if base.is_empty() {
        return Err("Trae 缺少 sidecar base_url".to_string());
    }
    let url = format!("{}/v1/models", base);
    let resp = with_optional_auth(trae_http_client().get(&url), account)
        .send()
        .await
        .map_err(|e| {
            format!(
                "无法连接 Trae sidecar ({}): {} — 请先启动 trae2anthropic（默认监听 {}）",
                url, e, DEFAULT_BASE
            )
        })?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "Trae sidecar 鉴权失败 ({}): {} — 请填写在 sidecar 管理面板生成的 API Key",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    if !status.is_success() {
        return Err(format!(
            "Trae sidecar 返回 {}: {}",
            status.as_u16(),
            truncate_text(&body, 200)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_trae_names_route_here() {
        assert!(is_trae_model("trae"));
        assert!(is_trae_model("TRAE"));
        assert!(is_trae_model("trae/gpt-5.4"));
        assert!(is_trae_model("trae-minimax-m3"));
        // Trae resells these under the vendors' own names; bare ids must NOT be
        // captured or they would hijack Codex / Kimi / GLM routing.
        assert!(!is_trae_model("gpt-5"));
        assert!(!is_trae_model("gpt-5.2-codex"));
        assert!(!is_trae_model("kimi-k2.5"));
        assert!(!is_trae_model("deepseek-v3.2"));
        assert!(!is_trae_model("gemini-3.1-pro"));
        assert!(!is_trae_model("claude-sonnet-4-5"));
        assert!(!is_trae_model("glm-4.6"));
    }

    #[test]
    fn canonicalization_strips_prefix_and_defaults_foreign_names() {
        std::env::remove_var("TRAE_DEFAULT_MODEL");
        assert_eq!(trae_canonical_model("trae/gpt-5.4"), "gpt-5.4");
        assert_eq!(trae_canonical_model("trae-kimi-k2.5"), "kimi-k2.5");
        assert_eq!(trae_canonical_model("trae"), BUILTIN_DEFAULT_MODEL);
        // A Claude name degraded onto this provider must become a real Trae id.
        assert_eq!(trae_canonical_model("claude-sonnet-4-5"), BUILTIN_DEFAULT_MODEL);
        assert_eq!(trae_canonical_model(""), BUILTIN_DEFAULT_MODEL);
    }

    #[test]
    fn catalog_has_default_first() {
        std::env::remove_var("TRAE_MODELS");
        let cat = trae_model_catalog();
        assert_eq!(cat[0].slug, "trae");
        assert!(cat.iter().any(|m| m.slug == "trae/minimax-m3"));
        assert!(cat.iter().any(|m| m.slug == "trae/gemini-3.1-pro"));
    }

    fn account_with(base: &str) -> UpstreamAccount {
        UpstreamAccount {
            id: "t1".into(),
            owner_user_id: "alice".into(),
            provider: "trae".into(),
            account_label: "sidecar".into(),
            access_token: String::new(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            api_key: String::new(),
            base_url: base.into(),
            base_url_alt: String::new(),
            share_enabled: true,
            share_limit_percent: None,
            daily_token_limit: None,
            created_at: Utc::now(),
            runtime: AccountRuntime::default(),
        }
    }

    #[test]
    fn base_defaults_to_loopback_sidecar() {
        std::env::remove_var("TRAE_BASE_URL");
        assert_eq!(trae_base(&account_with("")), DEFAULT_BASE);
        // An explicit base wins, and the trailing slash is normalized away so
        // `format!("{}/v1/messages", base)` never produces a double slash.
        assert_eq!(
            trae_base(&account_with("http://127.0.0.1:9999/")),
            "http://127.0.0.1:9999"
        );
    }
}
