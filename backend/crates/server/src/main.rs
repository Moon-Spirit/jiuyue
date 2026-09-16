//! Thin binary entry point for the jiuyue backend server.

use std::net::SocketAddr;
use std::process::ExitCode;

use jiuyue_server::{AppState, Config, Error, app};
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

/// Load configuration, bind the listener and serve until shutdown.
async fn run() -> Result<(), Error> {
    let config = Config::from_env()?;
    init_logging(&config.rust_log)?;

    let state = AppState::new(config);
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
