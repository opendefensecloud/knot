//! Shared `EnvFilter` construction.

use tracing_subscriber::EnvFilter;

/// Build the `EnvFilter` for `level`, honouring `RUST_LOG` when it is set
/// and valid.
///
/// The `tungstenite` and `tokio_tungstenite` targets are capped at `debug`:
/// tungstenite logs the raw handshake request (including the change event
/// hook's bearer token) at `trace`. The cap never raises verbosity, and an
/// explicit directive for either target is honoured as written.
pub fn env_filter(level: &str) -> EnvFilter {
    let spec = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| EnvFilter::try_new(s).is_ok())
        .unwrap_or_else(|| level.to_string());
    EnvFilter::try_new(capped_spec(&spec)).unwrap_or_else(|_| EnvFilter::new(level))
}

const CAPPED: [&str; 2] = ["tungstenite", "tokio_tungstenite"];

fn capped_spec(spec: &str) -> String {
    let mut default_trace = false;
    let mut explicit = false;
    for d in spec.split(',').map(str::trim) {
        if !d.contains('=') {
            default_trace |= d.eq_ignore_ascii_case("trace");
        }
        let target = d.split(['=', '[']).next().unwrap_or("");
        explicit |= CAPPED
            .iter()
            .any(|c| target == *c || target.starts_with(&format!("{c}::")));
    }
    if default_trace && !explicit {
        format!("{spec},tungstenite=debug,tokio_tungstenite=debug")
    } else {
        spec.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{fmt, layer::SubscriberExt};

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Emit trace+debug events for tungstenite and knot, return the output.
    fn run(spec: &str) -> String {
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let w = buf.clone();
        let sub = tracing_subscriber::registry()
            .with(EnvFilter::try_new(capped_spec(spec)).unwrap())
            .with(fmt::layer().with_ansi(false).with_writer(move || w.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::trace!(target: "tungstenite::handshake::client", "TW_TRACE");
            tracing::debug!(target: "tungstenite::handshake::client", "TW_DEBUG");
            tracing::trace!(target: "tokio_tungstenite", "TT_TRACE");
            tracing::trace!(target: "knot_server", "KNOT_TRACE");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        out
    }

    #[test]
    fn trace_level_caps_tungstenite_at_debug() {
        let out = run("trace");
        assert!(
            !out.contains("TW_TRACE") && !out.contains("TT_TRACE"),
            "{out}"
        );
        assert!(
            out.contains("TW_DEBUG") && out.contains("KNOT_TRACE"),
            "{out}"
        );
    }

    #[test]
    fn info_level_does_not_enable_tungstenite_debug() {
        let out = run("info");
        assert!(!out.contains("TW_") && !out.contains("KNOT_TRACE"), "{out}");
        let out = run("info,knot_server=trace");
        assert!(!out.contains("TW_") && out.contains("KNOT_TRACE"), "{out}");
    }

    #[test]
    fn explicit_tungstenite_directive_is_honoured() {
        let out = run("trace,tungstenite=trace");
        assert!(out.contains("TW_TRACE"), "{out}");
    }
}
