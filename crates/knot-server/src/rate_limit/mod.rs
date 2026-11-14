//! Rate limits on writes (issue #88).
//!
//! [`bucket`] holds the in-memory keyed token bucket every limiter uses:
//! self-pruning, key-capped, no background task. Buckets live in each
//! replica's memory; a user spread over N replicas gets up to N × a limit
//! (`rate_limit_scale` lets the operator divide).
//!
//! [`RateLimits`] is built from `state.config` in `router_with_state`, so
//! no separate wiring can be forgotten; the `AppState` constructors turn
//! limits off, so tests run unlimited unless they set the config first.

pub mod bucket;
pub mod collab;
pub mod http;

pub use bucket::{KeyedBuckets, Rate, Refusal, retry_after_secs};
pub use collab::{
    CollabKind, CollabLimits, ConnectionThrottle, Liveness, SessionSignals, WaitOutcome,
};
pub use http::{HttpBucket, HttpLimits};

use knot_config::Config;

/// Every rate limit of one replica: HTTP and collab WebSocket (not quotas
/// or import caps, which are always counted).
pub struct RateLimits {
    /// `rate_limit_enabled`: false turns every HTTP and WS rate limit off.
    pub enabled: bool,
    pub http: HttpLimits,
    pub collab: CollabLimits,
    /// Pings and the silence limit of collab sockets, so a vanished client
    /// doesn't hold a place under the socket cap; `None` (no pings, as
    /// before limits existed) when limits are off.
    pub liveness: Option<Liveness>,
}

impl RateLimits {
    /// The limits `config` asks for.
    pub fn from_config(config: &Config) -> Self {
        if !config.rate_limit_enabled {
            return Self::disabled();
        }
        Self {
            enabled: true,
            http: HttpLimits::new(effective_scale(config)),
            collab: CollabLimits::from_config(config),
            liveness: Some(Liveness::DEFAULT),
        }
    }

    /// No rate limit at all.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            http: HttpLimits::disabled(),
            collab: CollabLimits::disabled(),
            liveness: None,
        }
    }
}

/// `rate_limit_scale`, or 1 when it is out of range. Config validation
/// refuses such a value at startup, so only hand-built configs get here.
pub fn effective_scale(config: &Config) -> f64 {
    let scale = config.rate_limit_scale;
    if scale > 0.0 && scale <= 100.0 {
        scale
    } else {
        tracing::error!(scale, "rate_limit_scale is out of (0, 100]; using 1");
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_follows_the_enabled_switch() {
        let enabled = RateLimits::from_config(&Config::default());
        assert!(enabled.enabled);
        assert!(enabled.http.is_enabled());
        assert!(enabled.collab.is_enabled());
        assert_eq!(enabled.liveness, Some(Liveness::DEFAULT));
        let disabled = RateLimits::from_config(&Config {
            rate_limit_enabled: false,
            ..Default::default()
        });
        assert!(!disabled.enabled);
        assert!(!disabled.http.is_enabled());
        assert!(!disabled.collab.is_enabled());
        assert_eq!(disabled.liveness, None);
    }

    #[test]
    fn the_scale_comes_from_the_config() {
        let limits = RateLimits::from_config(&Config {
            rate_limit_scale: 0.5,
            ..Default::default()
        });
        assert_eq!(
            limits.http.rate(HttpBucket::Write),
            Some(Rate::per_minute(60, 60))
        );
    }

    #[test]
    fn an_out_of_range_scale_falls_back_to_one() {
        for scale in [0.0, -1.0, f64::NAN, 1000.0] {
            let config = Config {
                rate_limit_scale: scale,
                ..Default::default()
            };
            assert_eq!(effective_scale(&config), 1.0, "{scale}");
        }
    }
}
