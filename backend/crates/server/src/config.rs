//! Environment-backed runtime configuration.
//!
//! Configuration is read exactly once, at startup, into a single [`Config`]
//! value. Anything that needs tuning at runtime reads it from [`crate::AppState`]
//! rather than reaching into the environment again.

use std::env;

use thiserror::Error;

/// Default TCP port the server binds when `PORT` is absent.
pub const DEFAULT_PORT: u16 = 8080;

/// Default log filter used when `RUST_LOG` is absent or blank.
pub const DEFAULT_RUST_LOG: &str = "info";

/// Problems encountered while loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// An environment variable is present but is not valid UTF-8.
    #[error("environment variable `{name}` is not valid unicode")]
    NotUnicode {
        /// Name of the offending variable.
        name: &'static str,
    },

    /// `PORT` was set but does not parse as a `u16`.
    #[error("environment variable `PORT` must be a valid port number, got `{value}`")]
    InvalidPort {
        /// The rejected raw value.
        value: String,
    },
}

/// Runtime configuration for the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// TCP port to bind.
    pub port: u16,
    /// Filter directives handed to `tracing-subscriber`.
    pub rust_log: String,
    /// PostgreSQL connection string. Accepted now, unused until later tickets,
    /// and never required for the server to start.
    pub database_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            rust_log: DEFAULT_RUST_LOG.to_owned(),
            database_url: None,
        }
    }
}

impl Config {
    /// Load configuration from the process environment.
    ///
    /// A `.env` file in the working directory is loaded first when one exists;
    /// its absence is not an error. Real environment variables always win over
    /// `.env`, because `dotenvy` does not overwrite already-set variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        drop(dotenvy::dotenv());
        Self::from_values(
            read_var("PORT")?,
            read_var("RUST_LOG")?,
            read_var("DATABASE_URL")?,
        )
    }

    /// Build a configuration from already-read values, falling back to defaults.
    ///
    /// Kept free of environment access so defaults and parsing are unit-testable.
    fn from_values(
        port: Option<String>,
        rust_log: Option<String>,
        database_url: Option<String>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();

        let port = match non_empty(port) {
            Some(raw) => raw
                .trim()
                .parse::<u16>()
                .map_err(|_| ConfigError::InvalidPort { value: raw })?,
            None => defaults.port,
        };

        Ok(Self {
            port,
            rust_log: non_empty(rust_log).unwrap_or(defaults.rust_log),
            database_url: non_empty(database_url),
        })
    }
}

/// Read an environment variable, distinguishing "absent" from "not unicode".
fn read_var(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode { name }),
    }
}

/// Treat a blank value as absent so that empty variables fall back to defaults.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, DEFAULT_PORT, DEFAULT_RUST_LOG};

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let config = Config::from_values(None, None, None).expect("defaults must load");

        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.rust_log, DEFAULT_RUST_LOG);
        assert_eq!(config.database_url, None);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn explicit_values_override_defaults() {
        let config = Config::from_values(
            Some("9000".to_owned()),
            Some("debug".to_owned()),
            Some("postgres://localhost/jiuyue".to_owned()),
        )
        .expect("explicit values must load");

        assert_eq!(config.port, 9000);
        assert_eq!(config.rust_log, "debug");
        assert_eq!(
            config.database_url.as_deref(),
            Some("postgres://localhost/jiuyue")
        );
    }

    #[test]
    fn blank_values_fall_back_to_defaults() {
        let config = Config::from_values(Some("  ".to_owned()), Some(String::new()), None)
            .expect("blank values must fall back to defaults");

        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.rust_log, DEFAULT_RUST_LOG);
        assert_eq!(config.database_url, None);
    }

    #[test]
    fn non_numeric_port_is_rejected() {
        let error = Config::from_values(Some("http".to_owned()), None, None)
            .expect_err("a non-numeric port must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidPort { ref value } if value == "http"
        ));
    }
}
