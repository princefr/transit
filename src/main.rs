use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;
use transit::api;
use transit::api::graphql::build_schema;
use transit::cache::TransitCache;
use transit::config::Config;
use transit::feeds::spawn_feed_supervisor;
use transit::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env before config (IDFM PRIM key, etc.). Missing file is fine.
    let dotenv_path = dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,transit=debug")),
        )
        .init();

    if let Some(ref path) = dotenv_path {
        info!(?path, "loaded .env");
    }
    if std::env::var("IDFM_PRIM_API_KEY").is_ok() {
        info!("IDFM_PRIM_API_KEY is set (value not logged)");
    }
    if std::env::var("DATASETS_API_KEY").is_ok() || std::env::var("DATAGOUV_API_KEY").is_ok() {
        info!("DATASETS_API_KEY/DATAGOUV_API_KEY is set (value not logged)");
    }

    let config = Config::load().map_err(|e| anyhow::anyhow!(e))?;
    std::fs::create_dir_all(&config.runtime.data_dir)?;

    info!(
        bind = %config.server.bind,
        feeds = config.enabled_feeds().count(),
        "starting transit"
    );

    let cache = TransitCache::connect(config.redis.clone()).await;
    let state = Arc::new(AppState::new(config.clone(), cache.clone()));

    let _supervisor = spawn_feed_supervisor(
        state.config.clone(),
        state.epoch.clone(),
        state.rt.clone(),
        state.rt_version_tx.clone(),
        cache,
    );

    // PRIM Navitia: disruptions + elevator outages (when IDFM_PRIM_API_KEY is set).
    let prim_cancel = tokio_util::sync::CancellationToken::new();
    let _prim = transit::prim::spawn_prim_poller(
        state.equipment.clone(),
        state.prim.clone(),
        state.rt.clone(),
        prim_cancel.clone(),
        90,
    );

    let schema = build_schema(state.clone());
    let app = api::router(state.clone(), schema)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address: {e}"))?;

    info!(%addr, "listening (GraphQL POST /graphql, WS /ws, health GET /health)");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
