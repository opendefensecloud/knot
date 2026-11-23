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
    let directives: Vec<&str> = spec.split(',').map(str::trim).collect();
    // A bare directive is the default level; "5" is tracing's numeric trace.
    let default_trace = directives
        .iter()
        .any(|d| !d.contains('=') && (d.eq_ignore_ascii_case("trace") || *d == "5"));
    if !default_trace {
        return spec.to_string();
    }
    // Cap each target on its own: an explicit directive for one of them
    // (honoured as written) must not leave the other at trace.
    let mut out = spec.to_string();
    for c in CAPPED {
        let explicit = directives.iter().any(|d| {
            let target = d.split(['=', '[']).next().unwrap_or("");
            target == c || target.starts_with(&format!("{c}::"))
        });
        if !explicit {
            out.push_str(&format!(",{c}=debug"));
        }
    }
    out
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
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
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
    fn explicit_directive_for_one_target_still_caps_the_other() {
        let out = run("trace,tokio_tungstenite=info");
        assert!(!out.contains("TW_TRACE"), "{out}");
        assert!(!out.contains("TT_TRACE"), "{out}");
        let out = run("trace,tungstenite=info");
        assert!(
            !out.contains("TT_TRACE") && !out.contains("TW_TRACE"),
            "{out}"
        );
    }

    #[test]
    fn numeric_trace_level_is_capped() {
        let out = run("5");
        assert!(
            !out.contains("TW_TRACE") && !out.contains("TT_TRACE"),
            "{out}"
        );
        assert!(out.contains("KNOT_TRACE"), "{out}");
    }

    #[test]
    fn explicit_tungstenite_directive_is_honoured() {
        let out = run("trace,tungstenite=trace");
        assert!(out.contains("TW_TRACE"), "{out}");
    }
}
