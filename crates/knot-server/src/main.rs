//! knot spike server binary.
//!
//! Plan 2 wires layered config + observability + Postgres pool.

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::process;

use knot_config::Config;

#[tokio::main]
async fn main() {
    // 1. Load config.
    let cfg = match Config::load(std::env::var("KNOT_CONFIG").ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {e}");
            process::exit(2);
        }
    };

    // 2. Init observability (logging + optional OTLP; metrics).
    let _otlp_provider = if cfg.tracing_enabled && !cfg.otlp_endpoint.is_empty() {
        match knot_obs::tracing::init_with_otlp(
            &cfg.log_level,
            &cfg.log_format,
            &cfg.otlp_endpoint,
            "knot-server",
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("tracing init: {e}");
                process::exit(2);
            }
        }
    } else {
        if let Err(e) = knot_obs::logging::init(&cfg.log_level, &cfg.log_format) {
            eprintln!("logging init: {e}");
            process::exit(2);
        }
        None
    };
    if let Err(e) = knot_obs::metrics::init(&cfg.metrics_addr) {
        tracing::warn!(error=?e, "metrics init failed; continuing without /metrics");
    }

    // 3. Connect to Postgres if configured.
    let pool = if !cfg.database_url.is_empty() {
        match knot_storage::connect(&cfg.database_url, 16).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!(error=?e, "database connect failed");
                process::exit(3);
            }
        }
    } else {
        tracing::warn!("KNOT_DATABASE_URL not set; running in-memory only");
        None
    };

    // 4. Build router.
    let state = match pool {
        Some(p) => knot_server::AppState::with_pool(p),
        None => knot_server::AppState::in_memory(),
    };
    let app = knot_server::router_with_state(state);

    // 5. Bind + serve.
    let listener = match tokio::net::TcpListener::bind(normalize_addr(&cfg.addr)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error=?e, addr=%cfg.addr, "bind failed");
            process::exit(4);
        }
    };
    tracing::info!(addr=%listener.local_addr().unwrap(), "listening");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error=?e, "serve failed");
        process::exit(5);
    }
}

fn normalize_addr(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix(':')
        && port.parse::<u16>().is_ok()
    {
        return format!("0.0.0.0:{port}");
    }
    addr.to_string()
}
