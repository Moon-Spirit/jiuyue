//! JiuYue backend entrypoint.

use axum::{routing::get, Router};

async fn healthz() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jiuyue_server=info,tower_http=info".into()),
        )
        .init();

    let app = Router::new().route("/healthz", get(healthz));
    let addr = std::env::var("JIUYUE_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind listener");
    tracing::info!("jiuyue-server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
