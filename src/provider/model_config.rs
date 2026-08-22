//! Runtime-editable per-provider model configuration.
//!
//! ## Why this exists
//!
//! Every provider rewrites the model name a client asked for into an id its own
//! upstream understands (`provider::normalize_model_for_provider`). Those
//! rewrite targets used to come only from env vars read through
//! `std::env::var` on each call — which in practice meant "edit the launch
//! script and restart the gateway", because nothing in the process can change
//! the environment of an already-running server. When an upstream ships a new
//! model (DeepSeek v5, MiniMax M4, …) that restart is the whole cost of the
//! change, and it drops every in-flight request.
//!
//! So the same values now live in `data/provider_models.json`, editable at
//! runtime through `/v1/provider/model-map` and applied to the very next
//! request. Resolution order for every value is:
//!
//!   **stored override  >  env var  >  built-in constant**
//!
//! The env vars keep working untouched as the deploy-time default; the stored
//! file is the operator's live override on top. `Source` reports which one won
//! so the UI can say so instead of leaving the operator guessing why their env
//! var appears to be ignored.
//!
//! ## What is editable
//!
//! Two kinds of value, described per provider by `ProviderModelSpec`:
//!
//!   * **slots** — the rewrite targets. Three Claude Code tiers per provider
//!     (`opus` / `sonnet` / `fable`; `claude-haiku-*` is folded into the Sonnet
//!     slot because most third-party upstreams only have one mid-tier), plus a
//!     per-provider `default` slot for the bare slug and any unrecognised
//!     name. The default slot is **not** a Claude tier — it lives on the same
//!     row for convenience, and each provider declares its own set of four.
//!   * **catalog** — the id list offered in the model pickers. For providers
//!     with a live `GET /models` (glm / kimi / trae / ollama) an override also
//!     PINS the list, short-circuiting the network fetch exactly the way the
//!     env var already did.
//!
//! The matching *rules* (which client names count as haiku-tier, how `foo/bar`
//! is split) stay in Rust: they are protocol facts about each upstream, not
//! operator preferences, and a mis-edited regex would silently misroute
//! traffic.
use crate::prelude::*;
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

// ---------------------------------------------------------------------------
// Stored document
// ---------------------------------------------------------------------------

/// One provider's stored overrides. Every field is optional-by-emptiness: an
/// empty string / empty vec means "not overridden, fall through to env", which
/// is also what the UI sends when the operator clears a box.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProviderModelCfg {
    /// Generic fallback for **the bare provider slug** (e.g. `model: "kimi"`)
    /// and any foreign model name that doesn't match a Claude tier below.
    /// Not part of the 3 Claude tiers — providers that don't recognise a name
    /// route it here. Lives alongside the tiers on its own.
    #[serde(default)]
    pub(crate) default_model: String,
    /// Upstream target for `claude-opus-*` traffic (Claude Code's strongest tier).
    #[serde(default)]
    pub(crate) opus_model: String,
    /// Upstream target for `claude-sonnet-*` AND `claude-haiku-*` traffic —
    /// the two share one upstream slot because most third-party providers only
    /// expose a mid-tier (no separate Sonnet variant). Aliased to the old
    /// `haiku_model` / `sonnet_haiku_model` field names so any pre-existing
    /// `data/provider_models.json` still loads.
    #[serde(default, alias = "haiku_model", alias = "sonnet_haiku_model")]
    pub(crate) sonnet_model: String,
    /// Upstream target for `claude-fable-*` traffic (Claude Code's cheapest tier).
    #[serde(default)]
    pub(crate) fable_model: String,
    /// Wholesale catalog override. Non-empty also pins live-fetched catalogs.
    #[serde(default)]
    pub(crate) models: Vec<String>,
    /// Default reasoning-effort injected when the client sends none. Empty =
    /// fall through to env, then the provider's built-in (its strongest tier).
    #[serde(default)]
    pub(crate) default_effort: String,
    /// Upstream reasoning-effort target for the client's `low` tier.
    #[serde(default)]
    pub(crate) effort_low: String,
    /// Upstream reasoning-effort target for the client's `medium` tier.
    #[serde(default)]
    pub(crate) effort_medium: String,
    /// Upstream reasoning-effort target for the client's `high` tier.
    #[serde(default)]
    pub(crate) effort_high: String,
    /// Upstream reasoning-effort target for the client's `xhigh` tier.
    #[serde(default)]
    pub(crate) effort_xhigh: String,
}

impl ProviderModelCfg {
    /// Whether this config overrides nothing, in which case it is dropped
    /// rather than persisted as an empty object.
    pub(crate) fn is_empty(&self) -> bool {
        self.default_model.trim().is_empty()
            && self.opus_model.trim().is_empty()
            && self.sonnet_model.trim().is_empty()
            && self.fable_model.trim().is_empty()
            && self.models.is_empty()
            && self.default_effort.trim().is_empty()
            && self.effort_low.trim().is_empty()
            && self.effort_medium.trim().is_empty()
            && self.effort_high.trim().is_empty()
            && self.effort_xhigh.trim().is_empty()
    }

