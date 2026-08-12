//! Read/update the runtime model-mapping overrides (`/v1/provider/model-map`).
//!
//! GET is open to any identified caller (the UI reads it to render the panel);
//! PUT is restricted to owner-trusted callers — it changes how the very next
//! request is rewritten, and a stray edit could silently re-route traffic.
use crate::auth::identify_caller;
use crate::prelude::*;
use crate::provider::model_config::{
    self, persist_model_config, ModelOverrides, ProviderModelCfg, ProviderModelSpec, Slot, Source,
    PROVIDER_MODEL_SPECS,
};

/// One slot in the UI's "resolved value" column, with the source badge attached
/// so the page can say *why* an env var appears ignored.
#[derive(Debug, Serialize)]
pub(crate) struct ResolvedSlotView {
    pub(crate) slot: &'static str,
    pub(crate) label: &'static str,
    pub(crate) matches: &'static str,
    pub(crate) env: &'static str,
    pub(crate) builtin: &'static str,
    /// The override (if any) the operator has typed — sent back so a `PUT` of
    /// just one slot doesn't have to mention the others.
    pub(crate) override_value: Option<String>,
    /// The value the gateway will actually use.
    pub(crate) value: String,
    pub(crate) source: Source,
}

/// One provider's row in the UI table.
#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelView {
    pub(crate) provider: &'static str,
    pub(crate) label: &'static str,
    pub(crate) rule: &'static str,
    pub(crate) slots: Vec<ResolvedSlotView>,
    /// Override catalog ids (`None` = no override). The catalog itself comes from
    /// the live fetch endpoint; this file only pins it.
    pub(crate) catalog_override: Option<Vec<String>>,
    pub(crate) catalog_value: Vec<String>,
    pub(crate) catalog_source: Source,
    pub(crate) catalog_live: bool,
    pub(crate) catalog_env: &'static str,
    /// Built-in catalog ids, so the UI can show what it'd fall back to.
    pub(crate) catalog_builtin: &'static [&'static str],
}

fn slot_view(
    spec: &ProviderModelSpec,
    slot: &crate::provider::model_config::SlotSpec,
) -> ResolvedSlotView {
    let (value, source) = crate::provider::model_config::resolve_slot_sourced(
        spec.provider,
        slot.slot,
        slot.env,
        slot.builtin,
    );
    let override_value = crate::provider::model_config::snapshot()
        .get(spec.provider)
        .and_then(|cfg| {
            let v = cfg.slot_value(slot.slot);
            if v.is_empty() { None } else { Some(v.to_string()) }
        });
    ResolvedSlotView {
        slot: slot.slot.as_str(),
        label: slot.label,
        matches: slot.matches,
        env: slot.env,
        builtin: slot.builtin,
        override_value,
        value,
        source,
    }
}

/// `GET /v1/provider/model-map` — describe every editable provider mapping in a
/// shape the UI can render straight to DOM.
pub(crate) async fn get_model_map(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let _caller = identify_caller(&headers);
    let snapshot = crate::provider::model_config::snapshot();
    let mut providers = Vec::with_capacity(PROVIDER_MODEL_SPECS.len());
    for spec in PROVIDER_MODEL_SPECS {
        let slots: Vec<ResolvedSlotView> =
            spec.slots.iter().map(|s| slot_view(spec, s)).collect();
        let (catalog_ids, catalog_source) =
            crate::provider::model_config::resolve_catalog_sourced(
                spec.provider,
                spec.catalog_env,
                spec.catalog_builtin,
            );
        let _ = catalog_ids;
        let effective_source = if matches!(catalog_source, Source::Override) {
            Source::Override
        } else if catalog_source == Source::Env {
            Source::Env
        } else if spec.catalog_live {
            Source::Live
        } else {
            Source::Builtin
        };
        let catalog_override = snapshot
            .get(spec.provider)
            .map(|c| c.models.clone())
            .filter(|v| !v.is_empty());
        providers.push(ProviderModelView {
            provider: spec.provider,
            label: spec.label,
            rule: spec.rule,
            slots,
            catalog_override,
            catalog_value: spec.catalog(),
            catalog_source: effective_source,
            catalog_live: spec.catalog_live,
            catalog_env: spec.catalog_env,
            catalog_builtin: spec.catalog_builtin,
        });
    }
    let _ = state;
    (StatusCode::OK, Json(json!({ "providers": providers }))).into_response()
}

