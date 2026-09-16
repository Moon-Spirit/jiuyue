//! Environment-backed runtime configuration.
//!
//! Configuration is read exactly once, at startup, into a single [`Config`]
//! value. Anything that needs tuning at runtime reads it from [`crate::AppState`]
//! rather than reaching into the environment again.
//!
//! Two variables are **required**, and starting without them is a hard error
//! rather than a silent fallback:
//!
//! - `DATABASE_URL` — identity is database-backed; there is no in-memory mode.
//! - `JWT_SECRET` — a signing key that defaulted to something predictable would
//!   let anyone mint valid access tokens, so it must be supplied.
//!
//! [`Config::default`] exists for tests and produces a configuration with no
//! secrets; [`Config::from_env`] is the production path and rejects a missing one.

use std::env;
use std::time::Duration;

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

    /// A required environment variable is absent or blank.
    #[error("required environment variable `{name}` is not set")]
    Missing {
        /// Name of the missing variable.
        name: &'static str,
    },

    /// `PORT` was set but does not parse as a `u16`.
    #[error("environment variable `PORT` must be a valid port number, got `{value}`")]
    InvalidPort {
        /// The rejected raw value.
        value: String,
    },

    /// A duration variable was set but is not a whole number of seconds.
    #[error("environment variable `{name}` must be a whole number of seconds, got `{value}`")]
    InvalidDuration {
        /// Name of the offending variable.
        name: &'static str,
        /// The rejected raw value.
        value: String,
    },

    /// A URL variable was set but is not an absolute `http(s)` URL.
    #[error("environment variable `{name}` must be an absolute http(s) URL, got `{value}`")]
    InvalidUrl {
        /// Name of the offending variable.
        name: &'static str,
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
    /// PostgreSQL connection string. Required at startup; optional only for the
    /// test-friendly [`Config::default`].
    pub database_url: Option<String>,
    /// HS256 signing secret for access tokens. Required at startup; optional only
    /// for the test-friendly [`Config::default`].
    pub jwt_secret: Option<String>,
    /// Access-token lifetime, in seconds, from `ACCESS_TOKEN_TTL_SECS`.
    pub access_token_ttl: Duration,
    /// Refresh-token lifetime, in seconds, from `REFRESH_TOKEN_TTL_SECS`.
    pub refresh_token_ttl: Duration,
    /// Public origin the frontend is served from, from `APP_BASE_URL`. Used to
    /// build the links inside verification and reset emails.
    pub public_base_url: String,
    /// Verification-link lifetime, in seconds, from `EMAIL_VERIFICATION_TTL_SECS`.
    pub verification_token_ttl: Duration,
    /// Reset-link lifetime, in seconds, from `PASSWORD_RESET_TTL_SECS`.
    pub reset_token_ttl: Duration,
    /// Transactional-mail transport, from `SMTP_URL`. `None` means "no provider
    /// configured", which selects the development transport. The shipped build
    /// carries no provider implementation, so setting this makes startup fail
    /// rather than let the process pretend to deliver password-reset mail.
    pub smtp_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            rust_log: DEFAULT_RUST_LOG.to_owned(),
            database_url: None,
            jwt_secret: None,
            access_token_ttl: DEFAULT_ACCESS_TOKEN_TTL,
            refresh_token_ttl: DEFAULT_REFRESH_TOKEN_TTL,
            public_base_url: DEFAULT_PUBLIC_BASE_URL.to_owned(),
            verification_token_ttl: DEFAULT_VERIFICATION_TOKEN_TTL,
            reset_token_ttl: DEFAULT_RESET_TOKEN_TTL,
            smtp_url: None,
        }
    }
}

/// Default access-token lifetime (15 minutes).
pub const DEFAULT_ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// Default refresh-token lifetime (30 days).
pub const DEFAULT_REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default public origin for email links, taken from the identity module so the
/// two can never disagree (the Vite dev server's origin).
pub const DEFAULT_PUBLIC_BASE_URL: &str = jiuyue_auth::AuthConfig::DEFAULT_PUBLIC_BASE_URL;

