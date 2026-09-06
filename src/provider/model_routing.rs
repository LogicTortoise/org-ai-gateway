use crate::provider::{
    Provider, chains::ChainCfg, chains::ChainMode, chains::ChainSlot, chains::ordered_attempts,
};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Validated model-specific provider overrides.
///
/// A matching rule selects exactly one provider and one literal upstream model.
/// Requests without a matching rule continue through the normal provider chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ModelRouting {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) slots: BTreeMap<String, SlotRouting>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct SlotRouting {
    #[serde(default)]
    pub(crate) rules: BTreeMap<String, ModelRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ModelRoute {
    pub(crate) provider: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteDecision {
    pub(crate) rule_id: String,
    pub(crate) requested_model: String,
    pub(crate) provider: Provider,
    pub(crate) upstream_model: String,
}

impl ModelRouting {
    pub(crate) fn canonical_empty() -> Self {
        Self {
            version: 1,
            slots: BTreeMap::new(),
        }
    }

    pub(crate) fn normalize(input: ModelRouting) -> Result<Self, String> {
        if input.version != 1 {
            return Err(format!(
                "unsupported model routing version {} (expected 1)",
                input.version
            ));
        }

        let mut slots = BTreeMap::new();
        for (raw_slot, raw_routes) in input.slots {
            let slot_name = canonicalize_slot(&raw_slot)?;
            if slots.contains_key(&slot_name) {
                return Err(format!("duplicate routing slot '{}'", slot_name));
            }
            let slot = parse_slot(&slot_name)?;
            let allowed = slot.allowed_providers();
            let mut rules = BTreeMap::new();
            let mut seen_rules = HashSet::new();

            for (raw_model, route) in raw_routes.rules {
                let requested_model = raw_model.trim();
                if requested_model.is_empty() {
                    return Err("model routing rule key must not be empty".into());
                }
                let rule_id = requested_model.to_ascii_lowercase();
                if !seen_rules.insert(rule_id.clone()) {
                    return Err(format!(
                        "duplicate model routing rule '{}'",
                        requested_model
                    ));
                }

                let provider = Provider::parse(&route.provider).ok_or_else(|| {
                    format!(
                        "unknown provider '{}' in {} model route",
                        route.provider,
                        slot.as_str()
                    )
                })?;
                if !allowed.contains(&provider.as_str()) {
                    return Err(format!(
                        "provider '{}' is not allowed in {} slot",
                        provider.as_str(),
                        slot.as_str()
                    ));
                }

                let upstream_model = route.model.trim();
                if upstream_model.is_empty() {
                    return Err(format!(
                        "{} model route '{}' upstream model must not be empty",
                        slot.as_str(),
                        requested_model
                    ));
                }

                rules.insert(
                    rule_id,
                    ModelRoute {
                        provider: provider.as_str().to_string(),
                        model: upstream_model.to_string(),
                    },
                );
            }

            slots.insert(slot_name, SlotRouting { rules });
        }

        Ok(Self { version: 1, slots })
    }

    pub(crate) fn resolve(&self, slot: ChainSlot, requested_model: &str) -> Option<RouteDecision> {
        let rule_id = requested_model.trim().to_ascii_lowercase();
        if rule_id.is_empty() {
            return None;
        }
        let route = self.slots.get(slot.as_str())?.rules.get(&rule_id)?;
        Some(RouteDecision {
            rule_id,
            requested_model: requested_model.to_string(),
            provider: Provider::parse(&route.provider).expect("canonical model route provider"),
            upstream_model: route.model.clone(),
        })
    }
}

impl Default for ModelRouting {
    fn default() -> Self {
        Self::canonical_empty()
    }
}

/// A model route replaces the chain with one provider. Without a route, retain
/// the existing failover/round-robin chain behavior.
pub(crate) fn provider_order(
    cfg: &ChainCfg,
    decision: Option<&RouteDecision>,
    rr_offset: usize,
) -> Vec<String> {
    match decision {
        Some(route) => vec![route.provider.as_str().to_string()],
        None => ordered_attempts(cfg, rr_offset),
    }
}

/// Model-routed requests do not consume the global chain's round-robin counter.
pub(crate) fn next_round_robin_offset(
    cfg: &ChainCfg,
    prev_counter: usize,
    decision: Option<&RouteDecision>,
) -> (usize, usize) {
    if decision.is_some() || !matches!(cfg.mode, ChainMode::RoundRobin) {
        return (prev_counter, 0);
    }
    (prev_counter.wrapping_add(1), prev_counter)
}

fn canonicalize_slot(value: &str) -> Result<String, String> {
    let slot = value.trim().to_ascii_lowercase();
    match slot.as_str() {
        "codex" | "claude" => Ok(slot),
        _ => Err(format!("unknown routing slot: {}", value)),
    }
}

fn parse_slot(value: &str) -> Result<ChainSlot, String> {
    match value {
        "codex" => Ok(ChainSlot::Codex),
        "claude" => Ok(ChainSlot::Claude),
        _ => Err(format!("unknown routing slot: {}", value)),
    }
}

/// Invalid routing never prevents startup; requests fall back to normal chains.
pub(crate) async fn load_model_routing(path: &Path) -> ModelRouting {
    match tokio::fs::read_to_string(path).await {
        Ok(data) => match serde_json::from_str::<ModelRouting>(&data) {
            Ok(value) => match ModelRouting::normalize(value) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        "model routing config invalid: {}; using normal provider chains",
                        error
                    );
                    ModelRouting::canonical_empty()
                }
            },
            Err(error) => {
                tracing::warn!(
                    "model routing config unreadable: {}; using normal provider chains",
                    error
                );
                ModelRouting::canonical_empty()
            }
        },
        Err(_) => ModelRouting::canonical_empty(),
    }
}