/// What the UI PUTs back. Every field optional; missing = "leave that piece
/// alone" so a one-slot edit doesn't have to ship the rest.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModelMapUpdateRequest {
    #[serde(default)]
    pub(crate) providers: std::collections::BTreeMap<String, ProviderModelCfg>,
}

/// `PUT /v1/provider/model-map` — replace any subset of the overrides. Each
/// provider's cfg is normalized (whitespace trimmed, empty entries dropped);
/// all-empty entries are deleted entirely so the file doesn't accumulate junk.
pub(crate) async fn update_model_map(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ModelMapUpdateRequest>,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    if !caller.owner_trusted {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "只有受信任的本机用户可以修改模型映射" })),
        )
            .into_response();
    }

    // Validate the keys up-front so an unknown provider name returns a clean
    // 400 instead of silently being kept in the file.
    let known: std::collections::BTreeSet<&'static str> =
        PROVIDER_MODEL_SPECS.iter().map(|s| s.provider).collect();
    for k in payload.providers.keys() {
        let k_norm = k.trim().to_ascii_lowercase();
        if !known.contains(k_norm.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("unknown provider '{}'", k_norm),
                    "known": known.iter().copied().collect::<Vec<_>>(),
                })),
            )
                .into_response();
        }
    }

    // Merge: anything the request omits keeps its previous override (so a one-
    // slot edit doesn't blank the others). Each entry is normalized in `replace`
    // — we only have to make sure keys are lowercase here.
    let previous = model_config::snapshot();
    let mut next: ModelOverrides = previous.clone();
    for (k, cfg) in payload.providers {
        let key = k.trim().to_ascii_lowercase();
        let cfg = cfg.normalized();
        if cfg.is_empty() {
            next.remove(&key);
        } else {
            next.insert(key, cfg);
        }
    }
    model_config::replace(next.clone());

    if let Err(e) = persist_model_config(&state).await {
        // Roll back to the previous in-memory state so a failed save doesn't
        // silently desync file and memory — the next request would otherwise
        // see values that aren't on disk.
        model_config::replace(previous);
        error!("failed persisting provider model config: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("保存失败: {}", e) })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(json!({ "providers": next }))).into_response()
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelMapTestResult {
    pub(crate) provider: &'static str,
    pub(crate) ok: bool,
    pub(crate) status: u16,
    pub(crate) latency_ms: u128,
    pub(crate) slot: String,
    pub(crate) sent_model: String,
    /// The model the upstream actually answered with, parsed from the response
    /// body via `usage::tokens::parse_response_model` — that's the one usage
    /// stats will bill against, so the test endpoint surfaces it explicitly.
    pub(crate) served_model: Option<String>,
    pub(crate) tokens: crate::prelude::TokenUsage,
    pub(crate) error: Option<String>,
    pub(crate) account_id: Option<String>,
}

/// `POST /v1/provider/model-map/test` — fire one minimal request through the
/// provider's CURRENT mapping and report what came back. This is what the
/// "测试" button on the UI page calls.
pub(crate) async fn test_model_map(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ModelMapTestRequest>,
) -> impl IntoResponse {
    let caller = identify_caller(&headers);
    if !caller.owner_trusted {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "只有受信任的本机用户可以触发测试" })),
        )
            .into_response();
    }

    let provider = payload.provider.trim().to_ascii_lowercase();
    let spec = match crate::provider::model_config::spec(&provider) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown provider '{}'", provider) })),
            )
                .into_response();
        }
    };
    let slot = match payload.slot.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("opus") => Slot::Opus,
        // `sonnet` is the canonical name; `haiku` is accepted as a shortcut
        // because the two share one upstream slot.
        Some("sonnet") | Some("haiku") => Slot::Sonnet,
        Some("fable") => Slot::Fable,
        _ => Slot::Default,
    };

    // Pick a healthy account. The test should be cheap and the API key shouldn't
    // need to be the caller's — the operator hitting "test" is almost certainly
    // an owner.
    let user_id = caller.id.clone();
    let account = crate::pool::select_healthy_account(
        &state,
        &provider,
        &user_id,
        None,
        false,
        !caller.owner_trusted,
    )
    .await;

    let Some(account) = account else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ModelMapTestResult {
                provider: spec.provider,
                ok: false,
                status: 0,
                latency_ms: 0,
                slot: slot.as_str().to_string(),
                sent_model: String::new(),
                served_model: None,
                tokens: crate::prelude::TokenUsage::default(),
                error: Some(format!(
                    "no healthy {} account available — connect one first",
                    spec.label
                )),
                account_id: None,
            }),
        )
            .into_response();
    };

    // What we'll send. Resolve through the current mapping — that's exactly
    // what the next real request will see.
    let sent_model = spec.resolve(slot);
    let body = json!({
        "model": sent_model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "ping" }],
        "stream": false,
    });

    let started = std::time::Instant::now();
    let result = dispatch_test_request(&provider, &account, &body).await;
    let latency_ms = started.elapsed().as_millis();

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::OK,
                Json(ModelMapTestResult {
                    provider: spec.provider,
                    ok: false,
                    status: 0,
                    latency_ms,
                    slot: slot.as_str().to_string(),
                    sent_model,
                    served_model: None,
                    tokens: crate::prelude::TokenUsage::default(),
                    error: Some(e),
                    account_id: Some(account.id),
                }),
            )
                .into_response();
        }
    };

    let served_model = crate::usage::tokens::parse_response_model(&result.body);
    let ok = result.status.is_success();

    (
        StatusCode::OK,
        Json(ModelMapTestResult {
            provider: spec.provider,
            ok,
            status: result.status.as_u16(),
            latency_ms,
            slot: slot.as_str().to_string(),
            sent_model,
            served_model,
            tokens: result.usage,
            error: if ok { None } else { Some(truncate(&result.body, 400)) },
            account_id: Some(account.id),
        }),
    )
        .into_response()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t = s[..max].to_string();
        t.push_str("…");
        t
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModelMapTestRequest {
    pub(crate) provider: String,
    /// `"default"` or `"haiku"`; defaults to `"default"`.
    #[serde(default)]
    pub(crate) slot: Option<String>,
}

