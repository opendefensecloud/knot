//! Prometheus metrics exporter.

use std::net::SocketAddr;

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

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

/// `knot_history_replay_seconds` only: a page history replay may run up to
/// the builder's `REPLAY_TIMEOUT` (270 s), far past the latency buckets' 30 s.
const REPLAY_BUCKETS_SECONDS: [f64; 14] = [
    0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 180.0, 270.0, 300.0,
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
        .set_buckets_for_metric(
            Matcher::Full("knot_history_replay_seconds".into()),
            &REPLAY_BUCKETS_SECONDS,
        )
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
    describe_counter!(
        "knot_room_updates_refused_total",
        "Inbound CRDT updates a page or board room refused, closing their sender with 4422: ones that nest the document's types deeper than the room accepts (knot_crdt::nesting::MAX_NESTING), refused by the best-effort prefilter before applying or by the authoritative check after applying (also when yrs rejected the rest of the update), and ones that don't decode, of which nothing is applied. Only a refusal after applying evicts the room (see knot_room_evicted_total). An update yrs rejects part-way within the limit is not refused: as before, it is neither persisted nor fanned out, and its sender stays"
    );
    describe_counter!(
        "knot_room_evicted_total",
        "Rooms that evicted themselves after applying an inbound update that nested the document's types past knot_crdt::nesting::MAX_NESTING (the authoritative post-apply check), whether yrs applied all of the update or rejected the rest. The update was neither persisted nor fanned out, every connection of the room was closed (the sender with 4422, every other one with 1012) so its client reconnects, and the next join reloads the page from storage"
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

    describe_history_metrics();

    // Notifications
    describe_counter!(
        "knot_notifications_emitted_total",
        "Notifications written to the inbox, by kind"
    );

    // Export
    describe_counter!(
        "knot_export_total",
        "Page exports to DOCX/ODT, by format (docx|odt|unknown) and outcome (ok|client_error|busy|server_error); a request timed out or abandoned before its response counts as client_error (408)"
    );
    describe_histogram!(
        "knot_export_duration_seconds",
        Unit::Seconds,
        "Page export request duration, from the request to the written file, or to the request timeout or the client going away"
    );
    describe_counter!(
        "knot_export_fallback_total",
        "Content an export could not include as-is, by kind (diagram|image_unavailable|image_over_budget|unknown_element)"
    );

    // Markdown
    describe_counter!(
        "knot_markdown_unknown_nodes_total",
        "Nodes the schema doesn't name that a page's Markdown export (cache, search, tasks, GET markdown) wrote as their text"
    );
    describe_counter!(
        "knot_markdown_depth_limited_total",
        "Blockquotes, lists and tables nested past the Markdown writer's depth limit (128 block containers) that a page's Markdown export (cache, search, tasks, GET markdown) wrote as their text"
    );

    // Storage / pool
    describe_gauge!("knot_db_pool_size", "Total connections in the pool");
    describe_gauge!("knot_db_pool_idle", "Idle connections in the pool");

    Ok(())
}

/// Page history (M1.2's log and M1.3's builder).
fn describe_history_metrics() {
    describe_gauge!(
        "knot_history_table_bytes",
        Unit::Bytes,
        "On-disk size (heap, indexes and TOAST) of each table page history stores, by table; sampled every 10 minutes by each replica, one at a time under an advisory lock, so read it with max"
    );
    describe_gauge!(
        "knot_history_builder_lag_revs",
        "Largest head_rev - built_rev over pages whose history is not yet built and not given up; sampled by every replica's sweep, so read it with max"
    );
    describe_gauge!(
        "knot_history_given_up_pages",
        "Pages whose history the builder gave up on (they get no further history until a newer builder version retries them); sampled by every replica's sweep, so read it with max"
    );
    describe_gauge!(
        "knot_history_replay_threads_stalled",
        "Replay threads of this replica that timed out and still run, each holding a worker until it ends"
    );
    describe_counter!(
        "knot_history_rows_built_total",
        "Log rows the history builder replayed in written passes and geneses"
    );
    describe_counter!(
        "knot_history_rows_unapplied_total",
        "Log rows the history builder could not apply, by reason (apply_error|undecodable|skip_underflow|skipped); their content shows as gap entries where a later snapshot holds it"
    );
    describe_counter!(
        "knot_history_rows_parked_total",
        "Log rows whose content the history replay parked because a dependency had not arrived"
    );
    describe_counter!(
        "knot_history_gap_total",
        "Gap entries: content a room snapshot holds but no log row carries, recorded without an author"
    );
    describe_counter!(
        "knot_history_snapshots_unchecked_total",
        "Room snapshots the history builder left out of its gap check, by reason (refused|corrupt|undecodable|panic|timeout|too_large|kept_gap)"
    );
    describe_counter!(
        "knot_history_build_failures_total",
        "History builder passes that failed; their page is backed off and retried, and given up after repeated failures"
    );
    describe_counter!(
        "knot_history_replay_timeouts_total",
        "History replays that ran past their timeout, by phase (genesis|build|words)"
    );
    describe_counter!(
        "knot_history_pages_given_up_total",
        "Pages the history builder gave up on, by reason (failures|too_large|timeout|crash)"
    );
    describe_counter!(
        "knot_history_words_skipped_total",
        "History sessions left without a word count, by reason (too_large|diff_error|corrupt|gap_state|panic|timeout)"
    );
    describe_counter!(
        "knot_history_legacy_mismatch_total",
        "Pages whose rows from before page history did not match the count taken at the upgrade when they were numbered; the numbering is refused and logged"
    );
    describe_histogram!(
        "knot_history_replay_seconds",
        Unit::Seconds,
        "Compute time of one history replay, by phase (genesis|build|words)"
    );
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
    use std::sync::Mutex;

    use metrics::{
        Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };

    use super::{
        LATENCY_BUCKETS_SECONDS, REPLAY_BUCKETS_SECONDS, describe_history_metrics, normalize_addr,
    };

    #[test]
    fn shorthand_port() {
        assert_eq!(normalize_addr(":9090").unwrap(), "0.0.0.0:9090");
    }

    #[test]
    fn explicit_addr() {
        assert_eq!(normalize_addr("127.0.0.1:9090").unwrap(), "127.0.0.1:9090");
    }

    /// Keeps every description: (name, kind, unit).
    #[derive(Default)]
    struct Descriptions(Mutex<Vec<(String, &'static str, Option<Unit>)>>);

    impl Descriptions {
        fn push(&self, name: KeyName, kind: &'static str, unit: Option<Unit>) {
            let name = name.as_str().to_owned();
            self.0.lock().unwrap().push((name, kind, unit));
        }
    }

    impl Recorder for Descriptions {
        fn describe_counter(&self, name: KeyName, unit: Option<Unit>, _: SharedString) {
            self.push(name, "counter", unit);
        }
        fn describe_gauge(&self, name: KeyName, unit: Option<Unit>, _: SharedString) {
            self.push(name, "gauge", unit);
        }
        fn describe_histogram(&self, name: KeyName, unit: Option<Unit>, _: SharedString) {
            self.push(name, "histogram", unit);
        }
        fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
            Counter::noop()
        }
        fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }
        fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }

    #[test]
    fn history_metrics_are_described() {
        let rec = Descriptions::default();
        metrics::with_local_recorder(&rec, describe_history_metrics);
        let mut got: Vec<(String, &str)> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(n, k, _)| (n.clone(), *k))
            .collect();
        got.sort();
        let mut want = vec![
            ("knot_history_table_bytes", "gauge"),
            ("knot_history_builder_lag_revs", "gauge"),
            ("knot_history_given_up_pages", "gauge"),
            ("knot_history_replay_threads_stalled", "gauge"),
            ("knot_history_rows_built_total", "counter"),
            ("knot_history_rows_unapplied_total", "counter"),
            ("knot_history_rows_parked_total", "counter"),
            ("knot_history_gap_total", "counter"),
            ("knot_history_snapshots_unchecked_total", "counter"),
            ("knot_history_build_failures_total", "counter"),
            ("knot_history_replay_timeouts_total", "counter"),
            ("knot_history_pages_given_up_total", "counter"),
            ("knot_history_words_skipped_total", "counter"),
            ("knot_history_legacy_mismatch_total", "counter"),
            ("knot_history_replay_seconds", "histogram"),
        ];
        want.sort();
        let want: Vec<(String, &str)> = want.into_iter().map(|(n, k)| (n.to_owned(), k)).collect();
        assert_eq!(got, want);

        let unit = |name: &str| {
            rec.0
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _, _)| n == name)
                .and_then(|(_, _, u)| *u)
        };
        assert_eq!(unit("knot_history_table_bytes"), Some(Unit::Bytes));
        assert_eq!(unit("knot_history_replay_seconds"), Some(Unit::Seconds));
    }

    #[test]
    fn replay_seconds_has_buckets_up_to_the_replay_timeout() {
        // knot_history::builder::REPLAY_TIMEOUT (LEASE − 30 s); knot-obs
        // does not depend on knot-history.
        const REPLAY_TIMEOUT_SECS: f64 = 270.0;
        assert!(
            REPLAY_BUCKETS_SECONDS.windows(2).all(|w| w[0] < w[1]),
            "{REPLAY_BUCKETS_SECONDS:?} is not ascending"
        );
        assert!(*REPLAY_BUCKETS_SECONDS.last().unwrap() >= REPLAY_TIMEOUT_SECS);
        assert!(*LATENCY_BUCKETS_SECONDS.last().unwrap() < REPLAY_TIMEOUT_SECS);
    }
}
