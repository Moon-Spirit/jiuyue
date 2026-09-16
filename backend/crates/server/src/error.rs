//! Top-level error type for the server.
//!
//! Library code returns [`Result`] values built from this enum; the binary turns
//! a final error into a non-zero exit code. Nothing in this crate panics on a
//! recoverable condition.

use std::io;

use thiserror::Error;

use crate::config::ConfigError;

/// Errors that can prevent the server from running.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration could not be loaded.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The identity store could not be reached or migrated.
    #[error(transparent)]
    Store(#[from] jiuyue_store::StoreError),

    /// `SMTP_URL` was set, but this build ships no transactional-mail provider to
    /// deliver through it. Refusing to start is deliberate: the alternative is a
    /// process that accepts registrations and password resets while every link it
    /// "sends" lands in a log file.
    #[error(
        "SMTP_URL is set but this build has no transactional-mail provider; \
         unset it to use the development transport, or ship an SMTP-backed `Mailer`"
    )]
    MailTransportUnsupported,

    /// Identity could not be initialised (for example a rejected signing secret).
    #[error(transparent)]
    Auth(#[from] jiuyue_auth::AuthError),

    /// The listener could not bind to the requested address.
    #[error("failed to bind listener on `{address}`")]
    Bind {
        /// Address that was requested.
        address: String,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },

    /// The bound listener did not report its local address.
    #[error("failed to read the bound listener address")]
    ListenerAddress {
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },

    /// Logging could not be initialised with the configured filter.
    #[error("failed to initialize logging with filter `{filter}`: {reason}")]
    Logging {
        /// Filter string taken from configuration.
        filter: String,
        /// Human-readable failure reason.
        reason: String,
    },

    /// The HTTP server failed while running.
    #[error(transparent)]
    Serve(#[from] io::Error),
}
