//! Runs a replay on a thread of its own: a panic in yrs (some of its paths
//! panic, and a debug build panics on the skip underflow) becomes an error
//! instead of ending the process, and a replay that hangs (a release build
//! wraps the underflow, so an apply can loop) is abandoned after a timeout.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

/// The replay thread's stack: yrs deletes nested types recursively.
pub const REPLAY_STACK: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IsolateError {
    /// The replay panicked (or its thread could not be started); the
    /// message is the panic's.
    #[error("the replay panicked: {0}")]
    Panicked(String),
    /// The replay did not finish in time. Its thread runs on, abandoned.
    #[error("the replay timed out")]
    TimedOut,
}

/// Runs `f` on a new `knot-history-replay` thread with a [`REPLAY_STACK`]
/// stack, inside `catch_unwind`, and waits at most `timeout` for it.
///
/// The thread owns `guard` (the builder's worker permit) while it runs and
/// hands it back with the result. On a timeout the guard stays with the
/// thread and is dropped when the thread ends, so the guard bounds live
/// replay threads, abandoned ones included: the second value is `None`
/// then (and when the thread could not be started, which drops the guard).
pub async fn run_isolated<T: Send + 'static, G: Send + 'static>(
    guard: G,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> (Result<T, IsolateError>, Option<G>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("knot-history-replay".to_string())
        .stack_size(REPLAY_STACK)
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(f))
                .map_err(|payload| IsolateError::Panicked(panic_message(payload.as_ref())));
            // After a timeout nobody receives: the guard drops here, as the
            // thread ends.
            let _ = tx.send((result, guard));
        });
    if let Err(e) = spawned {
        return (
            Err(IsolateError::Panicked(format!(
                "the replay thread did not start: {e}"
            ))),
            None,
        );
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok((result, guard))) => (result, Some(guard)),
        Ok(Err(_)) => (
            Err(IsolateError::Panicked(
                "the replay thread ended without a result".to_string(),
            )),
            None,
        ),
        Err(_) => (Err(IsolateError::TimedOut), None),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_string()
    }
}