    /// Trim every field and drop blank catalog entries, so a value typed with
    /// stray whitespace can't produce a model id the upstream will reject.
    pub(crate) fn normalized(&self) -> Self {
        Self {
            default_model: self.default_model.trim().to_string(),
            opus_model: self.opus_model.trim().to_string(),
            sonnet_model: self.sonnet_model.trim().to_string(),
            fable_model: self.fable_model.trim().to_string(),
            models: self
                .models
                .iter()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect(),
            default_effort: self.default_effort.trim().to_string(),
            effort_low: self.effort_low.trim().to_string(),
            effort_medium: self.effort_medium.trim().to_string(),
            effort_high: self.effort_high.trim().to_string(),
            effort_xhigh: self.effort_xhigh.trim().to_string(),
        }
    }

    pub(crate) fn slot_value(&self, slot: Slot) -> &str {
        match slot {
            Slot::Default => self.default_model.trim(),
            Slot::Opus => self.opus_model.trim(),
            Slot::Sonnet => self.sonnet_model.trim(),
            Slot::Fable => self.fable_model.trim(),
        }
    }

    pub(crate) fn effort_value(&self, level: EffortLevel) -> &str {
        match level {
            EffortLevel::Default => self.default_effort.trim(),
            EffortLevel::Low => self.effort_low.trim(),
            EffortLevel::Medium => self.effort_medium.trim(),
            EffortLevel::High => self.effort_high.trim(),
            EffortLevel::Xhigh => self.effort_xhigh.trim(),
        }
    }
}

/// The whole persisted document: provider name → overrides. A `BTreeMap` so the
/// file has a stable key order across writes and diffs cleanly.
pub(crate) type ModelOverrides = BTreeMap<String, ProviderModelCfg>;

// ---------------------------------------------------------------------------
// Process-global store
// ---------------------------------------------------------------------------

/// Held in a plain `std::sync::RwLock` rather than the tokio one because every
/// reader is a synchronous `*_canonical_model` call deep inside request
/// handling. Reads are uncontended map lookups; the only writer is the
/// `PUT /v1/provider/model-map` handler.
fn store() -> &'static RwLock<ModelOverrides> {
    static STORE: OnceLock<RwLock<ModelOverrides>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(ModelOverrides::new()))
}

fn with_cfg<T>(provider: &str, f: impl FnOnce(&ProviderModelCfg) -> T) -> Option<T> {
    let guard = store().read().ok()?;
    guard.get(provider).map(f)
}

/// A copy of the current overrides, for serving `GET` and for persisting.
pub(crate) fn snapshot() -> ModelOverrides {
    store().read().map(|g| g.clone()).unwrap_or_default()
}

/// Replace the whole in-memory document. Entries are normalized, and ones that
/// override nothing are dropped so the file doesn't accumulate empty objects.
/// Takes effect on the next request — nothing caches a resolved model id.
pub(crate) fn replace(next: ModelOverrides) {
    let cleaned: ModelOverrides = next
        .into_iter()
        .filter_map(|(provider, cfg)| {
            let cfg = cfg.normalized();
            if cfg.is_empty() {
                None
            } else {
                Some((provider.trim().to_ascii_lowercase(), cfg))
            }
        })
        .collect();
    if let Ok(mut guard) = store().write() {
        *guard = cleaned;
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Which rewrite target a value configures.
///
/// The mapping is keyed by Claude Code's three rewrite-relevant tiers. Fable
/// is kept on its own slot because it maps to a distinctly cheaper upstream
/// model; Sonnet absorbs Haiku (`claude-haiku-*` lands in the Sonnet slot too)
/// because most third-party providers only expose a mid-tier and don't
/// distinguish a separate Sonnet. `Default` is **not** a Claude tier — it is
/// the per-provider fallback used for the bare provider slug and any foreign
/// model name that doesn't match a tier; it lives on the same UI row for
/// convenience but is independent of the tier system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Slot {
    /// Per-provider fallback for the bare slug and unrecognised names.
    /// Not a Claude tier; intentionally grouped with the tiers on the UI.
    Default,
    /// `claude-opus-*` traffic.
    Opus,
    /// `claude-sonnet-*` AND `claude-haiku-*` traffic (shared slot).
    Sonnet,
    /// `claude-fable-*` traffic (Claude Code's cheapest tier).
    Fable,
}

impl Slot {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Slot::Default => "default",
            Slot::Opus => "opus",
            Slot::Sonnet => "sonnet",
            Slot::Fable => "fable",
        }
    }
}

/// The client-side reasoning-effort tier an upstream effort value maps from.
/// `Default` is not a tier — it is the per-provider fallback injected when the
/// client sends no effort (or an unrecognised one). The four real tiers mirror
/// Claude Code's effort vocabulary; Codex's `minimal` folds into `Low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffortLevel {
    /// Injected fallback when no effort is present.
    Default,
    /// Client `low` (and Codex `minimal`).
    Low,
    /// Client `medium`.
    Medium,
    /// Client `high`.
    High,
    /// Client `xhigh`.
    Xhigh,
}

