//! Thin binary entry point for the jiuyue backend server.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use jiuyue_auth::{AuthConfig, AuthService};
use jiuyue_chat::ChatService;
use jiuyue_realtime::RealtimeHub;
use jiuyue_server::{AppState, Config, Error, Services, app};
use jiuyue_store::Store;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fatal: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Load configuration, connect the database, and serve until shutdown.
///
/// The order matters: configuration is validated (and a missing signing secret
/// rejected) before anything binds a socket, and migrations run before the server
/// accepts traffic, so a request never reaches a half-created schema.
async fn run() -> Result<(), Error> {
    let config = Config::from_env()?;
    init_logging(&config.rust_log)?;

    let store = Store::connect(config.require_database_url()?).await?;
    store.migrate().await?;

    // One pool, three domains: identity, chat, and the realtime gateway that
    // composes chat into the socket path. The hub shares the chat service rather
    // than opening its own, so a send and a history read hit the same tables.
    let auth = AuthService::new(
        store.pool().clone(),
        AuthConfig::new(config.require_jwt_secret()?),
    )
    .await?;
    let chat = Arc::new(ChatService::new(store.pool().clone()));
    let realtime = Arc::new(RealtimeHub::new(Arc::clone(&chat)));

    let state = AppState::with_services(
        config,
        Services {
            auth: Arc::new(auth),
            chat,
            realtime,
        },
    );
    let address = SocketAddr::from(([0, 0, 0, 0], state.config().port));

    let listener = TcpListener::bind(address)
        .await
        .map_err(|source| Error::Bind {
            address: address.to_string(),
            source,
        })?;

    let bound = listener
        .local_addr()
        .map_err(|source| Error::ListenerAddress { source })?;
    tracing::info!(%bound, "jiuyue-server listening");

    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("jiuyue-server stopped");
    Ok(())
}

/// Install a `tracing` subscriber that honours the configured filter.
fn init_logging(filter: &str) -> Result<(), Error> {
    let logging_error = |reason: String| Error::Logging {
        filter: filter.to_owned(),
        reason,
    };

    let env_filter =
        EnvFilter::try_new(filter).map_err(|error| logging_error(error.to_string()))?;

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init()
        .map_err(|error| logging_error(error.to_string()))?;

    Ok(())
}

/// Resolve when the process should begin draining connections (Ctrl-C).
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("shutdown signal received, draining connections"),
        Err(error) => tracing::error!(%error, "failed to listen for shutdown signal"),
    }
}
