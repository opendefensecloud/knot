//! Prometheus metrics exporter.

use std::net::SocketAddr;

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("invalid address: {0}")]
    Address(String),
    #[error("install exporter: {0}")]
    Install(String),
}

/// Shared by every histogram (all of them are latencies in seconds). The
/// edges include each SLO threshold in docs/SLO.md so a target reads off a
/// bucket boundary instead of an interpolation.
const LATENCY_BUCKETS_SECONDS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 30.0,
];

/// Install the global metrics recorder and start the HTTP exporter
/// on `addr` (e.g. ":9090" or "0.0.0.0:9090").
pub fn init(addr: &str) -> Result<(), MetricsError> {
    let sa: SocketAddr = normalize_addr(addr)?
        .parse()
        .map_err(|e| MetricsError::Address(format!("{addr}: {e}")))?;

    // Without buckets the exporter renders every histogram as a summary, and
    // the dashboard, SLO queries and alert rule (`histogram_quantile` over
    // `_bucket`) all come back empty.
    PrometheusBuilder::new()
        .with_http_listener(sa)
        .set_buckets(&LATENCY_BUCKETS_SECONDS)
        .map_err(|e| MetricsError::Install(e.to_string()))?
        .install()
        .map_err(|e| MetricsError::Install(e.to_string()))?;

    // HTTP layer
    describe_counter!(
        "knot_http_requests_total",
        "HTTP requests handled, labeled by method, route, status_class"
    );
    describe_histogram!(
        "knot_http_request_duration_seconds",
        Unit::Seconds,
        "HTTP request duration"
    );

    // CRDT rooms
    describe_gauge!("knot_room_active", "Currently loaded rooms in this process");
    describe_gauge!(
        "knot_board_room_active",
        "Currently loaded Excalidraw board rooms in this process"
    );
    describe_counter!(
        "knot_room_updates_total",
        "CRDT updates applied to rooms, by source (local|peer)"
    );
    describe_counter!("knot_room_snapshots_total", "Snapshots written to storage");
    describe_histogram!(
        "knot_room_hydrate_seconds",
        Unit::Seconds,
        "Loading a cold room: latest snapshot + replay of later updates"
    );
    describe_histogram!(
        "knot_collab_initial_sync_seconds",
        Unit::Seconds,
        "Collab socket upgraded -> initial document state queued to the client"
    );

    // Storage / pool
    describe_gauge!("knot_db_pool_size", "Total connections in the pool");
    describe_gauge!("knot_db_pool_idle", "Idle connections in the pool");

    Ok(())
}

fn normalize_addr(addr: &str) -> Result<String, MetricsError> {
    if let Some(port) = addr.strip_prefix(':')
        && port.parse::<u16>().is_ok()
    {
        return Ok(format!("0.0.0.0:{port}"));
    }
    Ok(addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_addr;

    #[test]
    fn shorthand_port() {
        assert_eq!(normalize_addr(":9090").unwrap(), "0.0.0.0:9090");
    }

    #[test]
    fn explicit_addr() {
        assert_eq!(normalize_addr("127.0.0.1:9090").unwrap(), "127.0.0.1:9090");
    }
}
