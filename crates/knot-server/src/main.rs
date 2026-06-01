//! knot spike server binary.

use std::process;

// mimalloc as the global allocator everywhere except Windows MSVC dev
// builds (where the C dep is fiddly and not needed for development).
// See Cargo.toml for the rationale (ARM friendliness, Nix build hygiene,
// multi-threaded throughput vs musl's default mallocng).
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let listener = match tokio::net::TcpListener::bind("0.0.0.0:3000").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind :3000 failed: {e}");
            process::exit(1);
        }
    };
    tracing::info!(addr=%listener.local_addr().unwrap(), "listening");
    if let Err(e) = axum::serve(listener, knot_server::router()).await {
        eprintln!("serve failed: {e}");
        process::exit(1);
    }
}
