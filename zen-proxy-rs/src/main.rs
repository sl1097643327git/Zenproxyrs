#![allow(dead_code)]
#![recursion_limit = "256"]

mod admin;
mod collector;
mod config;
mod health;
mod lanes;
mod ledger;
mod opencode_headers;
mod pool;
mod provider;
mod proxy;
mod server;
mod sse;
mod state;
mod utils;
mod v4;

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::Json,
    routing::{any, get},
    Router,
};
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use tracing_subscriber::{prelude::*, reload, EnvFilter, Registry};

use collector::async_collector::AsyncCollector;
use collector::default::DefaultCollector;
use collector::export::JsonBackend;
use collector::DataCollector;
use lanes::LaneLimiter;
use pool::active::ActivePool;
use pool::dead::DeadPoolImpl;
use pool::dispatch::{AimdConfig, DispatchPool, NodeBudgetLimits};
use pool::global_budget::{GlobalBudgetConfig, GlobalBudgetRegistry};
use pool::manager::PoolManagerImpl;
use pool::ratelimited::RateLimitedPoolImpl;
use pool::{DeadPool, NodeRef, Pool, RateLimitedPool, ResultKind};
use provider::clash::ClashCoordinator;
use provider::webshare::WebShareProvider;
use state::AppState;
use v4::model::ModelRegistry;
use v4::model_discovery::DynamicModelRegistry;
use v4::model_probe_runner::run_dynamic_model_probe_once;

const DEFAULT_TOKIO_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

static LOG_RELOAD: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub(crate) fn set_log_level(level: &str) -> Result<(), &'static str> {
    let handle = LOG_RELOAD.get().ok_or("log reload not initialized")?;
    let new_filter = match level.to_lowercase().as_str() {
        "off" => EnvFilter::new("off"),
        "error" => EnvFilter::new("error"),
        "warn" => EnvFilter::new("warn"),
        "info" => EnvFilter::new("info"),
        "debug" => EnvFilter::new("debug"),
        "trace" => EnvFilter::new("trace"),
        _ => return Err("invalid log level, use: off/error/warn/info/debug/trace"),
    };
    handle
        .modify(|f| *f = new_filter)
        .map_err(|_| "reload failed")
}

async fn health_handler(State(st): State<Arc<AppState>>) -> Json<Value> {
    let uptime = st.startup_time.elapsed().as_secs();
    let pools = st.pool_manager.pool_stats();
    let backoff = st.upstream_health.is_backoff();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime,
        "pid": std::process::id(),
        "pools": {
            "dispatch": pools.dispatch_size,
            "active": pools.active_size,
            "ratelimited": pools.ratelimited_size,
            "dead": pools.dead_size,
            "total": pools.total(),
            "fuse": pools.fuse,
        },
        "upstream": { "backoff": backoff }
    }))
}

async fn index_handler() -> Json<Value> {
    Json(json!({"service": "zen-proxy-rs", "version": "0.2.0", "status": "ok"}))
}

async fn metrics_handler(State(st): State<Arc<AppState>>) -> String {
    let mut snapshot = st.collector.snapshot();
    let pools = st.pool_manager.pool_stats();
    snapshot.pools.dispatch_size = pools.dispatch_size;
    snapshot.pools.active_size = pools.active_size;
    snapshot.pools.ratelimited_size = pools.ratelimited_size;
    snapshot.pools.dead_size = pools.dead_size;
    snapshot.pools.pool_transitions = pools.pool_transitions;
    snapshot.pools.active_concurrency = pools.active_concurrency;
    snapshot.system.uptime_secs = st.startup_time.elapsed().as_secs();
    let backend = collector::export::PrometheusBackend;
    backend.encode(&snapshot)
}

async fn models_handler(State(st): State<Arc<AppState>>) -> Json<Value> {
    let cfg = st.config.read().unwrap();
    let data = if cfg.v4_model_registry_active() {
        let registry = v4::model::EffectiveModelRegistry::with_dynamic_allowlists(
            cfg.dynamic_model_public_mode,
            st.dynamic_models.snapshot(),
            cfg.dynamic_model_public_allowlist.clone(),
            cfg.dynamic_model_claudecode_compat_allowlist.clone(),
        );
        registry
            .public_models()
            .into_iter()
            .map(|model| json!({"id": model.id, "object": "model", "owned_by": "deepseek"}))
            .collect::<Vec<_>>()
    } else {
        vec![
            json!({"id": "deepseek-v4-flash", "object": "model", "owned_by": "deepseek"}),
            json!({"id": "deepseek-v4-pro", "object": "model", "owned_by": "deepseek"}),
        ]
    };
    Json(json!({
        "object": "list",
        "data": data
    }))
}

