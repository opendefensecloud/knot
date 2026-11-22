//! Observability primitives for knot.
//!
//! - `logging::init` sets up `tracing-subscriber` with JSON or text output.
//! - `metrics::init` exposes Prometheus on a configurable port.
//! - `tracing::init_with_otlp` (optional) attaches an OpenTelemetry OTLP layer.
//!
//! The three modules are independent; a binary can opt into any subset.

pub mod filter;
pub mod logging;
pub mod metrics;
pub mod tracing;
