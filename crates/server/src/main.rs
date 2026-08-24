//! JiuYue backend entrypoint.

use jiuyue_server::app::build_router;
use jiuyue_server::config::Config;
use jiuyue_server::state::AppState;
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jiuyue_server=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await?;
    // Idempotent: sqlx records applied versions in _sqlx_migrations.
    jiuyue_server::MIGRATOR.run(&pool).await?;
    let redis_client = redis::Client::open(config.redis_url.as_str())?;
    let redis = redis::aio::ConnectionManager::new(redis_client).await?;
    let state = AppState::new(pool, redis, config.jwt_secret);

    // Dev-only CORS: allow localhost origins (Vite dev server / Tauri shell).
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            match origin.to_str() {
                Ok(origin) => {
                    origin.starts_with("http://localhost") || origin.starts_with("http://127.0.0.1")
                }
                Err(_) => false,
            }
        }))
        .allow_methods(Any)
        .allow_headers(Any);

    let app = build_router(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("jiuyue-server listening on {}", config.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}
