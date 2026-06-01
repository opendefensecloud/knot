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
    /// HMAC key for CSRF token signing. Required in production.
    pub session_key: String,
    /// Filesystem path for blob storage (fs BlobStore impl).
    pub data_dir: String,

    /// Log level for the application: trace/debug/info/warn/error.
    pub log_level: String,
    /// Log format: "json" or "text".
    pub log_format: String,
    /// Listen address for the metrics + pprof endpoints.
    pub metrics_addr: String,
    /// Enable OpenTelemetry OTLP exporter.
    pub tracing_enabled: bool,
    /// OTLP endpoint when tracing is enabled.
    pub otlp_endpoint: String,
    /// Enable pprof endpoints on the metrics port.
    pub pprof_enabled: bool,

    /// CRDT snapshot trigger: N updates between snapshots.
    pub snapshot_every_n: u32,
    /// CRDT snapshot trigger: idle seconds before snapshotting.
    pub snapshot_idle_sec: u32,
    /// CRDT room eviction: idle seconds before unloading a room.
    pub room_idle_evict_sec: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: ":3000".into(),
            env: "development".into(),
            base_url: "http://localhost:3000".into(),
            database_url: String::new(),
            session_key: String::new(),
            data_dir: "./data".into(),
            log_level: "info".into(),
            log_format: "json".into(),
            metrics_addr: ":9090".into(),
            tracing_enabled: false,
            otlp_endpoint: String::new(),
            pprof_enabled: false,
            snapshot_every_n: 200,
            snapshot_idle_sec: 30,
            room_idle_evict_sec: 300,
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
        Ok(())
    }
}