async fn model_detail_handler(
    State(st): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cfg = st.config.read().unwrap();
    let data = if cfg.v4_model_registry_active() {
        let registry = v4::model::EffectiveModelRegistry::with_dynamic_allowlists(
            cfg.dynamic_model_public_mode,
            st.dynamic_models.snapshot(),
            cfg.dynamic_model_public_allowlist.clone(),
            cfg.dynamic_model_claudecode_compat_allowlist.clone(),
        );
        match registry.resolve(&model_id) {
            Ok(model) => json!({
                "id": model.public_model,
                "object": "model",
                "owned_by": "deepseek",
                "upstream_id": model.upstream_model,
                "profile": model.compatibility_profile.as_str()
            }),
            Err(_) => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({"error":{"message":"model not found","type":"invalid_request_error","code":"model_not_found"}}),
                    ),
                ));
            }
        }
    } else {
        match model_id.as_str() {
            "deepseek-v4-flash" | "deepseek-v4-pro" => json!({
                "id": model_id,
                "object": "model",
                "owned_by": "deepseek"
            }),
            _ => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({"error":{"message":"model not found","type":"invalid_request_error","code":"model_not_found"}}),
                    ),
                ));
            }
        }
    };
    Ok(Json(data))
}

async fn discover_dynamic_models_once(state: &AppState) {
    let (enabled, url, timeout_secs, probe_enabled, probe_adapter_mode, probe_max_per_round) = {
        let cfg = state.config.read().unwrap();
        (
            cfg.dynamic_model_discovery_enabled,
            cfg.dynamic_model_discovery_url.clone(),
            cfg.probe_connect_timeout_secs.max(1),
            cfg.dynamic_model_probe_enabled,
            cfg.dynamic_model_probe_adapter_mode,
            cfg.dynamic_model_probe_max_per_round,
        )
    };
    if !enabled {
        return;
    }

    state.dynamic_models.record_attempt();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            state
                .dynamic_models
                .record_error(format!("failed to build discovery client: {err}"));
            return;
        }
    };

    let body = match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                state
                    .dynamic_models
                    .record_error(format!("discovery http status {status}"));
                return;
            }
            match resp.text().await {
                Ok(body) => body,
                Err(err) => {
                    state
                        .dynamic_models
                        .record_error(format!("failed to read discovery body: {err}"));
                    return;
                }
            }
        }
        Err(err) => {
            state
                .dynamic_models
                .record_error(format!("discovery request failed: {err}"));
            return;
        }
    };

    match state.dynamic_models.update_from_opencode_json(&body) {
        Ok(snapshot) => {
            tracing::info!(
                candidates = snapshot.candidate_total,
                ignored = snapshot.ignored_total,
                "dynamic model discovery updated candidate registry"
            );
            if probe_enabled {
                let planned = state.dynamic_models.probe_candidates(probe_max_per_round);
                tracing::info!(
                    planned = planned.len(),
                    max_per_round = probe_max_per_round,
                    adapter = %probe_adapter_mode,
                    "dynamic model probe scheduler selected candidate batch"
                );
                for model in planned {
                    match run_dynamic_model_probe_once(state, &model.id).await {
                        Ok(summary) => {
                            tracing::info!(
                                model = %summary.model_id,
                                attempted = summary.attempted_probe_names.len(),
                                passed = summary.passed_probe_names.len(),
                                final_state = ?summary.final_state,
                                adapter = %probe_adapter_mode,
                                "dynamic model probe completed"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                model = %model.id,
                                error = ?err,
                                adapter = %probe_adapter_mode,
                                "dynamic model probe failed"
                            );
                        }
                    }
                }
            }
        }
        Err(err) => state.dynamic_models.record_error(err),
    }
}