/// What `dispatch_test_request` returns: HTTP status, body text, parsed usage.
struct TestUpstream {
    status: reqwest::StatusCode,
    body: String,
    usage: crate::prelude::TokenUsage,
}

/// Send the minimal request through whichever upstream call the provider has,
/// keeping the same shape as `probe_*`. The body uses provider-native schema so
/// the upstream's actual model-mapper gets exercised.
async fn dispatch_test_request(
    provider: &str,
    account: &UpstreamAccount,
    body: &Value,
) -> Result<TestUpstream, String> {
    let model_id = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let req = crate::provider::cursor::ExtractedRequest {
        instruction: String::new(),
        turns: vec![crate::provider::cursor::ChatTurn {
            role: 1,
            content: "ping".into(),
        }],
        stream: false,
    };
    match provider {
        "glm" => {
            let r = crate::provider::glm::send_glm_openai(
                account,
                model_id,
                body,
            )
            .await?;
            Ok(TestUpstream {
                status: r.status,
                body: r.error.unwrap_or_else(|| r.text.clone()),
                usage: r.usage,
            })
        }
        "kimi" => {
            let r = crate::provider::kimi::send_kimi_openai(
                account,
                model_id,
                body,
            )
            .await?;
            Ok(TestUpstream {
                status: r.status,
                body: r.error.unwrap_or_else(|| r.text.clone()),
                usage: r.usage,
            })
        }
        "ollama" => {
            let r = crate::provider::ollama::send_ollama_upstream(
                crate::provider::ollama::ollama_http_client(),
                account,
                model_id,
                &req,
            )
            .await?;
            Ok(TestUpstream {
                status: r.status,
                body: r.error.unwrap_or_else(|| r.text.clone()),
                usage: r.usage,
            })
        }
        "minimax" => {
            let resp = crate::provider::minimax::send_minimax_anthropic(account, body).await?;
            collect_test_response(resp, "minimax").await
        }
        "trae" => {
            let resp = crate::provider::trae::send_trae_anthropic(account, body).await?;
            collect_test_response(resp, "trae").await
        }
        "deepseek" => {
            let resp = crate::provider::deepseek::send_deepseek_anthropic(account, body).await?;
            collect_test_response(resp, "deepseek").await
        }
        other => Err(format!("test not wired for provider '{}'", other)),
    }
}

async fn collect_test_response(
    resp: reqwest::Response,
    provider: &str,
) -> Result<TestUpstream, String> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading upstream body failed: {}", e))?;
    let usage = crate::usage::tokens::parse_usage(provider, &body);
    Ok(TestUpstream { status, body, usage })
}