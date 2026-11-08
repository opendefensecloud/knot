//! A `metrics::Recorder` that counts what is recorded and keeps each
//! gauge's last value; copied from `knot-crdt/src/test_metrics.rs`.
//!
//! Install it with `let _g = metrics::set_default_local_recorder(&rec);`.
//! The guard is thread-local, so only current-thread tests (`#[tokio::test]`,
//! `start_paused`) see what their spawned tasks record.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A counter's running total, or how many values a histogram recorded.
#[derive(Default)]
struct Total(AtomicU64);

impl metrics::CounterFn for Total {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    fn absolute(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

impl metrics::HistogramFn for Total {
    fn record(&self, _: f64) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A gauge's last value, as `f64` bits.
#[derive(Default)]
struct Last(AtomicU64);

impl metrics::GaugeFn for Last {
    fn increment(&self, value: f64) {
        let v = f64::from_bits(self.0.load(Ordering::Relaxed)) + value;
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    fn decrement(&self, value: f64) {
        self.increment(-value);
    }
    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// A metric's name and its labels, sorted.
type Key = (String, Vec<(String, String)>);

fn key(k: &metrics::Key) -> Key {
    let mut labels: Vec<(String, String)> = k
        .labels()
        .map(|l| (l.key().to_owned(), l.value().to_owned()))
        .collect();
    labels.sort();
    (k.name().to_owned(), labels)
}

/// Counts every counter by name and labels, and how many values each
/// histogram recorded.
#[derive(Default)]
pub struct CountingRecorder {
    counters: Mutex<HashMap<Key, Arc<Total>>>,
    histograms: Mutex<HashMap<Key, Arc<Total>>>,
    gauges: Mutex<HashMap<Key, Arc<Last>>>,
}

impl CountingRecorder {
    /// The total of counter `name` with exactly these labels (0 if never
    /// incremented).
    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        let mut labels: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        labels.sort();
        self.counters
            .lock()
            .unwrap()
            .get(&(name.to_owned(), labels))
            .map_or(0, |t| t.0.load(Ordering::Relaxed))
    }

    /// The last value gauge `name` (no labels) was set to, if any.
    pub fn gauge(&self, name: &str) -> Option<f64> {
        self.labeled_gauge(name, &[])
    }

    /// The last value gauge `name` with exactly these labels was set to, if
    /// any.
    pub fn labeled_gauge(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let mut labels: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        labels.sort();
        self.gauges
            .lock()
            .unwrap()
            .get(&(name.to_owned(), labels))
            .map(|g| f64::from_bits(g.0.load(Ordering::Relaxed)))
    }

    /// How many values histogram `name` with exactly these labels recorded.
    pub fn histogram(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        let mut labels: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        labels.sort();
        self.histograms
            .lock()
            .unwrap()
            .get(&(name.to_owned(), labels))
            .map_or(0, |t| t.0.load(Ordering::Relaxed))
    }

    /// How many values histogram `name` recorded, over all its labels.
    pub fn histogram_count(&self, name: &str) -> u64 {
        self.histograms
            .lock()
            .unwrap()
            .iter()
            .filter(|((n, _), _)| n == name)
            .map(|(_, t)| t.0.load(Ordering::Relaxed))
            .sum()
    }
}

impl metrics::Recorder for CountingRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, k: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let total = self
            .counters
            .lock()
            .unwrap()
            .entry(key(k))
            .or_default()
            .clone();
        metrics::Counter::from_arc(total)
    }
    fn register_gauge(&self, k: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        let last = self
            .gauges
            .lock()
            .unwrap()
            .entry(key(k))
            .or_default()
            .clone();
        metrics::Gauge::from_arc(last)
    }
    fn register_histogram(
        &self,
        k: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        let total = self
            .histograms
            .lock()
            .unwrap()
            .entry(key(k))
            .or_default()
            .clone();
        metrics::Histogram::from_arc(total)
    }
}
