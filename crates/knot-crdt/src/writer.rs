//! Per-room writer task. Batches `doc_updates` inserts.
//!
//! Flush triggers (whichever first):
//!   - batch reaches 200 updates
//!   - 250 ms has elapsed since the first item in the batch
//!
//! Each successful insert returns one seq per input; the writer publishes
//! each seq over the bus and informs the room via `applied_tx` so the room
//! can advance `last_applied_seq` and fan out to its local conns.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::Bus;
use knot_storage::UpdatesStore;

pub const BATCH_MAX: usize = 200;
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Input the writer receives from the room.
pub struct PersistJob {
    pub bytes: Vec<u8>,
    pub by_user_id: Option<Uuid>,
    /// Optional confirmation channel. Fired with the assigned seq after
    /// the row is durably inserted (or with Err if the batch failed). Lets
    /// the room actor's handler await persistence before replying to its
    /// HTTP caller — needed for endpoints like PATCH /tasks where a 204
    /// can't be returned until we know the write survived a crash.
    pub persisted: Option<tokio::sync::oneshot::Sender<Result<i64, String>>>,
}

/// Output the writer sends back so the room can fan-out + track watermark.
pub struct Applied {
    pub seq: i64,
    pub bytes: Vec<u8>,
}

pub fn spawn(
    doc_id: Uuid,
    store: Arc<dyn UpdatesStore>,
    bus: Arc<dyn Bus>,
    mut rx: mpsc::Receiver<PersistJob>,
    applied_tx: mpsc::Sender<Applied>,
) {
    tokio::spawn(async move {
        let mut buf: Vec<PersistJob> = Vec::with_capacity(BATCH_MAX);
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                _ = sleep => {
                    if !buf.is_empty() {
                        flush(doc_id, &store, &bus, &applied_tx, &mut buf).await;
                        deadline = None;
                    }
                }
                Some(job) = rx.recv() => {
                    buf.push(job);
                    if buf.len() == 1 {
                        deadline = Some(tokio::time::Instant::now() + BATCH_INTERVAL);
                    }
                    if buf.len() >= BATCH_MAX {
                        flush(doc_id, &store, &bus, &applied_tx, &mut buf).await;
                        deadline = None;
                    }
                }
                else => break,
            }
        }
        if !buf.is_empty() {
            flush(doc_id, &store, &bus, &applied_tx, &mut buf).await;
        }
    });
}

async fn flush(
    doc_id: Uuid,
    store: &Arc<dyn UpdatesStore>,
    bus: &Arc<dyn Bus>,
    applied_tx: &mpsc::Sender<Applied>,
    buf: &mut Vec<PersistJob>,
) {
    let by_user = buf.first().and_then(|j| j.by_user_id);
    let updates: Vec<Vec<u8>> = buf.iter().map(|j| j.bytes.clone()).collect();
    match store.insert_batch(doc_id, by_user, &updates).await {
        Ok(seqs) => {
            for (seq, job) in seqs.into_iter().zip(buf.drain(..)) {
                if bus.publish(doc_id, seq).await.is_err() {
                    tracing::warn!(%doc_id, "bus publish failed; relying on catch-up tick");
                }
                let _ = applied_tx.try_send(Applied {
                    seq,
                    bytes: job.bytes,
                });
                if let Some(reply) = job.persisted {
                    let _ = reply.send(Ok(seq));
                }
            }
        }
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "writer flush failed; dropping batch (will reapply on next read)");
            for job in buf.drain(..) {
                if let Some(reply) = job.persisted {
                    let _ = reply.send(Err(format!("{e:?}")));
                }
            }
        }
    }
}
