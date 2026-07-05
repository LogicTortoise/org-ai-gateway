//! Provider priority chains: a per-protocol, ordered list of providers the
//! gateway tries for a request, with a per-chain mode (failover or round-robin).
//!
//! Routing in this gateway is keyed by the INBOUND client protocol, which we
//! call a "slot":
//!   * `Codex`  — OpenAI / Responses traffic (`/v1/responses`, `/v1/chat/completions`).
//!   * `Claude` — Anthropic traffic (`/v1/messages`).
//!
//! Each slot has a `ChainCfg { mode, providers }`. The executor in
//! `routes::proxy` walks the providers in order (round-robin rotates the start
//! offset) and serves the request with the first provider whose pool can handle
//! it, degrading to the next on exhaustion / transient failure. Both GLM and
//! ollama can appear in BOTH slots; native `codex`/`claude` only in their own.
use crate::prelude::*;

/// The inbound-protocol slot a request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainSlot {
    Codex,
    Claude,
}

impl ChainSlot {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChainSlot::Codex => "codex",
            ChainSlot::Claude => "claude",
        }
    }

    /// The native provider for this slot (always a legal chain member, and the
    /// default single-element chain).
    pub(crate) fn native_provider(self) -> &'static str {
        match self {
            ChainSlot::Codex => "codex",
            ChainSlot::Claude => "claude",
        }
    }

    /// Providers that can legally serve this slot. GLM and ollama serve both
    /// (via the Anthropic passthrough / OpenAI adapter as appropriate); cursor
    /// rides the format adapter; the native provider only serves its own slot.
    pub(crate) fn allowed_providers(self) -> &'static [&'static str] {
        match self {
            ChainSlot::Codex => &["codex", "glm", "ollama", "cursor"],
            ChainSlot::Claude => &["claude", "glm", "ollama", "cursor"],
        }
    }
}

/// How a chain consumes its providers across requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChainMode {
    /// Always start at the first provider; advance only on failure (依次降级).
    Failover,
    /// Rotate the starting provider each request to spread load (循环使用),
    /// then degrade through the rest on failure.
    RoundRobin,
}

impl Default for ChainMode {
    fn default() -> Self {
        ChainMode::Failover
    }
}

/// One slot's configured chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChainCfg {
    #[serde(default)]
    pub(crate) mode: ChainMode,
    #[serde(default)]
    pub(crate) providers: Vec<String>,
}

/// The whole gateway's provider-priority configuration (global; one per slot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProviderChains {
    pub(crate) codex: ChainCfg,
    pub(crate) claude: ChainCfg,
}

impl Default for ProviderChains {
    fn default() -> Self {
        ProviderChains {
            codex: ChainCfg { mode: ChainMode::Failover, providers: vec!["codex".to_string()] },
            claude: ChainCfg { mode: ChainMode::Failover, providers: vec!["claude".to_string()] },
        }
    }
}

impl ProviderChains {
    pub(crate) fn for_slot(&self, slot: ChainSlot) -> &ChainCfg {
        match slot {
            ChainSlot::Codex => &self.codex,
            ChainSlot::Claude => &self.claude,
        }
    }

    fn set_slot(&mut self, slot: ChainSlot, cfg: ChainCfg) {
        match slot {
            ChainSlot::Codex => self.codex = cfg,
            ChainSlot::Claude => self.claude = cfg,
        }
    }

    /// Apply an incoming chain config for one slot after validation: keep only
    /// providers legal for the slot, de-duplicate, and guarantee a non-empty
    /// chain by falling back to the native provider.
    pub(crate) fn apply_validated(&mut self, slot: ChainSlot, mode: ChainMode, providers: &[String]) {
        let allowed = slot.allowed_providers();
        let mut seen = std::collections::HashSet::new();
        let mut clean: Vec<String> = Vec::new();
        for p in providers {
            let p = p.trim().to_ascii_lowercase();
            if allowed.contains(&p.as_str()) && seen.insert(p.clone()) {
                clean.push(p);
            }
        }
        if clean.is_empty() {
            clean.push(slot.native_provider().to_string());
        }
        self.set_slot(slot, ChainCfg { mode, providers: clean });
    }
}

/// Produce the ordered list of providers to attempt for a request, applying the
/// chain mode. `rr_offset` is ignored in failover mode; in round-robin it
/// rotates the starting index so successive requests spread across providers.
pub(crate) fn ordered_attempts(cfg: &ChainCfg, rr_offset: usize) -> Vec<String> {
    let n = cfg.providers.len();
    if n <= 1 {
        return cfg.providers.clone();
    }
    match cfg.mode {
        ChainMode::Failover => cfg.providers.clone(),
        ChainMode::RoundRobin => {
            let start = rr_offset % n;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(cfg.providers[(start + i) % n].clone());
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence (single JSON document; mirrors the account-snapshot durability)
// ---------------------------------------------------------------------------

/// Load the provider-chains config from disk, falling back to defaults when the
/// file is absent or unreadable/corrupt.
pub(crate) async fn load_chains(path: &std::path::Path) -> ProviderChains {
    match tokio::fs::read_to_string(path).await {
        Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
            warn!("provider_chains.json unreadable ({}); using defaults", e);
            ProviderChains::default()
        }),
        Err(_) => ProviderChains::default(),
    }
}

/// Atomically persist the current in-memory chains to disk (temp file → fsync →
/// rename → fsync dir), serialized behind the shared `persist_lock`.
pub(crate) async fn persist_chains(state: &AppState) -> Result<(), String> {
    let _guard = state.persist_lock.lock().await;
    let chains = state.chains.read().await.clone();
    let json = serde_json::to_string_pretty(&chains).map_err(|e| e.to_string())?;
    let tmp = state.chain_file.with_extension("json.tmp");
    {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&tmp, &state.chain_file).await.map_err(|e| e.to_string())?;
    crate::pool::storage::sync_parent_dir(&state.chain_file).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_keeps_order_roundrobin_rotates() {
        let cfg = ChainCfg {
            mode: ChainMode::Failover,
            providers: vec!["codex".into(), "glm".into(), "ollama".into()],
        };
        assert_eq!(ordered_attempts(&cfg, 5), vec!["codex", "glm", "ollama"]);

        let rr = ChainCfg { mode: ChainMode::RoundRobin, ..cfg.clone() };
        assert_eq!(ordered_attempts(&rr, 0), vec!["codex", "glm", "ollama"]);
        assert_eq!(ordered_attempts(&rr, 1), vec!["glm", "ollama", "codex"]);
        assert_eq!(ordered_attempts(&rr, 2), vec!["ollama", "codex", "glm"]);
        assert_eq!(ordered_attempts(&rr, 3), vec!["codex", "glm", "ollama"]);
    }

    #[test]
    fn validation_filters_and_dedups() {
        let mut pc = ProviderChains::default();
        pc.apply_validated(
            ChainSlot::Claude,
            ChainMode::RoundRobin,
            &["claude".into(), "CLAUDE".into(), "codex".into(), "glm".into(), "bogus".into()],
        );
        // codex is illegal in the Claude slot; CLAUDE dedups; bogus dropped.
        assert_eq!(pc.claude.providers, vec!["claude", "glm"]);
        assert_eq!(pc.claude.mode, ChainMode::RoundRobin);
    }

    #[test]
    fn empty_falls_back_to_native() {
        let mut pc = ProviderChains::default();
        pc.apply_validated(ChainSlot::Codex, ChainMode::Failover, &["bogus".into()]);
        assert_eq!(pc.codex.providers, vec!["codex"]);
    }
}