impl EffortLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Default => "default",
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
        }
    }
}

/// Where a resolved value came from, so the UI can show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// From `data/provider_models.json` — an operator edit.
    Override,
    /// From the provider's env var.
    Env,
    /// The compiled-in constant.
    Builtin,
    /// Fetched live from the upstream's `/models` (catalogs only).
    Live,
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve one rewrite target: stored override, else env, else the built-in.
pub(crate) fn resolve_slot(provider: &str, slot: Slot, env_key: &str, builtin: &str) -> String {
    resolve_slot_sourced(provider, slot, env_key, builtin).0
}

/// As `resolve_slot`, also reporting which layer supplied the value.
pub(crate) fn resolve_slot_sourced(
    provider: &str,
    slot: Slot,
    env_key: &str,
    builtin: &str,
) -> (String, Source) {
    let stored = with_cfg(provider, |cfg| cfg.slot_value(slot).to_string())
        .filter(|v| !v.is_empty());
    if let Some(v) = stored {
        return (v, Source::Override);
    }
    match env_value(env_key) {
        Some(v) => (v, Source::Env),
        None => (builtin.to_string(), Source::Builtin),
    }
}

/// Resolve one reasoning-effort target, mirroring `resolve_slot_sourced`'s
/// override > env > builtin chain. Returns only the resolved value.
pub(crate) fn resolve_effort(
    provider: &str,
    level: EffortLevel,
    env_key: &str,
    builtin: &str,
) -> String {
    resolve_effort_sourced(provider, level, env_key, builtin).0
}

/// As `resolve_effort`, also reporting which layer supplied the value.
pub(crate) fn resolve_effort_sourced(
    provider: &str,
    level: EffortLevel,
    env_key: &str,
    builtin: &str,
) -> (String, Source) {
    let stored = with_cfg(provider, |cfg| cfg.effort_value(level).to_string())
        .filter(|v| !v.is_empty());
    if let Some(v) = stored {
        return (v, Source::Override);
    }
    match env_value(env_key) {
        Some(v) => (v, Source::Env),
        None => (builtin.to_string(), Source::Builtin),
    }
}

/// Resolve a catalog id list: stored override, else the comma-separated env
/// var, else the built-in list.
pub(crate) fn resolve_catalog(provider: &str, env_key: &str, builtin: &[&str]) -> Vec<String> {
    resolve_catalog_sourced(provider, env_key, builtin).0
}

/// As `resolve_catalog`, also reporting which layer supplied the list.
pub(crate) fn resolve_catalog_sourced(
    provider: &str,
    env_key: &str,
    builtin: &[&str],
) -> (Vec<String>, Source) {
    let stored = with_cfg(provider, |cfg| cfg.models.clone()).filter(|v| !v.is_empty());
    if let Some(v) = stored {
        return (v, Source::Override);
    }
    if let Some(raw) = env_value(env_key) {
        let ids: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !ids.is_empty() {
            return (ids, Source::Env);
        }
    }
    (builtin.iter().map(|s| s.to_string()).collect(), Source::Builtin)
}

/// Whether the catalog is explicitly pinned (by override or env), meaning a
/// provider with a live `/models` endpoint must NOT fetch it. Callers used to
/// check the env var alone; an operator edit has to pin just as hard, otherwise
/// the live list would immediately overwrite what they just typed.
pub(crate) fn catalog_is_pinned(provider: &str, env_key: &str) -> bool {
    let stored_non_empty =
        with_cfg(provider, |cfg| !cfg.models.is_empty()).unwrap_or(false);
    stored_non_empty || env_value(env_key).is_some()
}

// ---------------------------------------------------------------------------
// Provider spec table — the single description of every editable mapping
// ---------------------------------------------------------------------------

/// One editable rewrite target.
pub(crate) struct SlotSpec {
    pub(crate) slot: Slot,
    /// Human label for the UI row.
    pub(crate) label: &'static str,
    /// Which client-side model names land in this slot.
    pub(crate) matches: &'static str,
    pub(crate) env: &'static str,
    pub(crate) builtin: &'static str,
}

/// Everything the UI and the resolver need to know about one provider's model
/// mapping. This table is the single place the mapping is described; the
/// provider modules read their own values back out of it via `spec()`.
pub(crate) struct ProviderModelSpec {
    pub(crate) provider: &'static str,
    pub(crate) label: &'static str,
    /// One-line summary of the rewrite behaviour, shown above the slot rows.
    pub(crate) rule: &'static str,
    pub(crate) slots: &'static [SlotSpec],
    pub(crate) catalog_env: &'static str,
    pub(crate) catalog_builtin: &'static [&'static str],
    /// True when the catalog is normally fetched from the upstream's `/models`
    /// and the built-in list is only a fallback.
    pub(crate) catalog_live: bool,
}

