mod apikey;
mod auth;
mod capacity;
mod client_config;
mod cyber;
mod edge;
mod fingerprint;
mod models;
mod pool;
mod prelude;
mod provider;
mod quota;
mod retry;
mod routes;
mod sse;
mod state;
mod usage;
mod util;

use axum::{
    extract::DefaultBodyLimit,
    routing::{any, delete, get, post},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::prelude::*;
use crate::pool::storage::load_accounts;
use crate::routes::accounts::connect_account_legacy;
use crate::routes::accounts::connect_claude_auth_json;
use crate::routes::accounts::connect_claude_local;
use crate::routes::accounts::connect_codex_auth_json;
use crate::routes::accounts::connect_codex_local;
use crate::routes::accounts::connect_cursor;
use crate::routes::accounts::connect_cursor_local;
use crate::routes::accounts::connect_glm;
use crate::routes::accounts::connect_ollama;
use crate::routes::accounts::delete_account;
use crate::routes::accounts::list_accounts;
use crate::routes::accounts::toggle_share;
use crate::routes::apikeys::create_api_key;
use crate::routes::chains_api::get_chains;
use crate::routes::chains_api::update_chains;
use crate::routes::apikeys::delete_api_key;
use crate::routes::apikeys::list_api_keys;
use crate::routes::health::fallback_handler;
use crate::routes::health::health;
use crate::routes::health::index_html;
use crate::routes::health::donate_script;
use crate::routes::health::whoami;
use crate::routes::mock_auth::mock_auth_refresh;
use crate::routes::mock_auth::mock_auth_session;
use crate::routes::models_api::get_claude_models;
use crate::routes::models_api::get_codex_models;
use crate::routes::models_api::get_cursor_models;
use crate::routes::models_api::get_glm_models;
use crate::routes::models_api::get_ollama_models;
use crate::routes::models_api::proxy_models_codex;
use crate::routes::models_api::proxy_models_openai;
use crate::routes::proxy::proxy_chat_completions;
use crate::routes::proxy::proxy_claude_messages;
use crate::routes::proxy::proxy_responses;
use crate::routes::relay::relay;
use crate::routes::setup::claude_apply;
use crate::routes::setup::claude_restore;
use crate::routes::setup::codex_apply;
use crate::routes::setup::codex_bootstrap;
use crate::routes::setup::codex_restore;
use crate::routes::setup::cursor_apply;
use crate::routes::setup::cursor_restore;
use crate::routes::capacity::get_capacity;
use crate::routes::stats::get_stats;
use crate::routes::websocket::proxy_codex_ws;

#[tokio::main]
async fn main() {
    init_tracing();

    // Announce trusted-edge identity mode (external auth via identity headers).
    crate::edge::log_startup();
    // Announce per-user quota limits (GATEWAY_USER_*).
    crate::quota::log_startup();

    // Ensure the persistence directory exists before anything tries to read or
    // append to it; otherwise account/audit/capacity writes fail at runtime.
    if let Err(e) = tokio::fs::create_dir_all("./data").await {
        warn!("failed creating ./data directory: {}", e);
    }

    let account_file = PathBuf::from("./data/accounts.ndjson");
    let capacity_file = PathBuf::from("./data/capacity.ndjson");
    let api_key_file = PathBuf::from("./data/api_keys.ndjson");
    let chain_file = PathBuf::from("./data/provider_chains.json");
    let initial_accounts = load_accounts(&account_file).await;
    let initial_capacity = crate::capacity::load_capacity_history(&capacity_file).await;
    let initial_chains = crate::provider::chains::load_chains(&chain_file).await;
    crate::apikey::init(crate::apikey::load(&api_key_file).await);
    let state = AppState {
        audit_file: PathBuf::from("./data/audit.ndjson"),
        account_file,
        api_key_file,
        capacity_file,
        chain_file,
        accounts: Arc::new(RwLock::new(initial_accounts)),
        rate_limits: Arc::new(RwLock::new(HashMap::new())),
        capacity_history: Arc::new(RwLock::new(initial_capacity)),
        capacity_outlooks: Arc::new(RwLock::new(HashMap::new())),
        ws_session_bindings: Arc::new(RwLock::new(HashMap::new())),
        prompt_cache_bindings: Arc::new(RwLock::new(HashMap::new())),
        owner_usage: Arc::new(RwLock::new(HashMap::new())),
        user_usage: Arc::new(RwLock::new(HashMap::new())),
        user_request_rate: Arc::new(RwLock::new(HashMap::new())),
        persist_lock: Arc::new(tokio::sync::Mutex::new(())),
        refresh_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        chains: Arc::new(RwLock::new(initial_chains)),
        chain_rr: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    };

    // Background loops are supervised: a panic (or unexpected return) is logged
    // and the loop is restarted, so one bad iteration can't silently kill token
    // refresh / health probing for the rest of the process lifetime.
    spawn_supervised("penalty_decay", {
        let s = state.clone();
        move || crate::retry::run_penalty_decay(s.clone())
    });
    spawn_supervised("claude_token_refresh", {
        let s = state.clone();
        move || crate::provider::claude::run_claude_token_refresh(s.clone())
    });
    spawn_supervised("codex_token_refresh", {
        let s = state.clone();
        move || crate::provider::codex::run_codex_token_refresh(s.clone())
    });
    spawn_supervised("owner_usage_refresh", {
        let s = state.clone();
        move || crate::usage::run_owner_usage_refresh(s.clone())
    });
    spawn_supervised("account_health_probe", {
        let s = state.clone();
        move || crate::usage::run_account_health_probe(s.clone())
    });
    spawn_supervised("capacity_maintenance", {
        let s = state.clone();
        move || crate::capacity::run_capacity_maintenance(s.clone())
    });

    let app = Router::new()
        .route("/", get(index_html))
        .route("/donate.sh", get(donate_script))
        .route("/health", get(health))
        .route("/v1/whoami", get(whoami))
        .route("/v1/provider/connect", post(connect_account_legacy))
        .route(
            "/v1/provider/connect/codex/local",
            post(connect_codex_local),
        )
        .route(
            "/v1/provider/connect/codex/auth-json",
            post(connect_codex_auth_json),
        )
        .route(
            "/v1/provider/connect/claude/local",
            post(connect_claude_local),
        )
        .route(
            "/v1/provider/connect/claude/auth-json",
            post(connect_claude_auth_json),
        )
        .route("/v1/provider/connect/cursor", post(connect_cursor))
        .route(
            "/v1/provider/connect/cursor/local",
            post(connect_cursor_local),
        )
        .route("/v1/provider/connect/ollama", post(connect_ollama))
        .route("/v1/provider/connect/glm", post(connect_glm))
        .route(
            "/v1/provider/chains",
            get(get_chains).put(update_chains),
        )
        .route("/v1/provider/accounts", get(list_accounts))
        .route("/v1/provider/accounts/:id", delete(delete_account))
        .route("/v1/provider/accounts/:id/share", post(toggle_share))
        .route("/v1/apikeys", get(list_api_keys).post(create_api_key))
        .route("/v1/apikeys/:id", delete(delete_api_key))
        .route("/v1/stats", get(get_stats))
        .route("/v1/stats/capacity", get(get_capacity))
        .route("/v1/provider/models/codex", get(get_codex_models))
        .route("/v1/provider/models/claude", get(get_claude_models))
        .route("/v1/provider/models/cursor", get(get_cursor_models))
        .route("/v1/provider/models/ollama", get(get_ollama_models))
        .route("/v1/provider/models/glm", get(get_glm_models))
        .route("/v1/gateway/relay", post(relay))
        .route("/v1/client/codex/bootstrap", post(codex_bootstrap))
        .route("/v1/responses", post(proxy_responses))
        .route("/v1/messages", post(proxy_claude_messages))
        .route("/v1/chat/completions", post(proxy_chat_completions))
        .route("/backend-api/codex/responses", post(proxy_responses))
        .route("/backend-api/codex/ws", get(proxy_codex_ws))
        .route("/v1/models", get(proxy_models_openai))
        .route("/backend-api/codex/models", get(proxy_models_codex))
        .route("/backend-api/auth/session", get(mock_auth_session))
        .route("/backend-api/auth/refresh", post(mock_auth_refresh))
        .route("/v1/client/codex/apply", post(codex_apply))
        .route("/v1/client/codex/restore", post(codex_restore))
        .route("/v1/client/claude/apply", post(claude_apply))
        .route("/v1/client/claude/restore", post(claude_restore))
        .route("/v1/client/cursor/apply", post(cursor_apply))
        .route("/v1/client/cursor/restore", post(cursor_restore))
        .fallback(any(fallback_handler))
        // Cap inbound request bodies. Axum's built-in default is only 2 MiB,
        // which is too small for real transcripts+tools; raise it to a sane,
        // explicit ceiling that still bounds a memory-exhaustion attempt.
        .layer(DefaultBodyLimit::max(max_request_bytes()))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let bind = std::env::var("GATEWAY_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse().expect("invalid GATEWAY_BIND_ADDR");
    info!("org-ai-gateway listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind listener");
    // Graceful shutdown: on SIGTERM/SIGINT, stop accepting new connections and
    // let in-flight requests (including long-lived SSE/WS streams) finish instead
    // of being hard-cut on deploy/restart.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("failed to run axum server");
    info!("org-ai-gateway shut down cleanly");
}

/// Resolves when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM
/// (the signal `kill`/Docker/systemd send on stop). Either one starts a graceful
/// drain of in-flight requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!("failed installing SIGTERM handler: {}", e);
                // Never resolve, so only Ctrl-C triggers shutdown.
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received SIGINT, starting graceful shutdown"),
        _ = terminate => info!("received SIGTERM, starting graceful shutdown"),
    }
}

/// Maximum inbound request body size. Override with `GATEWAY_MAX_REQUEST_BYTES`.
fn max_request_bytes() -> usize {
    std::env::var("GATEWAY_MAX_REQUEST_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(64 * 1024 * 1024)
}

/// Spawn a never-ending background loop under supervision: if the loop's future
/// panics or returns, log it and restart after a short delay. `factory` rebuilds
/// the future each restart (the run_* loops capture `AppState`, so it just
/// re-clones state).
fn spawn_supervised<F, Fut>(name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(factory()).await {
                Ok(()) => {
                    warn!("background task `{}` returned unexpectedly; restarting in 1s", name);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Err(e) => {
                    error!("background task `{}` panicked: {}; restarting in 5s", name, e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    });
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "org_ai_gateway=info,tower_http=info".to_string()),
        )
        .compact()
        .init();
}
