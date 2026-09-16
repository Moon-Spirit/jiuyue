//! Thin binary entry point for the jiuyue backend server.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use jiuyue_auth::{AuthConfig, AuthService, InMemoryMailer};
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

    // A build with no mail provider must not look like a working one: every
    // verification and reset link would only ever reach a log file. Refuse to
    // start rather than run a password-recovery flow that recovers nothing.
    if config.smtp_url.is_some() {
        return Err(Error::MailTransportUnsupported);
    }

    let store = Store::connect(config.require_database_url()?).await?;
    store.migrate().await?;

    // One pool, three domains: identity, chat, and the realtime gateway that
    // composes chat into the socket path. The hub shares the chat service rather
    // than opening its own, so a send and a history read hit the same tables, and
    // it takes the pool for the one table it owns itself — the durable last-seen
    // instant behind presence.
    let mut auth_config = AuthConfig::new(config.require_jwt_secret()?);
    auth_config.access_token_ttl = config.access_token_ttl;
    auth_config.refresh_token_ttl = config.refresh_token_ttl;
    auth_config.public_base_url = config.public_base_url.clone();
    auth_config.verification_token_ttl = config.verification_token_ttl;
    auth_config.reset_token_ttl = config.reset_token_ttl;

    // The development transport: it delivers nothing and logs each link, which is
    // how a local run "receives" mail. Production supplies a provider-backed
    // `Mailer` here instead — see
    // `docs/adr/0015-email-verification-and-the-mailer-seam.md`.
    let auth = AuthService::builder(store.pool().clone(), auth_config)
        .mailer(Arc::new(InMemoryMailer::new()))
        .build()
        .await?;

    tracing::warn!(
        "no mail provider configured: verification and password-reset emails are captured in the process log only"
    );
    let chat = Arc::new(ChatService::new(store.pool().clone()));
    let realtime = Arc::new(RealtimeHub::new(Arc::clone(&chat), store.pool().clone()));

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