/// Default verification-link lifetime, taken from the identity module (24 hours).
pub const DEFAULT_VERIFICATION_TOKEN_TTL: Duration =
    jiuyue_auth::AuthConfig::DEFAULT_VERIFICATION_TOKEN_TTL;

/// Default reset-link lifetime, taken from the identity module (1 hour).
pub const DEFAULT_RESET_TOKEN_TTL: Duration = jiuyue_auth::AuthConfig::DEFAULT_RESET_TOKEN_TTL;

/// Values read from the environment, before defaults and parsing.
#[derive(Debug, Default, Clone)]
struct RawConfig {
    port: Option<String>,
    rust_log: Option<String>,
    database_url: Option<String>,
    jwt_secret: Option<String>,
    access_token_ttl_secs: Option<String>,
    refresh_token_ttl_secs: Option<String>,
    app_base_url: Option<String>,
    verification_token_ttl_secs: Option<String>,
    password_reset_ttl_secs: Option<String>,
    smtp_url: Option<String>,
}

impl Config {
    /// Load configuration from the process environment.
    ///
    /// A `.env` file in the working directory is loaded first when one exists;
    /// its absence is not an error. Real environment variables always win over
    /// `.env`, because `dotenvy` does not overwrite already-set variables.
    ///
    /// Fails loudly when a required secret is absent — see the module docs.
    pub fn from_env() -> Result<Self, ConfigError> {
        drop(dotenvy::dotenv());
        Self::from_raw(RawConfig {
            port: read_var("PORT")?,
            rust_log: read_var("RUST_LOG")?,
            database_url: read_var("DATABASE_URL")?,
            jwt_secret: read_var("JWT_SECRET")?,
            access_token_ttl_secs: read_var("ACCESS_TOKEN_TTL_SECS")?,
            refresh_token_ttl_secs: read_var("REFRESH_TOKEN_TTL_SECS")?,
            app_base_url: read_var("APP_BASE_URL")?,
            verification_token_ttl_secs: read_var("EMAIL_VERIFICATION_TTL_SECS")?,
            password_reset_ttl_secs: read_var("PASSWORD_RESET_TTL_SECS")?,
            smtp_url: read_var("SMTP_URL")?,
        })
    }

    /// The database URL, or a configuration error when it was never supplied.
    pub fn require_database_url(&self) -> Result<&str, ConfigError> {
        self.database_url.as_deref().ok_or(ConfigError::Missing {
            name: "DATABASE_URL",
        })
    }

    /// The signing secret, or a configuration error when it was never supplied.
    pub fn require_jwt_secret(&self) -> Result<&str, ConfigError> {
        self.jwt_secret
            .as_deref()
            .ok_or(ConfigError::Missing { name: "JWT_SECRET" })
    }

    /// Build a configuration from already-read values.
    ///
    /// Kept free of environment access so defaults and parsing are unit-testable.
    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let defaults = Self::default();

        let port = match non_empty(raw.port) {
            Some(value) => value
                .trim()
                .parse::<u16>()
                .map_err(|_| ConfigError::InvalidPort { value })?,
            None => defaults.port,
        };

        Ok(Self {
            port,
            rust_log: non_empty(raw.rust_log).unwrap_or(defaults.rust_log),
            database_url: Some(required(raw.database_url, "DATABASE_URL")?),
            jwt_secret: Some(required(raw.jwt_secret, "JWT_SECRET")?),
            access_token_ttl: ttl(
                raw.access_token_ttl_secs,
                defaults.access_token_ttl,
                "ACCESS_TOKEN_TTL_SECS",
            )?,
            refresh_token_ttl: ttl(
                raw.refresh_token_ttl_secs,
                defaults.refresh_token_ttl,
                "REFRESH_TOKEN_TTL_SECS",
            )?,
            public_base_url: base_url(raw.app_base_url, defaults.public_base_url)?,
            verification_token_ttl: ttl(
                raw.verification_token_ttl_secs,
                defaults.verification_token_ttl,
                "EMAIL_VERIFICATION_TTL_SECS",
            )?,
            reset_token_ttl: ttl(
                raw.password_reset_ttl_secs,
                defaults.reset_token_ttl,
                "PASSWORD_RESET_TTL_SECS",
            )?,
            smtp_url: non_empty(raw.smtp_url).map(|value| value.trim().to_owned()),
        })
    }
}