pub(crate) async fn persist_and_publish_model_routing(
    state: &AppState,
    candidate: ModelRouting,
) -> Result<(), String> {
    let _guard = state.persist_lock.lock().await;
    write_atomic(&state.model_routing_file, &candidate).await?;
    *state.model_routing.write().await = candidate;
    Ok(())
}

pub(crate) async fn write_atomic(path: &Path, candidate: &ModelRouting) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(candidate).map_err(|error| error.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|error| format!("create {}: {}", tmp.display(), error))?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&data)
            .await
            .map_err(|error| format!("write {}: {}", tmp.display(), error))?;
        file.sync_all()
            .await
            .map_err(|error| format!("fsync {}: {}", tmp.display(), error))?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|error| format!("rename {} -> {}: {}", tmp.display(), path.display(), error))?;
    crate::pool::storage::sync_parent_dir(path).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn chain(providers: &[&str], mode: ChainMode) -> ChainCfg {
        ChainCfg {
            mode,
            providers: providers.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn routing(provider: &str, upstream_model: &str) -> ModelRouting {
        ModelRouting {
            version: 1,
            slots: [(
                "codex".into(),
                SlotRouting {
                    rules: [(
                        "gpt-5.6-luna".into(),
                        ModelRoute {
                            provider: provider.into(),
                            model: upstream_model.into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        }
    }

    #[test]
    fn default_is_empty_v1() {
        assert_eq!(ModelRouting::default(), ModelRouting::canonical_empty());
    }

    #[test]
    fn deserialize_requires_version() {
        let error = serde_json::from_str::<ModelRouting>(r#"{"slots":{}}"#).unwrap_err();
        assert!(error.to_string().contains("missing field `version`"));
    }

    #[test]
    fn normalize_rejects_unsupported_version() {
        let mut raw = routing("minimax", "MiniMax-M3");
        raw.version = 2;
        assert!(ModelRouting::normalize(raw).is_err());
    }

    #[test]
    fn normalize_rejects_unknown_or_illegal_provider() {
        let error = ModelRouting::normalize(routing("bogus", "model")).unwrap_err();
        assert!(error.contains("unknown provider"));

        let raw = ModelRouting {
            version: 1,
            slots: [(
                "codex".into(),
                SlotRouting {
                    rules: [(
                        "gpt".into(),
                        ModelRoute {
                            provider: "trae".into(),
                            model: "trae-model".into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        };
        assert!(ModelRouting::normalize(raw).unwrap_err().contains("not allowed"));
    }

    #[test]
    fn normalize_requires_requested_and_upstream_models() {
        let raw = ModelRouting {
            version: 1,
            slots: [(
                "codex".into(),
                SlotRouting {
                    rules: [(
                        "  ".into(),
                        ModelRoute {
                            provider: "codex".into(),
                            model: "gpt-5".into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        };
        assert!(
            ModelRouting::normalize(raw)
                .unwrap_err()
                .contains("rule key")
        );
        assert!(
            ModelRouting::normalize(routing("minimax", "   "))
                .unwrap_err()
                .contains("upstream model")
        );
    }

    #[test]
    fn normalize_canonicalizes_slot_rule_provider_and_model() {
        let raw = ModelRouting {
            version: 1,
            slots: [(
                " CODEX ".into(),
                SlotRouting {
                    rules: [(
                        " GPT-5.6-LUNA ".into(),
                        ModelRoute {
                            provider: " MiniMax ".into(),
                            model: " MiniMax-M3 ".into(),
                        },
                    )]
                    .into(),
                },
            )]
            .into(),
        };
        let canonical = ModelRouting::normalize(raw).unwrap();
        let route = &canonical.slots["codex"].rules["gpt-5.6-luna"];
        assert_eq!(route.provider, "minimax");
        assert_eq!(route.model, "MiniMax-M3");
    }

    #[test]
    fn normalize_rejects_duplicate_canonical_rule_keys() {
        let raw = ModelRouting {
            version: 1,
            slots: [(
                "codex".into(),
                SlotRouting {
                    rules: [
                        (
                            "GPT-5".into(),
                            ModelRoute {
                                provider: "codex".into(),
                                model: "gpt-5".into(),
                            },
                        ),
                        (
                            "gpt-5".into(),
                            ModelRoute {
                                provider: "minimax".into(),
                                model: "MiniMax-M3".into(),
                            },
                        ),
                    ]
                    .into(),
                },
            )]
            .into(),
        };
        assert!(ModelRouting::normalize(raw).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn resolve_returns_single_literal_route_or_none() {
        let canonical =
            ModelRouting::normalize(routing("minimax", "MiniMax-M3")).expect("normalize");
        let decision = canonical
            .resolve(ChainSlot::Codex, " GPT-5.6-LUNA ")
            .expect("route");
        assert_eq!(decision.rule_id, "gpt-5.6-luna");
        assert_eq!(decision.requested_model, " GPT-5.6-LUNA ");
        assert_eq!(decision.provider, Provider::Minimax);
        assert_eq!(decision.upstream_model, "MiniMax-M3");
        assert!(
            canonical
                .resolve(ChainSlot::Codex, "gpt-5.6-terra")
                .is_none()
        );
    }

    #[test]
    fn provider_order_uses_route_or_normal_chain() {
        let cfg = chain(&["codex", "minimax"], ChainMode::Failover);
        let decision = RouteDecision {
            rule_id: "gpt-5.6-luna".into(),
            requested_model: "gpt-5.6-luna".into(),
            provider: Provider::Minimax,
            upstream_model: "MiniMax-M3".into(),
        };
        assert_eq!(
            provider_order(&cfg, Some(&decision), 0),
            vec!["minimax"]
        );
        assert_eq!(
            provider_order(&cfg, None, 0),
            vec!["codex", "minimax"]
        );
    }

    #[test]
    fn round_robin_advances_only_for_normal_chain() {
        let cfg = chain(&["codex", "minimax"], ChainMode::RoundRobin);
        let decision = RouteDecision {
            rule_id: "gpt-5.6-luna".into(),
            requested_model: "gpt-5.6-luna".into(),
            provider: Provider::Minimax,
            upstream_model: "MiniMax-M3".into(),
        };
        assert_eq!(next_round_robin_offset(&cfg, 4, Some(&decision)), (4, 0));
        assert_eq!(next_round_robin_offset(&cfg, 4, None), (5, 4));
    }

    #[tokio::test]
    async fn write_atomic_reports_bad_path() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "missing-model-routing-{}-{}/routing.json",
            std::process::id(),
            nonce
        ));
        assert!(write_atomic(&path, &ModelRouting::default()).await.is_err());
    }

    #[tokio::test]
    async fn write_atomic_round_trips() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "model-routing-{}-{}.json",
            std::process::id(),
            nonce
        ));
        let canonical = ModelRouting::normalize(routing("minimax", "MiniMax-M3")).unwrap();
        write_atomic(&path, &canonical).await.unwrap();
        assert_eq!(load_model_routing(&path).await, canonical);
        tokio::fs::remove_file(path).await.unwrap();
    }
}