impl ProviderModelSpec {
    /// The spec for one slot, if this provider has it.
    pub(crate) fn slot(&self, slot: Slot) -> Option<&SlotSpec> {
        self.slots.iter().find(|s| s.slot == slot)
    }

    /// Resolve one of this provider's slots. Panics only on a slot the provider
    /// doesn't declare, which is a programming error in the table.
    pub(crate) fn resolve(&self, slot: Slot) -> String {
        let spec = self
            .slot(slot)
            .unwrap_or_else(|| panic!("provider {} has no {} slot", self.provider, slot.as_str()));
        resolve_slot(self.provider, slot, spec.env, spec.builtin)
    }

    /// Resolve this provider's catalog id list.
    pub(crate) fn catalog(&self) -> Vec<String> {
        resolve_catalog(self.provider, self.catalog_env, self.catalog_builtin)
    }

    /// Whether the catalog is pinned, so a live `/models` fetch must be skipped.
    pub(crate) fn catalog_pinned(&self) -> bool {
        catalog_is_pinned(self.provider, self.catalog_env)
    }
}

/// Every provider whose model mapping is editable.
///
/// Absent on purpose: `codex` / `claude` send the client's model straight
/// through to their first-party upstream (there is no rewrite to configure),
/// and `cursor` maps its bare slug to the fixed literal `default` that the
/// upstream's own model list defines — not an operator choice.
pub(crate) const PROVIDER_MODEL_SPECS: &[ProviderModelSpec] = &[
    ProviderModelSpec {
        provider: "deepseek",
        label: "DeepSeek",
        rule: "按 Claude Code 的 3 档改写：opus / sonnet（含 haiku）/ fable 各落到对应上游档。bare `deepseek` 或未知名 → 默认档。`deepseek/<id>` 直接指定。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档 (bare deepseek)",
                matches: "裸 `deepseek`，以及未匹配到任何 Claude 档的外来名",
                env: "DEEPSEEK_DEFAULT_MODEL",
                builtin: super::deepseek::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "DEEPSEEK_OPUS_MODEL",
                builtin: super::deepseek::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "DEEPSEEK_SONNET_MODEL",
                builtin: super::deepseek::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*（Claude Code 最便宜的 tier）",
                env: "DEEPSEEK_FABLE_MODEL",
                builtin: super::deepseek::BUILTIN_FABLE_MODEL,
            },
        ],
        catalog_env: "DEEPSEEK_MODELS",
        catalog_builtin: super::deepseek::BUILTIN_MODELS,
        catalog_live: false,
    },
    ProviderModelSpec {
        provider: "minimax",
        label: "MiniMax",
        rule: "Claude 流量按 3 档改写到 MiniMax 自己的模型（默认都用 MiniMax-M3，可在面板覆盖）。minimax/<id> 直接指定，裸 minimax 走默认档。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档",
                matches: "裸 minimax（含 minimax-<id> 未识别时）",
                env: "MINIMAX_DEFAULT_MODEL",
                builtin: super::minimax::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "MINIMAX_OPUS_MODEL",
                builtin: super::minimax::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "MINIMAX_SONNET_MODEL",
                builtin: super::minimax::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*",
                env: "MINIMAX_FABLE_MODEL",
                builtin: super::minimax::BUILTIN_FABLE_MODEL,
            },
        ],
        catalog_env: "MINIMAX_MODELS",
        catalog_builtin: super::minimax::BUILTIN_MODELS,
        catalog_live: false,
    },
    ProviderModelSpec {
        provider: "trae",
        label: "Trae",
        rule: "Claude 流量按 3 档改写到 sidecar 的上游模型（默认都用 minimax-m3）。trae/<id>、trae-<id> 直接指定，裸 trae 走默认档。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档",
                matches: "裸 trae / 未识别名",
                env: "TRAE_DEFAULT_MODEL",
                builtin: super::trae::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "TRAE_OPUS_MODEL",
                builtin: super::trae::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "TRAE_SONNET_MODEL",
                builtin: super::trae::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*",
                env: "TRAE_FABLE_MODEL",
                builtin: super::trae::BUILTIN_FABLE_MODEL,
            },
        ],
        catalog_env: "TRAE_MODELS",
        catalog_builtin: super::trae::BUILTIN_MODELS,
        catalog_live: true,
    },
    ProviderModelSpec {
        provider: "glm",
        label: "GLM (智谱 / z.ai)",
        rule: "Claude 流量按 3 档改写到 GLM 自己的模型（默认都用 glm-5.2，可在面板覆盖）。glm/<id>、glm-<id> 直接指定，裸 glm 走默认档。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档",
                matches: "裸 glm",
                env: "GLM_DEFAULT_MODEL",
                builtin: super::glm::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "GLM_OPUS_MODEL",
                builtin: super::glm::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "GLM_SONNET_MODEL",
                builtin: super::glm::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*",
                env: "GLM_FABLE_MODEL",
                builtin: super::glm::BUILTIN_FABLE_MODEL,
            },
        ],
        catalog_env: "GLM_MODELS",
        catalog_builtin: super::glm::BUILTIN_MODELS,
        catalog_live: true,
    },
    ProviderModelSpec {
        provider: "kimi",
        label: "Kimi (Moonshot)",
        rule: "Claude 流量按 3 档改写到 Kimi 自己的模型（默认都用 kimi-k2-0711-preview，可在面板覆盖）。kimi/<id>、moonshot/<id> 直接指定，裸 kimi 走默认档。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档",
                matches: "裸 kimi",
                env: "KIMI_DEFAULT_MODEL",
                builtin: super::kimi::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "KIMI_OPUS_MODEL",
                builtin: super::kimi::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "KIMI_SONNET_MODEL",
                builtin: super::kimi::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*",
                env: "KIMI_FABLE_MODEL",
                builtin: super::kimi::BUILTIN_FABLE_MODEL,
            },
        ],
        catalog_env: "KIMI_MODELS",
        catalog_builtin: super::kimi::BUILTIN_MODELS,
        catalog_live: true,
    },
    ProviderModelSpec {
        provider: "ollama",
        label: "Ollama (本地)",
        rule: "Claude 流量按 3 档改写到本地 Ollama 模型（默认都用 llama3，可在面板覆盖）。ollama/<tag> 直接指定，裸 ollama 走默认档。",
        slots: &[
            SlotSpec {
                slot: Slot::Default,
                label: "默认档",
                matches: "裸 ollama",
                env: "OLLAMA_DEFAULT_MODEL",
                builtin: super::ollama::BUILTIN_DEFAULT_MODEL,
            },
            SlotSpec {
                slot: Slot::Opus,
                label: "opus 档",
                matches: "claude-opus-*",
                env: "OLLAMA_OPUS_MODEL",
                builtin: super::ollama::BUILTIN_OPUS_MODEL,
            },
            SlotSpec {
                slot: Slot::Sonnet,
                label: "sonnet + haiku 档",
                matches: "claude-sonnet-*、claude-haiku-*（haiku 合并到 sonnet）",
                env: "OLLAMA_SONNET_MODEL",
                builtin: super::ollama::BUILTIN_SONNET_MODEL,
            },
            SlotSpec {
                slot: Slot::Fable,
                label: "fable 档",
                matches: "claude-fable-*",
                env: "OLLAMA_FABLE_MODEL",
                builtin: super::ollama::BUILTIN_FABLE_MODEL,
            },
        ],
        // Ollama has no catalog env var: the local daemon's `/api/tags` is the
        // only truth about which models are actually pulled, and typing an id
        // that isn't pulled would just 404 at request time.
        catalog_env: "",
        catalog_builtin: &[],
        catalog_live: true,
    },
];