/// Parse the public base URL, defaulting when blank and rejecting a value that
/// could never be a link origin.
///
/// A trailing slash is trimmed so `https://a.example/` and `https://a.example`
/// both build the same link prefix.
fn base_url(value: Option<String>, default: String) -> Result<String, ConfigError> {
    match non_empty(value) {
        Some(raw) => {
            let trimmed = raw.trim().trim_end_matches('/').to_owned();

            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                return Err(ConfigError::InvalidUrl {
                    name: "APP_BASE_URL",
                    value: raw,
                });
            }

            Ok(trimmed)
        }
        None => Ok(default),
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

/// Treat a blank value as absent so that empty variables count as missing.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|raw| !raw.trim().is_empty())
}

fn required(value: Option<String>, name: &'static str) -> Result<String, ConfigError> {
    non_empty(value).ok_or(ConfigError::Missing { name })
}

/// Parse an optional whole-second duration, falling back to the default.
///
/// A blank value means "use the default"; a non-numeric one is a hard error so a
/// typo cannot silently shorten or lengthen a credential's life.
fn ttl(
    value: Option<String>,
    default: Duration,
    name: &'static str,
) -> Result<Duration, ConfigError> {
    match non_empty(value) {
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| ConfigError::InvalidDuration { name, value: raw }),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, ConfigError, DEFAULT_ACCESS_TOKEN_TTL, DEFAULT_PORT, DEFAULT_PUBLIC_BASE_URL,
        DEFAULT_REFRESH_TOKEN_TTL, DEFAULT_RESET_TOKEN_TTL, DEFAULT_RUST_LOG,
        DEFAULT_VERIFICATION_TOKEN_TTL, RawConfig,
    };

    fn raw() -> RawConfig {
        RawConfig {
            database_url: Some("postgres://localhost/jiuyue".to_owned()),
            jwt_secret: Some("0123456789abcdef0123456789abcdef".to_owned()),
            ..RawConfig::default()
        }
    }

    #[test]
    fn defaults_apply_to_everything_optional() {
        let config = Config::from_raw(raw()).expect("required values are present");

        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.rust_log, DEFAULT_RUST_LOG);
        assert_eq!(config.access_token_ttl, DEFAULT_ACCESS_TOKEN_TTL);
        assert_eq!(config.refresh_token_ttl, DEFAULT_REFRESH_TOKEN_TTL);
    }

    #[test]
    fn explicit_values_override_defaults() {
        let config = Config::from_raw(RawConfig {
            port: Some("9000".to_owned()),
            rust_log: Some("debug".to_owned()),
            access_token_ttl_secs: Some("60".to_owned()),
            refresh_token_ttl_secs: Some("120".to_owned()),
            ..raw()
        })
        .expect("explicit values must load");

        assert_eq!(config.port, 9000);
        assert_eq!(config.rust_log, "debug");
        assert_eq!(config.access_token_ttl.as_secs(), 60);
        assert_eq!(config.refresh_token_ttl.as_secs(), 120);
    }

    #[test]
    fn blank_optional_values_fall_back_to_defaults() {
        let config = Config::from_raw(RawConfig {
            port: Some("  ".to_owned()),
            rust_log: Some(String::new()),
            ..raw()
        })
        .expect("blank optionals must fall back to defaults");

        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.rust_log, DEFAULT_RUST_LOG);
    }

    #[test]
    fn non_numeric_port_is_rejected() {
        let error = Config::from_raw(RawConfig {
            port: Some("http".to_owned()),
            ..raw()
        })
        .expect_err("a non-numeric port must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidPort { ref value } if value == "http"
        ));
    }

    #[test]
    fn a_non_numeric_ttl_is_rejected() {
        let error = Config::from_raw(RawConfig {
            access_token_ttl_secs: Some("soon".to_owned()),
            ..raw()
        })
        .expect_err("a non-numeric duration must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidDuration {
                name: "ACCESS_TOKEN_TTL_SECS",
                ref value,
            } if value == "soon"
        ));
    }

    #[test]
    fn a_missing_database_url_is_a_hard_error() {
        let error = Config::from_raw(RawConfig {
            database_url: None,
            ..raw()
        })
        .expect_err("DATABASE_URL must be required");

        assert!(matches!(
            error,
            ConfigError::Missing {
                name: "DATABASE_URL"
            }
        ));
    }

    #[test]
    fn a_missing_or_blank_signing_secret_is_a_hard_error() {
        for secret in [None, Some("   ".to_owned())] {
            let error = Config::from_raw(RawConfig {
                jwt_secret: secret,
                ..raw()
            })
            .expect_err("JWT_SECRET must be required");

            assert!(
                matches!(error, ConfigError::Missing { name: "JWT_SECRET" }),
                "a missing or blank secret must fail loudly, not default"
            );
        }
    }

    #[test]
    fn the_test_defaults_carry_no_secrets() {
        let config = Config::default();

        assert_eq!(config.database_url, None);
        assert_eq!(config.jwt_secret, None);
        assert!(config.require_database_url().is_err());
        assert!(config.require_jwt_secret().is_err());
    }

    #[test]
    fn email_settings_default_when_absent() {
        let config = Config::from_raw(raw()).expect("the required values are present");

        assert_eq!(config.public_base_url, DEFAULT_PUBLIC_BASE_URL);
        assert_eq!(
            config.verification_token_ttl,
            DEFAULT_VERIFICATION_TOKEN_TTL
        );
        assert_eq!(config.reset_token_ttl, DEFAULT_RESET_TOKEN_TTL);
        assert_eq!(
            config.smtp_url, None,
            "no provider configured means the development transport"
        );
    }

    #[test]
    fn explicit_email_settings_override_defaults_and_trim_the_origin() {
        let config = Config::from_raw(RawConfig {
            app_base_url: Some("  https://jiuyue.example/  ".to_owned()),
            verification_token_ttl_secs: Some("60".to_owned()),
            password_reset_ttl_secs: Some("120".to_owned()),
            smtp_url: Some("smtp://mail.example".to_owned()),
            ..raw()
        })
        .expect("explicit email settings must load");

        assert_eq!(
            config.public_base_url, "https://jiuyue.example",
            "a trailing slash must not survive into the link prefix"
        );
        assert_eq!(config.verification_token_ttl.as_secs(), 60);
        assert_eq!(config.reset_token_ttl.as_secs(), 120);
        assert_eq!(config.smtp_url.as_deref(), Some("smtp://mail.example"));
    }

    #[test]
    fn a_non_http_base_url_is_rejected() {
        let error = Config::from_raw(RawConfig {
            app_base_url: Some("jiuyue.example".to_owned()),
            ..raw()
        })
        .expect_err("a base URL without a scheme must be rejected");

        assert!(matches!(
            error,
            ConfigError::InvalidUrl {
                name: "APP_BASE_URL",
                ref value,
            } if value == "jiuyue.example"
        ));
    }

    #[test]
    fn a_blank_smtp_url_means_no_provider() {
        let config = Config::from_raw(RawConfig {
            smtp_url: Some("   ".to_owned()),
            ..raw()
        })
        .expect("a blank optional value must fall back to the default");

        assert_eq!(config.smtp_url, None);
    }
}
