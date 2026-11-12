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

/// The default `history_replay_max_bytes`: 4 MiB, set by page history
/// M1.3's replay spike (`cargo run --release -p knot-history --example
/// replay_throughput`). The cap is the largest power of two at most
/// min(the slower replay workload's throughput × 2 s, (512 MiB − 256 MiB
/// for the web server) ÷ (1 worker × F)), at most 32 MiB. The spike
/// measured 7 MiB/s for pastes (14 MiB in 2 s) and a doc-memory factor
/// F = 40: a pass whose input is mostly a checkpoint of interleaved
/// one-character typing, whose items never squash (one decoded checkpoint
/// byte takes 38 bytes of heap; rows and snapshots cost less, so mixed
/// inputs measured 23). Memory bounds the cap at 6.4 MiB, so one worker
/// takes up to 40 × 4 MiB = 160 MiB.
pub const DEFAULT_HISTORY_REPLAY_MAX_BYTES: u64 = 4 * 1024 * 1024;

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
    /// alike. Peak memory is up to about `history_workers` × 40 × this. Minimum
    /// 1 MiB.
    pub history_replay_max_bytes: u64,
    /// The share of one core each history worker may use on average: after
    /// each page it sleeps `compute × (1 / share − 1)`. In (0, 1].
    pub history_cpu_share: f64,

    /// Turns every HTTP and collab WebSocket rate limit on or off. Quotas
    /// and the per-import caps are separate and stay as configured.
    pub rate_limit_enabled: bool,
    /// Multiplies the burst and refill rate of every rate-limit bucket (the
    /// per-bucket numbers are constants in the server). Scaled values never
    /// drop below a floor, so no setting makes a wait last days. Buckets live
    /// in each replica's memory: with N replicas a user can get up to N × the
    /// limit, so divide by N for a hard ceiling. In (0, 100].
    pub rate_limit_scale: f64,
    /// Open collab WebSockets (doc and board) one user may hold on this
    /// replica; the next upgrade gets a 429. Minimum 1.
    pub rate_limit_collab_max_sockets: u32,
    /// Comma-separated IPs or CIDRs of reverse proxies whose
    /// `X-Forwarded-For` is believed (a bare IP is one address). Empty: the
    /// client is the TCP peer. List only the ingress controller's addresses;
    /// `0.0.0.0/0` and `::/0` are refused. A single address could arrive from
    /// the env as a number, hence `de_string_or_number`.
    #[serde(deserialize_with = "de_string_or_number")]
    pub trusted_proxies: String,
    /// Pages one user may create in the workspace (`documents.created_by`),
    /// archived ones included until purged. `0` turns the quota off.
    pub quota_pages_per_user: u64,
    /// Pages the workspace may hold, archived ones included until purged.
    /// `0` turns the quota off.
    pub quota_pages_per_workspace: u64,
    /// Bytes of files one user may upload (`blobs.created_by`), archived
    /// ones included until purged. `0` turns the quota off.
    pub quota_blob_bytes_per_user: u64,
    /// Bytes of files the workspace may hold, archived ones included until
    /// purged. Without a per-user quota, one account can use all of it. `0`
    /// turns the quota off.
    pub quota_blob_bytes_per_workspace: u64,
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
            rate_limit_enabled: true,
            rate_limit_scale: 1.0,
            rate_limit_collab_max_sockets: 64,
            trusted_proxies: String::new(),
            quota_pages_per_user: 0,
            quota_pages_per_workspace: 0,
            quota_blob_bytes_per_user: 0,
            quota_blob_bytes_per_workspace: 0,
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

    /// Parse `trusted_proxies` (comma-separated IPs or CIDRs) into networks.
    /// Entries are trimmed and empty ones skipped; a bare IP becomes a `/32`
    /// or `/128`, and an IPv4-mapped IPv6 address or network becomes its
    /// IPv4 form. Refused: a prefix length of 0 (it would let every client
    /// choose its own address), a mapped network shorter than /96, and host
    /// bits after the prefix (`10.0.0.5/8`). Empty when unset.
    pub fn trusted_proxies_list(&self) -> Result<Vec<ipnet::IpNet>, ConfigError> {
        self.trusted_proxies
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(parse_trusted_proxy)
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
        // Written so that NaN fails too.
        let rate_limit_scale_valid = self.rate_limit_scale > 0.0 && self.rate_limit_scale <= 100.0;
        if !rate_limit_scale_valid {
            return Err(ConfigError::Invalid(
                "KNOT_RATE_LIMIT_SCALE must be > 0 and <= 100".into(),
            ));
        }
        if self.rate_limit_collab_max_sockets == 0 {
            return Err(ConfigError::Invalid(
                "KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS must be >= 1".into(),
            ));
        }
        self.trusted_proxies_list()?;
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

/// One `trusted_proxies` entry: a CIDR (`10.0.0.0/8`, `fd00::/8`) or a bare
/// address. An IPv4-mapped IPv6 entry (`::ffff:10.0.0.0/104`) becomes its
/// IPv4 form (`10.0.0.0/8`), as peers are compared in IPv4. Refused: a /0
/// network, a mapped CIDR shorter than /96 (it isn't an IPv4 network) and
/// host bits after the prefix (`10.0.0.5/8`: which was meant, the address
/// or the network?). Errors name the env var and the entry.
fn parse_trusted_proxy(entry: &str) -> Result<ipnet::IpNet, ConfigError> {
    let invalid_entry = |reason: String| {
        ConfigError::Invalid(format!(
            "KNOT_TRUSTED_PROXIES entry {entry:?} is not valid: {reason}"
        ))
    };
    let network = if entry.contains('/') {
        let written_network = entry.parse::<ipnet::IpNet>().map_err(|parse_error| {
            invalid_entry(format!("{parse_error} (expected an IP or CIDR)"))
        })?;
        canonical_network(written_network).ok_or_else(|| {
            invalid_entry(
                "an IPv4-mapped IPv6 network needs a prefix of at least /96; \
                 write the IPv4 network instead"
                    .into(),
            )
        })?
    } else {
        let address = entry.parse::<std::net::IpAddr>().map_err(|parse_error| {
            invalid_entry(format!("{parse_error} (expected an IP or CIDR)"))
        })?;
        ipnet::IpNet::from(address.to_canonical())
    };
    if network.prefix_len() == 0 {
        return Err(invalid_entry(
            "a /0 network trusts every address, so any client could choose its own IP".into(),
        ));
    }
    if network.trunc() != network {
        return Err(invalid_entry(format!(
            "host bits are set after the /{prefix}; write the network {network_form}, \
             or the bare address for a single proxy",
            prefix = network.prefix_len(),
            network_form = network.trunc(),
        )));
    }
    Ok(network)
}

/// `network` with an IPv4-mapped IPv6 network (`::ffff:a.b.c.d/96+n`) as
/// the IPv4 network `a.b.c.d/n`; `None` for a mapped network shorter than
/// /96. Host bits are kept, for the caller to refuse.
fn canonical_network(network: ipnet::IpNet) -> Option<ipnet::IpNet> {
    const MAPPED_PREFIX_LEN: u8 = 96;
    let ipnet::IpNet::V6(v6_network) = network else {
        return Some(network);
    };
    let Some(mapped_address) = v6_network.addr().to_ipv4_mapped() else {
        return Some(network);
    };
    if v6_network.prefix_len() < MAPPED_PREFIX_LEN {
        return None;
    }
    let v4_network =
        ipnet::Ipv4Net::new(mapped_address, v6_network.prefix_len() - MAPPED_PREFIX_LEN)
            .expect("a /96..=/128 mapped prefix leaves a valid IPv4 prefix");
    Some(ipnet::IpNet::V4(v4_network))
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
    fn rate_limit_and_quota_defaults() {
        let defaults = Config::default();
        assert!(defaults.rate_limit_enabled);
        assert_eq!(defaults.rate_limit_scale, 1.0);
        assert_eq!(defaults.rate_limit_collab_max_sockets, 64);
        assert!(defaults.trusted_proxies.is_empty());
        assert!(
            defaults
                .trusted_proxies_list()
                .expect("empty parses")
                .is_empty()
        );
        assert_eq!(
            (
                defaults.quota_pages_per_user,
                defaults.quota_pages_per_workspace,
                defaults.quota_blob_bytes_per_user,
                defaults.quota_blob_bytes_per_workspace,
            ),
            (0, 0, 0, 0),
            "quotas are off by default"
        );
        let valid_defaults = Config {
            session_key: KEY.into(),
            ..Default::default()
        };
        assert!(valid_defaults.validate().is_ok());
    }

    #[test]
    fn rate_limit_scale_must_be_in_zero_hundred() {
        let with_scale = |scale: f64| Config {
            session_key: KEY.into(),
            rate_limit_scale: scale,
            ..Default::default()
        };
        for bad_scale in [0.0, -1.0, 100.5, f64::NAN, f64::INFINITY] {
            let error = with_scale(bad_scale).validate().unwrap_err().to_string();
            assert!(
                error.contains("KNOT_RATE_LIMIT_SCALE must be > 0 and <= 100"),
                "{bad_scale}: {error}"
            );
        }
        for good_scale in [0.01, 0.5, 1.0, 100.0] {
            assert!(with_scale(good_scale).validate().is_ok(), "{good_scale}");
        }
    }

    #[test]
    fn collab_max_sockets_must_be_at_least_one() {
        let with_sockets = |sockets: u32| Config {
            session_key: KEY.into(),
            rate_limit_collab_max_sockets: sockets,
            ..Default::default()
        };
        let error = with_sockets(0).validate().unwrap_err().to_string();
        assert!(
            error.contains("KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS must be >= 1"),
            "{error}"
        );
        assert!(with_sockets(1).validate().is_ok());
    }

    fn with_trusted_proxies(trusted_proxies: &str) -> Config {
        Config {
            session_key: KEY.into(),
            trusted_proxies: trusted_proxies.into(),
            ..Default::default()
        }
    }

    #[test]
    fn trusted_proxies_list_parses_nets_and_bare_addresses() {
        let config = with_trusted_proxies("10.0.0.0/8, 192.168.1.5 ,fd00::/8");
        let parsed_nets = config.trusted_proxies_list().expect("parses");
        let expected_nets: Vec<ipnet::IpNet> = ["10.0.0.0/8", "192.168.1.5/32", "fd00::/8"]
            .iter()
            .map(|net| net.parse().unwrap())
            .collect();
        assert_eq!(parsed_nets, expected_nets);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn trusted_proxies_bare_ipv6_becomes_a_single_address() {
        let parsed_nets = with_trusted_proxies("fd00::1")
            .trusted_proxies_list()
            .expect("parses");
        assert_eq!(
            parsed_nets,
            vec!["fd00::1/128".parse::<ipnet::IpNet>().unwrap()]
        );
    }

    #[test]
    fn trusted_proxies_bare_mapped_ipv6_becomes_ipv4() {
        let parsed_nets = with_trusted_proxies("::ffff:10.0.0.7")
            .trusted_proxies_list()
            .expect("parses");
        assert_eq!(
            parsed_nets,
            vec!["10.0.0.7/32".parse::<ipnet::IpNet>().unwrap()]
        );
    }

    #[test]
    fn trusted_proxies_skips_empty_entries() {
        let parsed_nets = with_trusted_proxies(" , 10.1.2.3,, ")
            .trusted_proxies_list()
            .expect("parses");
        assert_eq!(
            parsed_nets,
            vec!["10.1.2.3/32".parse::<ipnet::IpNet>().unwrap()]
        );
        assert!(
            with_trusted_proxies(" ,, ")
                .trusted_proxies_list()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn trusted_proxies_refuses_networks_that_trust_everyone() {
        for everything in ["0.0.0.0/0", "::/0", "10.0.0.0/8, ::/0", "10.0.0.0/0"] {
            let config = with_trusted_proxies(everything);
            let list_error = config.trusted_proxies_list().unwrap_err().to_string();
            assert!(
                list_error.contains("KNOT_TRUSTED_PROXIES"),
                "{everything}: {list_error}"
            );
            let validate_error = config.validate().unwrap_err().to_string();
            assert!(
                validate_error.contains("KNOT_TRUSTED_PROXIES"),
                "{everything}: {validate_error}"
            );
        }
    }

    #[test]
    fn trusted_proxies_mapped_networks_become_ipv4() {
        let parsed_nets = with_trusted_proxies("::ffff:10.0.0.0/104, ::ffff:192.168.1.5/128")
            .trusted_proxies_list()
            .expect("parses");
        let expected_nets: Vec<ipnet::IpNet> = ["10.0.0.0/8", "192.168.1.5/32"]
            .iter()
            .map(|net| net.parse().unwrap())
            .collect();
        assert_eq!(parsed_nets, expected_nets);
        let ipv4_peer: std::net::IpAddr = "10.9.8.7".parse().unwrap();
        assert!(
            parsed_nets[0].contains(&ipv4_peer),
            "a mapped /104 matches 10.0.0.0/8 peers"
        );
    }

    #[test]
    fn trusted_proxies_refuses_a_mapped_network_that_trusts_everyone() {
        let config = with_trusted_proxies("::ffff:0:0/96");
        let list_error = config.trusted_proxies_list().unwrap_err().to_string();
        assert!(
            list_error.contains("KNOT_TRUSTED_PROXIES") && list_error.contains("/0 network"),
            "{list_error}"
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn trusted_proxies_refuses_mapped_networks_shorter_than_96() {
        for too_short in ["::ffff:10.0.0.0/95", "::ffff:0:0/80"] {
            let config = with_trusted_proxies(too_short);
            let list_error = config.trusted_proxies_list().unwrap_err().to_string();
            assert!(
                list_error.contains("KNOT_TRUSTED_PROXIES")
                    && list_error.contains(too_short)
                    && list_error.contains("/96"),
                "{too_short}: {list_error}"
            );
            assert!(config.validate().is_err(), "{too_short}");
        }
    }

    #[test]
    fn trusted_proxies_refuses_host_bits_and_suggests_the_network() {
        for (with_host_bits, network_form) in [
            ("10.0.0.5/8", "10.0.0.0/8"),
            ("fd00::1/8", "fd00::/8"),
            ("::ffff:10.0.0.5/104", "10.0.0.0/8"),
        ] {
            let config = with_trusted_proxies(with_host_bits);
            let list_error = config.trusted_proxies_list().unwrap_err().to_string();
            assert!(
                list_error.contains("KNOT_TRUSTED_PROXIES")
                    && list_error.contains(with_host_bits)
                    && list_error.contains(network_form),
                "{with_host_bits}: {list_error}"
            );
            assert!(config.validate().is_err(), "{with_host_bits}");
        }
    }

    #[test]
    fn trusted_proxies_refuses_malformed_entries() {
        for malformed in [
            "not-an-ip",
            "10.0.0.0/33",
            "10.0.0",
            "fd00::/129",
            "10.0.0.1:80",
        ] {
            let config = with_trusted_proxies(malformed);
            let list_error = config.trusted_proxies_list().unwrap_err().to_string();
            assert!(
                list_error.contains("KNOT_TRUSTED_PROXIES") && list_error.contains(malformed),
                "{malformed}: {list_error}"
            );
            assert!(config.validate().is_err(), "{malformed}");
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