/// The spec for a provider, or `None` if its mapping isn't operator-editable.
pub(crate) fn spec(provider: &str) -> Option<&'static ProviderModelSpec> {
    PROVIDER_MODEL_SPECS.iter().find(|s| s.provider == provider)
}

// ---------------------------------------------------------------------------
// Reasoning-effort mapping — the same override > env > builtin chain, applied
// to the `reasoning.effort` tier of the three Responses-API providers.
// ---------------------------------------------------------------------------

/// One editable reasoning-effort target.
pub(crate) struct EffortLevelSpec {
    pub(crate) level: EffortLevel,
    /// Human label for the UI row.
    pub(crate) label: &'static str,
    pub(crate) env: &'static str,
    pub(crate) builtin: &'static str,
}

/// Everything the UI and the resolver need to know about one provider's
/// reasoning-effort mapping. Only Responses-API providers (codex / minimax /
/// deepseek) have one — the Claude-format path handles `output_config.effort`
/// natively, and the Chat-Completions adapters carry no reasoning effort.
pub(crate) struct ProviderEffortSpec {
    pub(crate) provider: &'static str,
    pub(crate) label: &'static str,
    pub(crate) levels: &'static [EffortLevelSpec],
}

impl ProviderEffortSpec {
    /// The spec for one effort tier, if this provider declares it.
    pub(crate) fn level(&self, level: EffortLevel) -> Option<&EffortLevelSpec> {
        self.levels.iter().find(|l| l.level == level)
    }

    /// Resolve one of this provider's effort tiers.
    pub(crate) fn resolve(&self, level: EffortLevel) -> String {
        let spec = self.level(level).unwrap_or_else(|| {
            panic!(
                "provider {} has no {} effort tier",
                self.provider,
                level.as_str()
            )
        });
        resolve_effort(self.provider, level, spec.env, spec.builtin)
    }
}

