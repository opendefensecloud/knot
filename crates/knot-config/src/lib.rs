//! Layered configuration loader.
//!
//! Precedence (lowest → highest): defaults < optional file < environment.
//! Environment variables are prefixed `KNOT_` and lowercased to match
//! the field names (e.g. `KNOT_ADDR` → `addr`).

use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("figment: {0}")]
    Figment(#[from] Box<figment::Error>),
    #[error("invalid: {0}")]
    Invalid(String),
}

// Provide a direct From<figment::Error> via boxing so the `?` operator works.
impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Figment(Box::new(e))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listen address for HTTP/WS (e.g. ":3000" or "127.0.0.1:3000").
    pub addr: String,
    /// "development" or "production". Affects strict-mode checks.
    pub env: String,
    /// External base URL (used for OIDC redirect URLs, links, etc.).
    pub base_url: String,
    /// Postgres connection string.
    pub database_url: String,
    /// Max Postgres connections in the pool, per replica. Budget against the
    /// server's `max_connections`: roughly `replicas * (this + 2)` (the pool
    /// plus one LISTEN/NOTIFY bus connection and one ACL-listener connection).
    pub db_max_connections: u32,
    /// HMAC key for CSRF token signing. Required in production.
    pub session_key: String,
    /// Filesystem path for blob storage (fs BlobStore impl).
    pub data_dir: String,

    /// Log level for the application: trace/debug/info/warn/error.
    pub log_level: String,
    /// Log format: "json" or "text".
    pub log_format: String,
    /// Listen address for the metrics endpoint.
    pub metrics_addr: String,
    /// Enable OpenTelemetry OTLP exporter.
    pub tracing_enabled: bool,
    /// OTLP endpoint when tracing is enabled.
    pub otlp_endpoint: String,

    /// CRDT snapshot trigger: N updates between snapshots.
    pub snapshot_every_n: u32,
    /// CRDT snapshot trigger: idle seconds before snapshotting.
    pub snapshot_idle_sec: u32,
    /// CRDT room eviction: idle seconds before unloading a room.
    pub room_idle_evict_sec: u32,

    /// Enable OIDC login.
    pub oidc_enabled: bool,
    /// OIDC issuer URL (e.g. `http://dex:5556/dex`).
    pub oidc_issuer: String,
    /// OIDC client id.
    pub oidc_client_id: String,
    /// OIDC client secret.
    pub oidc_client_secret: String,
    /// Redirect URL registered with the IdP.
    pub oidc_redirect_url: String,
    /// Auto-provision policy: `off`, `always`, `domain`, or `group`.
    pub oidc_auto_provision: String,
    /// Comma-separated list of allowed email domains (used by `domain` policy).
    pub oidc_allowed_domains: String,
    /// JSON map of OIDC group → workspace role (used by `group` policy).
    pub oidc_role_from_groups: String,

    /// Blob storage backend: "postgres" (default) or "s3".
    pub blob_backend: String,
    /// S3 bucket name (required when `blob_backend = "s3"`).
    pub s3_bucket: String,
    /// S3 endpoint URL (e.g. `https://s3.us-east-1.amazonaws.com` for native
    /// AWS, or `http://minio.local:9000` / R2 / Backblaze endpoints).
    /// Empty for native AWS S3.
    pub s3_endpoint: String,
    /// S3 region.
    pub s3_region: String,
    /// Optional key prefix (e.g. `knot/blobs`). Empty = bucket root.
    pub s3_prefix: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: ":3000".into(),
            env: "development".into(),
            base_url: "http://localhost:3000".into(),
            database_url: String::new(),
            db_max_connections: 16,
            session_key: String::new(),
            data_dir: "./data".into(),
            log_level: "info".into(),
            log_format: "json".into(),
            metrics_addr: ":9090".into(),
            tracing_enabled: false,
            otlp_endpoint: String::new(),
            snapshot_every_n: 200,
            snapshot_idle_sec: 30,
            room_idle_evict_sec: 300,
            oidc_enabled: false,
            oidc_issuer: String::new(),
            oidc_client_id: String::new(),
            oidc_client_secret: String::new(),
            oidc_redirect_url: String::new(),
            oidc_auto_provision: "off".into(),
            oidc_allowed_domains: String::new(),
            oidc_role_from_groups: String::new(),
            blob_backend: "postgres".into(),
            s3_bucket: String::new(),
            s3_endpoint: String::new(),
            s3_region: "us-east-1".into(),
            s3_prefix: String::new(),
        }
    }
}

impl Config {
    /// Load configuration with optional yaml file path.
    ///
    /// Precedence: defaults < file (if Some) < env (`KNOT_*`).
    pub fn load<P: AsRef<Path>>(file: Option<P>) -> Result<Self, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if let Some(path) = file {
            fig = fig.merge(Yaml::file(path.as_ref()));
        }
        let cfg: Config = fig.merge(Env::prefixed("KNOT_")).extract()?;

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.env == "production" && self.session_key.is_empty() {
            return Err(ConfigError::Invalid(
                "KNOT_SESSION_KEY is required when KNOT_ENV=production".into(),
            ));
        }
        // A short signing key trivially weakens CSRF/session HMACs. Enforce a
        // 32-byte floor whenever a key is set (empty is allowed only outside
        // production, handled above).
        if !self.session_key.is_empty() && self.session_key.len() < 32 {
            return Err(ConfigError::Invalid(format!(
                "KNOT_SESSION_KEY must be at least 32 bytes (got {})",
                self.session_key.len()
            )));
        }
        if self.db_max_connections == 0 {
            return Err(ConfigError::Invalid(
                "KNOT_DB_MAX_CONNECTIONS must be >= 1".into(),
            ));
        }
        if !matches!(
            self.log_level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            return Err(ConfigError::Invalid(format!(
                "invalid log_level: {}",
                self.log_level
            )));
        }
        if !matches!(self.log_format.as_str(), "json" | "text") {
            return Err(ConfigError::Invalid(format!(
                "invalid log_format: {}",
                self.log_format
            )));
        }
        if !matches!(
            self.oidc_auto_provision.as_str(),
            "off" | "always" | "domain" | "group"
        ) {
            return Err(ConfigError::Invalid(format!(
                "invalid oidc_auto_provision: {}",
                self.oidc_auto_provision
            )));
        }
        if self.oidc_enabled {
            for (name, value) in [
                ("oidc_issuer", &self.oidc_issuer),
                ("oidc_client_id", &self.oidc_client_id),
                ("oidc_redirect_url", &self.oidc_redirect_url),
            ] {
                if value.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "{} is required when oidc_enabled=true",
                        name
                    )));
                }
            }
        }
        if !self.oidc_role_from_groups.is_empty() {
            // Parse-check only; the route layer parses again at use.
            serde_json::from_str::<std::collections::HashMap<String, String>>(
                &self.oidc_role_from_groups,
            )
            .map_err(|e| {
                ConfigError::Invalid(format!("oidc_role_from_groups not valid JSON: {e}"))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn short_session_key_is_rejected() {
        let cfg = Config {
            session_key: "too-short".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn thirty_two_byte_session_key_is_accepted() {
        let cfg = Config {
            session_key: "0123456789abcdef0123456789abcdef".into(),
            ..Default::default()
        };
        assert_eq!(cfg.session_key.len(), 32);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn zero_pool_size_is_rejected() {
        let cfg = Config {
            db_max_connections: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }
}
