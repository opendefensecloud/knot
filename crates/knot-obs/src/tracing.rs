//! OpenTelemetry OTLP exporter setup.
//!
//! API notes for opentelemetry-otlp 0.32.0 / opentelemetry_sdk 0.32.1:
//!
//! - `SpanExporter::builder().with_tonic().with_endpoint(…).build()` — unchanged.
//! - The SDK struct is `SdkTracerProvider` (renamed from `TracerProvider` in 0.28,
//!   which is why it no longer collides with the `trace::TracerProvider` trait).
//! - `TracerProviderBuilder::with_batch_exporter` takes only the exporter; the
//!   runtime argument was dropped, so the `rt-tokio` feature is only needed for
//!   the batch processor's own scheduling.
//! - `Resource::new(kvs)` is gone; build via `Resource::builder().with_attribute(…)`.
//! - `global::shutdown_tracer_provider()` is gone; call `provider.shutdown()` directly.
//! - The caller must hold the returned provider and call `shutdown()` on drop/exit.

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, thiserror::Error)]
pub enum TracingError {
    #[error("otlp: {0}")]
    Otlp(String),
    #[error("subscriber: {0}")]
    Subscriber(String),
}

/// Initialise the global tracing subscriber WITH an OTLP layer.
///
/// Call this **instead of** `logging::init` when OTLP is enabled.
/// Returns the `TracerProvider`; the caller must call `.shutdown()` at
/// process exit to flush any in-flight spans.
pub fn init_with_otlp(
    level: &str,
    format: &str,
    endpoint: &str,
    service_name: &str,
) -> Result<SdkTracerProvider, TracingError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TracingError::Otlp(e.to_string()))?;

    let resource = Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.to_string()))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(service_name.to_string());
    opentelemetry::global::set_tracer_provider(provider.clone());

    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer);

    match format {
        "json" => registry
            .with(tracing_subscriber::fmt::layer().json())
            .try_init()
            .map_err(|e| TracingError::Subscriber(e.to_string())),
        _ => registry
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .map_err(|e| TracingError::Subscriber(e.to_string())),
    }?;

    Ok(provider)
}

/// Flush and shut down the OTLP exporter.
///
/// Pass the `SdkTracerProvider` returned by `init_with_otlp`.
/// Any error is logged but not propagated (best-effort at shutdown time).
pub fn shutdown(provider: SdkTracerProvider) {
    if let Err(e) = provider.shutdown() {
        eprintln!("knot-obs: OTLP shutdown error: {e}");
    }
}