/// Providers whose reasoning-effort mapping is editable. Only the three that
/// serve the Responses API natively (`/v1/responses`) — the effort tier is a
/// `reasoning.effort` field on that surface. Claude-format providers don't
/// rewrite effort; the Chat-Completions adapters don't accept it at all.
pub(crate) const PROVIDER_EFFORT_SPECS: &[ProviderEffortSpec] = &[
    ProviderEffortSpec {
        provider: "codex",
        label: "Codex (OpenAI)",
        levels: &[
            EffortLevelSpec {
                level: EffortLevel::Default,
                label: "默认档（未传 effort）",
                env: "CODEX_DEFAULT_EFFORT",
                builtin: super::codex::CODEX_DEFAULT_EFFORT,
            },
            EffortLevelSpec {
                level: EffortLevel::Low,
                label: "low 档",
                env: "CODEX_EFFORT_LOW",
                builtin: super::codex::CODEX_EFFORT_LOW,
            },
            EffortLevelSpec {
                level: EffortLevel::Medium,
                label: "medium 档",
                env: "CODEX_EFFORT_MEDIUM",
                builtin: super::codex::CODEX_EFFORT_MEDIUM,
            },
            EffortLevelSpec {
                level: EffortLevel::High,
                label: "high 档",
                env: "CODEX_EFFORT_HIGH",
                builtin: super::codex::CODEX_EFFORT_HIGH,
            },
            EffortLevelSpec {
                level: EffortLevel::Xhigh,
                label: "xhigh 档",
                env: "CODEX_EFFORT_XHIGH",
                builtin: super::codex::CODEX_EFFORT_XHIGH,
            },
        ],
    },
    ProviderEffortSpec {
        provider: "minimax",
        label: "MiniMax",
        levels: &[
            EffortLevelSpec {
                level: EffortLevel::Default,
                label: "默认档（未传 effort）",
                env: "MINIMAX_DEFAULT_EFFORT",
                builtin: super::minimax::MINIMAX_DEFAULT_EFFORT,
            },
            EffortLevelSpec {
                level: EffortLevel::Low,
                label: "low 档",
                env: "MINIMAX_EFFORT_LOW",
                builtin: super::minimax::MINIMAX_EFFORT_LOW,
            },
            EffortLevelSpec {
                level: EffortLevel::Medium,
                label: "medium 档",
                env: "MINIMAX_EFFORT_MEDIUM",
                builtin: super::minimax::MINIMAX_EFFORT_MEDIUM,
            },
            EffortLevelSpec {
                level: EffortLevel::High,
                label: "high 档",
                env: "MINIMAX_EFFORT_HIGH",
                builtin: super::minimax::MINIMAX_EFFORT_HIGH,
            },
            EffortLevelSpec {
                level: EffortLevel::Xhigh,
                label: "xhigh 档",
                env: "MINIMAX_EFFORT_XHIGH",
                builtin: super::minimax::MINIMAX_EFFORT_XHIGH,
            },
        ],
    },
    ProviderEffortSpec {
        provider: "deepseek",
        label: "DeepSeek",
        levels: &[
            EffortLevelSpec {
                level: EffortLevel::Default,
                label: "默认档（未传 effort）",
                env: "DEEPSEEK_DEFAULT_EFFORT",
                builtin: super::deepseek::DEEPSEEK_DEFAULT_EFFORT,
            },
            EffortLevelSpec {
                level: EffortLevel::Low,
                label: "low 档",
                env: "DEEPSEEK_EFFORT_LOW",
                builtin: super::deepseek::DEEPSEEK_EFFORT_LOW,
            },
            EffortLevelSpec {
                level: EffortLevel::Medium,
                label: "medium 档",
                env: "DEEPSEEK_EFFORT_MEDIUM",
                builtin: super::deepseek::DEEPSEEK_EFFORT_MEDIUM,
            },
            EffortLevelSpec {
                level: EffortLevel::High,
                label: "high 档",
                env: "DEEPSEEK_EFFORT_HIGH",
                builtin: super::deepseek::DEEPSEEK_EFFORT_HIGH,
            },
            EffortLevelSpec {
                level: EffortLevel::Xhigh,
                label: "xhigh 档",
                env: "DEEPSEEK_EFFORT_XHIGH",
                builtin: super::deepseek::DEEPSEEK_EFFORT_XHIGH,
            },
        ],
    },
];

/// The effort spec for a provider, or `None` if effort isn't mapped for it.
pub(crate) fn effort_spec(provider: &str) -> Option<&'static ProviderEffortSpec> {
    PROVIDER_EFFORT_SPECS.iter().find(|s| s.provider == provider)
}

/// Normalise the client's `reasoning.effort` (if any) onto this provider's own
/// tier, and inject the provider default when the client sent none. Only
/// Responses-API providers are mapped; anything else is a no-op so the
/// Claude-format `output_config.effort` path stays untouched.
pub(crate) fn apply_effort_mapping(payload: &mut Value, provider: &str) {
    let Some(spec) = effort_spec(provider) else {
        return;
    };
    let Some(obj) = payload.as_object_mut() else {
        return;
    };

    let input = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .map(|s| s.trim().to_ascii_lowercase());

    let level = match input.as_deref() {
        Some("minimal") | Some("low") => EffortLevel::Low,
        Some("medium") => EffortLevel::Medium,
        Some("high") => EffortLevel::High,
        Some("xhigh") => EffortLevel::Xhigh,
        _ => EffortLevel::Default,
    };

    let target = spec.resolve(level);

    let reasoning = obj
        .entry("reasoning".to_string())
        .or_insert_with(|| json!({}));
    if let Some(robj) = reasoning.as_object_mut() {
        robj.insert("effort".to_string(), Value::String(target));
    }
}

