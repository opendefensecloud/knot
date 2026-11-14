//! Test helper: what this thread logs at `info`, the default `log_level`.

use std::{
    io,
    sync::{Arc, Mutex},
};

use tracing::subscriber::DefaultGuard;

#[derive(Clone, Default)]
pub(crate) struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Captures until the guard drops. The subscriber is per thread, so the test
/// must run on the current-thread runtime, the `#[tokio::test]` default.
pub(crate) fn capture() -> (Logs, DefaultGuard) {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_writer(move || writer.clone())
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}
