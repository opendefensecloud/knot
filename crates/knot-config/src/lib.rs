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

/// Deserialize a `String` field that may arrive as a string OR an integer.
///
/// figment's `Env` provider parses bare-digit env values as integers, so a
/// numeric OIDC client id (e.g. the all-digit IDs Zitadel issues) would
/// otherwise fail with "invalid type: found unsigned int ..., expected a
/// string". Accept both and normalise to the canonical decimal string.
fn de_string_or_number<'de, D>(de: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Unsigned(u64),
        Signed(i64),
    }
    Ok(match StringOrNumber::deserialize(de)? {
        StringOrNumber::String(s) => s,
        StringOrNumber::Unsigned(n) => n.to_string(),
        StringOrNumber::Signed(n) => n.to_string(),
    })
}

/// The default `history_replay_max_bytes`: 8 MiB, set by page history
/// M1.3's replay spike (`cargo run --release -p knot-history --example
/// replay_throughput`). The cap is the largest power of two at most
/// min(the slower replay workload's throughput × 2 s, (512 MiB − 256 MiB
/// for the web server) ÷ (2 workers × F)), clamped to [8 MiB, 32 MiB]. The
/// spike measured 6.4 MiB/s for pastes (12.8 MiB in 2 s) and a doc-memory
/// factor F = 24 (a pass on a page of interleaved one-character typing,
/// whose items never squash), so memory bounds it at 5.3 MiB and the floor
/// applies: at F = 24 only one worker fits the chart's 512 Mi limit
/// (24 × 8 MiB = 192 MiB).
pub const DEFAULT_HISTORY_REPLAY_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listen address for HTTP/WS (e.g. ":3000" or "127.0.0.1:3000").
    pub addr: String,
    /// "development" or "production". Affects strict-mode checks.
    pub env: String,
    /// External base URL (used for OIDC redirect URLs, links, etc.).
    pub base_url: String,
    /// Set the `Secure` flag on auth cookies and emit HSTS. Default true;
    /// dev sets KNOT_COOKIE_SECURE=false to allow plain-HTTP localhost.
    pub cookie_secure: bool,
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
    #[serde(deserialize_with = "de_string_or_number")]
    pub oidc_client_id: String,
    /// OIDC client secret.
    #[serde(deserialize_with = "de_string_or_number")]
    pub oidc_client_secret: String,
    /// Redirect URL registered with the IdP.
    pub oidc_redirect_url: String,
    /// Auto-provision policy: `off`, `always`, `domain`, or `group`.
    pub oidc_auto_provision: String,
    /// Comma-separated list of allowed email domains (used by `domain` policy).
    pub oidc_allowed_domains: String,
    /// JSON map of OIDC group → workspace role (used by `group` policy).
    pub oidc_role_from_groups: String,
    /// Comma-separated list of regex patterns for additional `aud` values to
    /// trust in the ID token, beyond the client id. Each pattern is matched
    /// against the whole audience (a bare id matches only itself; `\d{18}`
    /// matches any 18-digit id). Some IdPs add extra audiences — Zitadel, for
    /// one, includes the project id (and sometimes more) alongside the client
    /// id — which the OIDC verifier rejects by default ("not a trusted
    /// audience"). A single all-digit value arrives from the env as an integer,
    /// hence `de_string_or_number` (same as the client id/secret).
    #[serde(deserialize_with = "de_string_or_number")]
    pub oidc_extra_audiences: String,

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

    /// Days a deleted doc stays in the archive (restorable) before the
    /// hourly purge job deletes it for good. Minimum 1.
    pub archive_retention_days: u32,

    /// Page history builder workers on this replica: each replays one page
    /// at a time on its own thread. `0` turns the builder and the history
    /// size sampler off on this replica.
    pub history_workers: u32,
    /// The most input bytes one history replay reads (a checkpoint or
    /// baseline, rows, gap states, snapshots), builds and word counts
    /// alike. Peak memory is about `history_workers` × 24 × this. Minimum
    /// 1 MiB.
    pub history_replay_max_bytes: u64,
    /// The share of one core each history worker may use on average: after
    /// each page it sleeps `compute × (1 / share − 1)`. In (0, 1].
    pub history_cpu_share: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: ":3000".into(),
            env: "development".into(),
            base_url: "http://localhost:3000".into(),
            cookie_secure: true,
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
            oidc_extra_audiences: String::new(),
            blob_backend: "postgres".into(),
            s3_bucket: String::new(),
            s3_endpoint: String::new(),
            s3_region: "us-east-1".into(),
            s3_prefix: String::new(),
            archive_retention_days: 30,
            history_workers: 1,
            history_replay_max_bytes: DEFAULT_HISTORY_REPLAY_MAX_BYTES,
            history_cpu_share: 0.5,
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

    /// Parse `oidc_extra_audiences` (comma-separated) into trimmed, non-empty
    /// audience strings. Empty when unset.
    pub fn oidc_extra_audiences_list(&self) -> Vec<String> {
        self.oidc_extra_audiences
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.session_key.is_empty() {
            return Err(ConfigError::Invalid(
                "KNOT_SESSION_KEY is required (set it in every environment)".into(),
            ));
        }
        // Cookies must carry the Secure flag in production (the .env.example
        // ships KNOT_COOKIE_SECURE=false for plain-HTTP dev; copying it to prod
        // would silently downgrade session cookies).
        if self.env == "production" && !self.cookie_secure {
            return Err(ConfigError::Invalid(
                "KNOT_COOKIE_SECURE must be true when KNOT_ENV=production".into(),
            ));
        }
        // A short signing key trivially weakens CSRF/session HMACs. Enforce a
        // 32-byte floor (the empty case is already rejected above, in all envs).
        if !self.session_key.is_empty() && self.session_key.len() < 32 {
            return Err(ConfigError::Invalid(format!(
                "KNOT_SESSION_KEY must be at least 32 bytes (got {})",
                self.session_key.len()
            )));
        }
        if self.archive_retention_days == 0 {
            return Err(ConfigError::Invalid(
                "KNOT_ARCHIVE_RETENTION_DAYS must be >= 1".into(),
            ));
        }
        if self.history_replay_max_bytes < 1024 * 1024 {
            return Err(ConfigError::Invalid(
                "KNOT_HISTORY_REPLAY_MAX_BYTES must be >= 1048576".into(),
            ));
        }
        // Written so that NaN fails too.
        let cpu_share_valid = self.history_cpu_share > 0.0 && self.history_cpu_share <= 1.0;
        if !cpu_share_valid {
            return Err(ConfigError::Invalid(
                "KNOT_HISTORY_CPU_SHARE must be > 0 and <= 1".into(),
            ));
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
    fn default_config_without_session_key_is_invalid() {
        // Config::default() has an empty session_key — must be rejected in all envs.
        assert!(Config::default().validate().is_err());
    }

    #[test]
    fn empty_session_key_is_rejected_in_development() {
        let cfg = Config {
            env: "development".into(),
            session_key: String::new(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn valid_session_key_is_accepted_in_development() {
        let cfg = Config {
            env: "development".into(),
            session_key: "0123456789abcdef0123456789abcdef".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
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
    fn archive_retention_defaults_to_30_days() {
        assert_eq!(Config::default().archive_retention_days, 30);
    }

    #[test]
    fn zero_archive_retention_is_rejected() {
        let cfg = Config {
            session_key: "0123456789abcdef0123456789abcdef".into(),
            archive_retention_days: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn zero_pool_size_is_rejected() {
        let cfg = Config {
            db_max_connections: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn insecure_cookies_rejected_in_production() {
        let cfg = Config {
            env: "production".into(),
            session_key: "0123456789abcdef0123456789abcdef".into(),
            cookie_secure: false,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn secure_cookies_accepted_in_production() {
        let cfg = Config {
            env: "production".into(),
            session_key: "0123456789abcdef0123456789abcdef".into(),
            cookie_secure: true,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn history_defaults() {
        let c = Config::default();
        assert_eq!(
            (c.history_workers, c.history_replay_max_bytes),
            (1, DEFAULT_HISTORY_REPLAY_MAX_BYTES)
        );
        assert_eq!(c.history_cpu_share, 0.5);
    }

    #[test]
    fn history_workers_zero_disables_the_builder() {
        let cfg = Config {
            session_key: KEY.into(),
            history_workers: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn replay_cap_below_one_mib_is_invalid() {
        let at = |bytes: u64| Config {
            session_key: KEY.into(),
            history_replay_max_bytes: bytes,
            ..Default::default()
        };
        let err = at(1_048_575).validate().unwrap_err().to_string();
        assert!(
            err.contains("KNOT_HISTORY_REPLAY_MAX_BYTES must be >= 1048576"),
            "{err}"
        );
        assert!(at(1_048_576).validate().is_ok());
    }

    #[test]
    fn cpu_share_must_be_in_zero_one() {
        assert_eq!(Config::default().history_cpu_share, 0.5);
        let at = |share: f64| Config {
            session_key: KEY.into(),
            history_cpu_share: share,
            ..Default::default()
        };
        for bad in [0.0, 1.5, -0.1, f64::NAN] {
            let err = at(bad).validate().unwrap_err().to_string();
            assert!(
                err.contains("KNOT_HISTORY_CPU_SHARE must be > 0 and <= 1"),
                "{bad}: {err}"
            );
        }
        assert!(at(1.0).validate().is_ok());
        assert!(at(0.01).validate().is_ok());
    }

    #[test]
    fn history_settings_parse_from_the_environment() {
        // SAFETY: single-threaded test; no concurrent env reads (nextest
        // runs each test in its own process).
        unsafe {
            std::env::set_var("KNOT_SESSION_KEY", KEY);
            std::env::set_var("KNOT_HISTORY_WORKERS", "0");
            std::env::set_var("KNOT_HISTORY_REPLAY_MAX_BYTES", "16777216");
            std::env::set_var("KNOT_HISTORY_CPU_SHARE", "0.25");
        }
        let cfg = Config::load(None::<&str>).expect("load");
        assert_eq!(cfg.history_workers, 0);
        assert_eq!(cfg.history_replay_max_bytes, 16 * 1024 * 1024);
        assert_eq!(cfg.history_cpu_share, 0.25);
        // SAFETY: same as above.
        unsafe {
            for k in [
                "KNOT_SESSION_KEY",
                "KNOT_HISTORY_WORKERS",
                "KNOT_HISTORY_REPLAY_MAX_BYTES",
                "KNOT_HISTORY_CPU_SHARE",
            ] {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn cookie_secure_defaults_true() {
        let cfg = Config::default();
        assert!(cfg.cookie_secure);
    }

    #[test]
    fn cookie_secure_env_false_parses_to_false() {
        // SAFETY: single-threaded test; no concurrent env reads.
        unsafe {
            std::env::set_var("KNOT_COOKIE_SECURE", "false");
            std::env::set_var("KNOT_SESSION_KEY", "0123456789abcdef0123456789abcdef");
        }
        let cfg = Config::load(None::<&str>).expect("load");
        assert!(!cfg.cookie_secure);
        // SAFETY: same as above.
        unsafe {
            std::env::remove_var("KNOT_COOKIE_SECURE");
            std::env::remove_var("KNOT_SESSION_KEY");
        }
    }
}