// ---------------------------------------------------------------------------
// Persistence (mirrors `chains.rs`: single JSON doc, atomic replace)
// ---------------------------------------------------------------------------

/// Load the overrides from disk into the process-global store. A missing file
/// is the normal first-run case; an unreadable one is logged and ignored rather
/// than fatal, so a hand-corrupted config can't stop the gateway from starting.
pub(crate) async fn load_model_config(path: &std::path::Path) {
    let parsed = match tokio::fs::read_to_string(path).await {
        Ok(data) => match serde_json::from_str::<ModelOverrides>(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!("provider_models.json unreadable ({}); ignoring overrides", e);
                return;
            }
        },
        Err(_) => return,
    };
    replace(parsed);
}

/// Atomically persist the current overrides (temp file → fsync → rename → fsync
/// dir), serialized behind the shared `persist_lock` like every other on-disk
/// state in this gateway.
pub(crate) async fn persist_model_config(state: &AppState) -> Result<(), String> {
    let _guard = state.persist_lock.lock().await;
    let json = serde_json::to_string_pretty(&snapshot()).map_err(|e| e.to_string())?;
    let tmp = state.model_config_file.with_extension("json.tmp");
    {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&tmp, &state.model_config_file).await.map_err(|e| e.to_string())?;
    crate::pool::storage::sync_parent_dir(&state.model_config_file).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here mutates the process-global store and env, so they must
    /// not interleave.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn cfg(default_model: &str, models: &[&str]) -> ProviderModelCfg {
        ProviderModelCfg {
            default_model: default_model.to_string(),
            opus_model: String::new(),
            sonnet_model: String::new(),
            fable_model: String::new(),
            models: models.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn slot_precedence_is_override_then_env_then_builtin() {
        let _g = lock();
        replace(ModelOverrides::new());
        std::env::remove_var("TEST_SLOT_ENV");

        // Nothing configured -> built-in.
        let (v, src) = resolve_slot_sourced("minimax", Slot::Default, "TEST_SLOT_ENV", "builtin-m");
        assert_eq!(v, "builtin-m");
        assert_eq!(src, Source::Builtin);

        // Env beats the built-in.
        std::env::set_var("TEST_SLOT_ENV", "env-m");
        let (v, src) = resolve_slot_sourced("minimax", Slot::Default, "TEST_SLOT_ENV", "builtin-m");
        assert_eq!(v, "env-m");
        assert_eq!(src, Source::Env);

        // A stored override beats the env — that's the whole point of the file.
        let mut doc = ModelOverrides::new();
        doc.insert("minimax".to_string(), cfg("stored-m", &[]));
        replace(doc);
        let (v, src) = resolve_slot_sourced("minimax", Slot::Default, "TEST_SLOT_ENV", "builtin-m");
        assert_eq!(v, "stored-m");
        assert_eq!(src, Source::Override);

        std::env::remove_var("TEST_SLOT_ENV");
        replace(ModelOverrides::new());
    }

    #[test]
    fn catalog_precedence_and_pinning() {
        let _g = lock();
        replace(ModelOverrides::new());
        std::env::remove_var("TEST_CATALOG_ENV");
        let builtin: &[&str] = &["a", "b"];

        let (v, src) = resolve_catalog_sourced("glm", "TEST_CATALOG_ENV", builtin);
        assert_eq!(v, vec!["a", "b"]);
        assert_eq!(src, Source::Builtin);
        // A live-fetching provider must still fetch while nothing pins the list.
        assert!(!catalog_is_pinned("glm", "TEST_CATALOG_ENV"));

        std::env::set_var("TEST_CATALOG_ENV", " c , d ,, ");
        let (v, src) = resolve_catalog_sourced("glm", "TEST_CATALOG_ENV", builtin);
        assert_eq!(v, vec!["c", "d"]); // whitespace trimmed, blanks dropped
        assert_eq!(src, Source::Env);
        assert!(catalog_is_pinned("glm", "TEST_CATALOG_ENV"));

        let mut doc = ModelOverrides::new();
        doc.insert("glm".to_string(), cfg("", &["e"]));
        replace(doc);
        let (v, src) = resolve_catalog_sourced("glm", "TEST_CATALOG_ENV", builtin);
        assert_eq!(v, vec!["e"]);
        assert_eq!(src, Source::Override);
        // An operator edit must pin as hard as the env var, or the live fetch
        // would overwrite what they just typed.
        assert!(catalog_is_pinned("glm", "TEST_CATALOG_ENV"));

        std::env::remove_var("TEST_CATALOG_ENV");
        replace(ModelOverrides::new());
    }

    #[test]
    fn replace_normalizes_and_drops_empty_entries() {
        let _g = lock();
        let mut doc = ModelOverrides::new();
        doc.insert(" DeepSeek ".to_string(), cfg("  ds-pro  ", &[" x ", "", "y"]));
        doc.insert("minimax".to_string(), ProviderModelCfg::default());
        replace(doc);

        let snap = snapshot();
        // Provider key lowercased/trimmed, values trimmed, blank catalog entry gone.
        let ds = snap.get("deepseek").expect("deepseek entry kept");
        assert_eq!(ds.default_model, "ds-pro");
        assert_eq!(ds.models, vec!["x", "y"]);
        // An all-empty config overrides nothing, so it isn't stored at all.
        assert!(!snap.contains_key("minimax"));

        replace(ModelOverrides::new());
    }

    #[test]
    fn every_spec_slot_resolves_to_its_builtin_when_unconfigured() {
        let _g = lock();
        replace(ModelOverrides::new());
        for spec in PROVIDER_MODEL_SPECS {
            for slot in spec.slots {
                std::env::remove_var(slot.env);
                assert!(
                    !slot.builtin.trim().is_empty(),
                    "{}/{} has no built-in model",
                    spec.provider,
                    slot.slot.as_str()
                );
                assert_eq!(spec.resolve(slot.slot), slot.builtin);
            }
        }
    }

    #[test]
    fn effort_precedence_is_override_then_env_then_builtin() {
        let _g = lock();
        replace(ModelOverrides::new());
        std::env::remove_var("TEST_EFFORT_ENV");

        let (v, src) =
            resolve_effort_sourced("codex", EffortLevel::Default, "TEST_EFFORT_ENV", "xhigh");
        assert_eq!(v, "xhigh");
        assert_eq!(src, Source::Builtin);

        std::env::set_var("TEST_EFFORT_ENV", "max");
        let (v, src) =
            resolve_effort_sourced("codex", EffortLevel::Default, "TEST_EFFORT_ENV", "xhigh");
        assert_eq!(v, "max");
        assert_eq!(src, Source::Env);

        let mut doc = ModelOverrides::new();
        doc.insert(
            "codex".to_string(),
            ProviderModelCfg {
                default_effort: "high".to_string(),
                ..Default::default()
            },
        );
        replace(doc);
        let (v, src) =
            resolve_effort_sourced("codex", EffortLevel::Default, "TEST_EFFORT_ENV", "xhigh");
        assert_eq!(v, "high");
        assert_eq!(src, Source::Override);

        std::env::remove_var("TEST_EFFORT_ENV");
        replace(ModelOverrides::new());
    }

    #[test]
    fn apply_effort_mapping_rewrites_per_provider() {
        let _g = lock();
        replace(ModelOverrides::new());
        // Clear every real env var so the assertions run against the built-ins.
        for spec in PROVIDER_EFFORT_SPECS {
            for l in spec.levels {
                std::env::remove_var(l.env);
            }
        }

        // Missing effort -> inject the provider's default tier.
        let mut p = serde_json::json!({ "model": "x" });
        apply_effort_mapping(&mut p, "deepseek");
        assert_eq!(p["reasoning"]["effort"], "max");

        // `xhigh` (client) -> DeepSeek's own top tier.
        let mut p = serde_json::json!({ "reasoning": { "effort": "xhigh" } });
        apply_effort_mapping(&mut p, "deepseek");
        assert_eq!(p["reasoning"]["effort"], "max");

        // `minimal` folds into the `low` tier.
        let mut p = serde_json::json!({ "reasoning": { "effort": "minimal" } });
        apply_effort_mapping(&mut p, "deepseek");
        assert_eq!(p["reasoning"]["effort"], "low");

        // Unrecognised tier -> default (not blindly forwarded).
        let mut p = serde_json::json!({ "reasoning": { "effort": "banana" } });
        apply_effort_mapping(&mut p, "deepseek");
        assert_eq!(p["reasoning"]["effort"], "max");

        // Codex keeps its own vocabulary (xhigh stays xhigh).
        let mut p = serde_json::json!({ "reasoning": { "effort": "xhigh" } });
        apply_effort_mapping(&mut p, "codex");
        assert_eq!(p["reasoning"]["effort"], "xhigh");

        // MiniMax collapses every tier onto `high` (its only real tier).
        let mut p = serde_json::json!({ "reasoning": { "effort": "xhigh" } });
        apply_effort_mapping(&mut p, "minimax");
        assert_eq!(p["reasoning"]["effort"], "high");

        // A provider without an effort spec is a no-op — no `reasoning` injected.
        let mut p = serde_json::json!({ "model": "x" });
        apply_effort_mapping(&mut p, "claude");
        assert!(p.get("reasoning").is_none());

        replace(ModelOverrides::new());
    }
}
