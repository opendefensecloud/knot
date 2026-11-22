//! Tracing-subscriber initialisation.

use std::str::FromStr;

use ::tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("invalid log level: {0}")]
    Level(String),
    #[error("subscriber already initialised")]
    AlreadyInit,
}

/// Initialise the global tracing subscriber.
///
/// `level` is one of "trace"/"debug"/"info"/"warn"/"error".
/// `format` is "json" or "text".
///
/// Honours `RUST_LOG` if set (overrides the level argument); otherwise
/// uses the passed level as the floor.
pub fn init(level: &str, format: &str) -> Result<(), LoggingError> {
    let lvl = Level::from_str(level).map_err(|_| LoggingError::Level(level.into()))?;
    let filter = crate::filter::env_filter(&lvl.to_string());

    let registry = tracing_subscriber::registry().with(filter);
    match format {
        "json" => registry
            .with(fmt::layer().json())
            .try_init()
            .map_err(|_| LoggingError::AlreadyInit),
        _ => registry
            .with(fmt::layer())
            .try_init()
            .map_err(|_| LoggingError::AlreadyInit),
    }
}