/// Fallback group resolution for clash mode: query the Clash API for its
/// routable Selector groups and pick the one at `idx` (reusing the last API
/// entry when there are more listeners than APIs). Used when neither
/// `CLASH_GROUP_NAMES` nor a usable `CLASH_CONFIG_FILE` is available.
async fn discover_group_for_instance(config: &config::Config, idx: usize, proxy: &str) -> String {
    let api = config
        .clash_api_urls
        .get(idx)
        .or_else(|| config.clash_api_urls.last());
    let secret = config
        .clash_api_secrets
        .get(idx)
        .or_else(|| config.clash_api_secrets.last());
    match api {
        Some(api) => match ClashCoordinator::discover_selector_groups(
            api,
            secret.map(String::as_str),
        )
        .await
        {
            Ok(groups) if !groups.is_empty() => groups
                .get(idx)
                .cloned()
                .unwrap_or_else(|| groups[0].clone()),
            Ok(_) => {
                tracing::warn!(
                    api,
                    proxy,
                    "no Selector group found on this API; using 'Proxy'"
                );
                "Proxy".to_string()
            }
            Err(err) => {
                tracing::warn!(
                    api,
                    proxy,
                    error = %err,
                    "group auto-discovery failed; using 'Proxy'"
                );
                "Proxy".to_string()
            }
        },
        None => "Proxy".to_string(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => { tracing::info!("received Ctrl+C, shutting down"); }
        _ = terminate => { tracing::info!("received SIGTERM, shutting down"); }
    }
}

fn main() {
    let worker_stack_bytes = std::env::var("TOKIO_WORKER_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_WORKER_STACK_BYTES)
        .max(DEFAULT_TOKIO_WORKER_STACK_BYTES);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(worker_stack_bytes)
        .build()
        .expect("failed to build tokio runtime");
    runtime.block_on(async_main());
}

async fn async_main() {
    let log_filter = EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into());
    let (log_filter, log_handle) = reload::Layer::new(log_filter);
    LOG_RELOAD.set(log_handle).ok();
    tracing_subscriber::registry()
        .with(log_filter)
        .with(tracing_subscriber::fmt::Layer::new())
        .init();

    let config = config::Config::from_env();
    let node_urls = config.load_nodes();

    tracing::info!(count = node_urls.len(), "loaded proxy nodes");

    // Clash mode: build a coordinator driving each Clash instance's internal
    // Selector group. Its proxy URLs (local mixed ports) are the zen-proxy nodes.
    let clash_coordinator = if config.node_provider_mode == config::NodeProviderMode::Clash
        && !config.clash_api_urls.is_empty()
    {
        // Resolve group names: an explicit CLASH_GROUP_NAMES wins; otherwise
        // try the mihomo config file (its `listeners` section maps each local
        // port to the *current* Selector group, so renames are picked up
        // automatically); as a last resort, auto-discover the Selector groups
        // exposed by each Clash API and assign them by index. The chosen
        // mapping is logged so you can verify it against your GUI bindings.
        let group_names = if config.clash_group_names.is_empty() {
            let mut discovered: Vec<String> = Vec::new();
            match &config.clash_config_file {
                Some(path) => {
                    match ClashCoordinator::discover_groups_from_config_file(
                        path,
                        &config.clash_proxy_urls,
                    ) {
                        Ok(groups) if !groups.is_empty() => {
                            tracing::info!(
                                file = path,
                                groups = ?groups,
                                "clash mode: group names from config file listeners (set CLASH_GROUP_NAMES to override)"
                            );
                            groups
                        }
                        Ok(_) => {
                            tracing::warn!(
                                file = path,
                                "clash config file has no listener port/proxy entries; falling back to API discovery"
                            );
                            for (idx, proxy) in config.clash_proxy_urls.iter().enumerate() {
                                discovered.push(
                                    discover_group_for_instance(&config, idx, proxy).await,
                                );
                            }
                            discovered
                        }
                        Err(err) => {
                            tracing::warn!(
                                file = path,
                                error = %err,
                                "clash config file parse failed; falling back to API discovery"
                            );
                            for (idx, proxy) in config.clash_proxy_urls.iter().enumerate() {
                                discovered.push(
                                    discover_group_for_instance(&config, idx, proxy).await,
                                );
                            }
                            discovered
                        }
                    }
                }
                None => {
                    for (idx, proxy) in config.clash_proxy_urls.iter().enumerate() {
                        discovered.push(discover_group_for_instance(&config, idx, proxy).await);
                    }
                    discovered
                }
            }
        } else {
            config.clash_group_names.clone()
        };

        match ClashCoordinator::from_config(
            &config.clash_api_urls,
            &config.clash_api_secrets,
            &config.clash_proxy_urls,
            &group_names,
            config.clash_switch_max_attempts,
            config.clash_invalid_ttl_secs,
        ) {
            Some(coord) => {
                tracing::info!(
                    instances = coord.instance_count(),
                    groups = ?group_names,
                    "clash provider mode: coordinator enabled"
                );
                // Startup: reconcile each Clash instance onto a distinct
                // internal node before serving traffic (a fresh core can leave
                // every group on the same default selection).
                coord.ensure_distinct_nodes().await;
                Some(coord)
            }
            None => {
                tracing::warn!(
                    "NODE_PROVIDER_MODE=clash but no valid CLASH_API_URLS/CLASH_PROXY_URLS pair; falling back to webshare nodes"
                );
                None
            }
        }
    } else {
        None
    };

    let _provider = Arc::new(WebShareProvider::new(node_urls.clone()));
    let mut dispatch = DispatchPool::new_with_options(
        NodeBudgetLimits {
            max_calls_per_window: config.node_max_calls_per_window,
            max_tokens_per_window: config.node_max_tokens_per_window,
            max_kb_per_window: config.node_max_kb_per_window,
            cooldown_secs: config.node_budget_cooldown_secs,
            window_secs: config.node_budget_window_secs,
            five_xx_break_threshold: config.node_5xx_break_threshold,
            five_xx_break_cooldown_secs: config.node_5xx_break_cooldown_secs,
            five_xx_probe_successes: config.node_5xx_probe_successes,
        },
        AimdConfig {
            min_concurrent: config.v43_node_min_concurrency,
            max_concurrent: config.v43_node_max_concurrency,
            success_step: config.v43_aimd_success_step,
            failure_percent: config.v43_aimd_failure_percent,
            slow_latency_ms: config.v43_aimd_slow_latency_ms,
        },
        config.v43_dispatch_shards,
    )
    .with_global_budget_fail_open(config.v43_global_budget_fail_open);
    if config.v43_global_budget_mode == config::GlobalBudgetMode::SyncRedis {
        if let Some(redis_url) = config.global_budget_redis_url.clone() {
            match GlobalBudgetRegistry::new(GlobalBudgetConfig {
                redis_url,
                instance_id: config.instance_id.clone(),
                window_secs: config.node_budget_window_secs,
                lease_ttl_secs: config.node_lease_ttl_secs,
                max_calls_per_window: config.node_max_calls_per_window,
                max_tokens_per_window: config.node_max_tokens_per_window,
                max_kb_per_window: config.node_max_kb_per_window,
                max_concurrent: 5,
                cooldown_secs: config.node_budget_cooldown_secs,
            }) {
                Ok(registry) => {
                    tracing::info!(instance_id = %config.instance_id, "global Redis budget registry enabled");
                    dispatch = dispatch.with_global_budget(registry);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "global Redis budget registry unavailable; using local budgets");
                }
            }
        }
    }
    let active = Arc::new(ActivePool::new());
    let ratelimited = Arc::new(RateLimitedPoolImpl::new());
    let dead = Arc::new(DeadPoolImpl::new());

    for url in &node_urls {
        let node = NodeRef::new(url.clone());
        let node_id = node.id.clone();
        dispatch.add(node);
        if config.preferred_proxy_urls.iter().any(|item| item == url) {
            for _ in 0..3 {
                dispatch.release_with_latency(&node_id, &ResultKind::Success(200), 100);
            }
        }
    }
    tracing::info!(count = node_urls.len(), "nodes added to dispatch pool");

    let session_pin_redis = std::env::var("CCP_SESSION_PIN_REDIS_URL")
        .ok()
        .or_else(|| config.global_budget_redis_url.clone());
    crate::pool::session_pin::configure(session_pin_redis.clone());
    if session_pin_redis.is_some() {
        tracing::info!("CCP session pin Redis backend enabled");
    } else {
        tracing::info!("CCP session pin using in-memory backend");
    }
    let reasoning_sidecar_redis = std::env::var("CCP_REASONING_SIDECAR_REDIS_URL")
        .ok()
        .or_else(|| session_pin_redis.clone())
        .or_else(|| config.global_budget_redis_url.clone());
    free_model_client_rs::session::reasoning_store::configure(reasoning_sidecar_redis.clone());
    if reasoning_sidecar_redis.is_some() {
        tracing::info!("CCP reasoning sidecar Redis backend enabled");
    } else {
        tracing::info!("CCP reasoning sidecar using in-memory backend");
    }

    let default_collector = Arc::new(DefaultCollector::new());
    {
        let json_backend = JsonBackend::new("/tmp/zen-proxy-snapshot.json");
        default_collector.set_backend(Box::new(json_backend));
    }
    let collector: Arc<dyn DataCollector> = if config.v43_async_collector_enabled {
        AsyncCollector::spawn(
            default_collector.clone(),
            config.v43_collector_queue_capacity,
        )
    } else {
        default_collector
    };

    let pool_manager = Arc::new(PoolManagerImpl::new(
        Arc::new(dispatch),
        active.clone(),
        ratelimited.clone(),
        dead.clone(),
        collector.clone(),
        config.upstream_base.clone(),
        config.upstream_api_key.clone(),
        config.probe_timeout_secs,
        config.connect_timeout(),
        config.request_timeout(),
        config.allow_direct_fallback,
    ));
    if let Some(coord) = &clash_coordinator {
        pool_manager.set_clash_coordinator(Some(coord.clone()));
    }
    for url in &node_urls {
        pool_manager.register_known_node(NodeRef::new(url.clone()));
    }

    let upstream_health = Arc::new(health::UpstreamHealth::new(1000));
    let lanes = Arc::new(LaneLimiter::from_config(&config));
    let dynamic_models = Arc::new(DynamicModelRegistry::new(
        config.dynamic_model_discovery_enabled,
        config.dynamic_model_discovery_url.clone(),
    ));

    let ledger = ledger::LedgerCounters::new();
    ledger.set_events_path(Some(config.ledger_events_path.clone()));

    let app_state = Arc::new(AppState {
        config: RwLock::new(config.clone()),
        pool_manager,
        dead_pool: dead.clone() as Arc<dyn DeadPool>,
        ratelimited_pool: ratelimited.clone() as Arc<dyn RateLimitedPool>,
        active_pool: active.clone() as Arc<dyn Pool>,
        collector,
        upstream_health,
        lanes,
        ledger,
        dynamic_models,
        startup_time: Instant::now(),
    });

    // Background: snapshot persist every 60s
    {
        let state = app_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                state.collector.persist();
            }
        });
    }

    // Background: side-channel opencode model discovery. This records only
    // candidate metadata for admin visibility; it does not alter /v1/models or
    // request model resolution.
    if config.dynamic_model_discovery_enabled {
        let state = app_state.clone();
        state.dynamic_models.set_worker_running(true);
        tokio::spawn(async move {
            discover_dynamic_models_once(&state).await;
            loop {
                let interval = {
                    let cfg = state.config.read().unwrap();
                    cfg.dynamic_model_discovery_interval_secs
                };
                tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
                discover_dynamic_models_once(&state).await;
            }
        });
    }

    // Background: adaptive Dead-pool + RateLimited-pool recovery.
    //
    // Rate-limited nodes MUST be retried on the same day (the pool's
    // date-based filter would otherwise leave them dead until midnight), so
    // this runs every minute. Clash mode rotates to a fresh internal node
    // before each probe.
    {
        let state = app_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                state.pool_manager.probe_dead_adaptive();
                state.pool_manager.probe_ratelimited_adaptive();
            }
        });
    }

    // Background: 5xx circuit-break recovery probe.
    //
    // Nodes that hit the consecutive-5xx circuit break are quarantined in a
    // cooldown state; this task probes them with a tiny "1+1" request every
    // node_5xx_probe_interval_ms and lifts the break after consecutive
    // successes. This avoids hammering a failing upstream with real traffic.
    {
        let state = app_state.clone();
        let interval_ms = config.node_5xx_probe_interval_ms.max(100);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;
                state.pool_manager.probe_five_xx_adaptive();
            }
        });
    }

    // SIGHUP hot-reload
    {
        #[cfg(unix)]
        let signal_state = app_state.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let state = signal_state;
                let Ok(mut stream) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                else {
                    tracing::error!("failed to install SIGHUP handler");
                    return;
                };
                loop {
                    stream.recv().await;
                    tracing::info!("SIGHUP received, reloading config from env");
                    *state.config.write().unwrap() = config::Config::from_env();
                }
            }
            #[cfg(not(unix))]
            std::future::pending::<()>().await;
        });
    }

    let request_body_limit = config
        .request_body_limit_mb
        .max(1)
        .saturating_mul(1024 * 1024);
    let app = Router::new()
        .merge(admin::admin_router())
        .route("/", get(index_handler))
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/v1/models/{model_id}", get(model_detail_handler))
        .route("/v1/{*path}", any(proxy::proxy_handler))
        .layer(DefaultBodyLimit::max(request_body_limit))
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    let addr = config.bind_addr();
    tracing::info!("starting on {}", addr);

    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket
        .bind(addr.parse::<std::net::SocketAddr>().unwrap())
        .unwrap();
    let listener = socket.listen(1024).unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}
