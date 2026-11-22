//! Coalesces changes per `(doc_id, kind)` over a leading-edge window: the
//! window opens at the first pending change and closes `window` later.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use knot_storage::DocStore;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ChangeKind, Lifecycle, WireMessage};

/// The changes of the open window, in arrival order, one per (doc, kind).
pub(super) struct Pending {
    opened_at: Option<Instant>,
    order: Vec<(Uuid, ChangeKind)>,
    workspace: HashMap<(Uuid, ChangeKind), Option<Uuid>>,
}

impl Pending {
    pub(super) fn new() -> Self {
        Self {
            opened_at: None,
            order: Vec::new(),
            workspace: HashMap::new(),
        }
    }

    pub(super) fn push(
        &mut self,
        doc_id: Uuid,
        kind: ChangeKind,
        workspace_id: Option<Uuid>,
        now: Instant,
    ) {
        self.opened_at.get_or_insert(now);
        match self.workspace.get_mut(&(doc_id, kind)) {
            Some(known) => {
                if known.is_none() {
                    *known = workspace_id;
                }
            }
            None => {
                self.order.push((doc_id, kind));
                self.workspace.insert((doc_id, kind), workspace_id);
            }
        }
    }

    pub(super) fn deadline(&self, window: Duration) -> Option<Instant> {
        self.opened_at.map(|t| t + window)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(super) fn take(&mut self) -> Vec<(Uuid, ChangeKind, Option<Uuid>)> {
        self.opened_at = None;
        let workspace = std::mem::take(&mut self.workspace);
        std::mem::take(&mut self.order)
            .into_iter()
            .map(|key| (key.0, key.1, workspace.get(&key).copied().flatten()))
            .collect()
    }
}

fn dropped(reason: &'static str, n: usize) {
    if n > 0 {
        metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => reason)
            .increment(n as u64);
    }
}

/// Close the window: resolve workspaces of content changes (a page gone
/// by now, e.g. purged, is skipped) and hand the batch to the connection.
async fn flush(
    batch: Vec<(Uuid, ChangeKind, Option<Uuid>)>,
    out: &mpsc::Sender<Vec<WireMessage>>,
    connected: &AtomicBool,
    docs: &dyn DocStore,
) {
    if !connected.load(Ordering::Acquire) {
        dropped("disconnected", batch.len());
        return;
    }
    let at = chrono::Utc::now();
    let mut messages = Vec::with_capacity(batch.len());
    for (doc_id, kind, workspace_id) in batch {
        let workspace_id = match workspace_id {
            Some(ws) => ws,
            None => match docs.get(doc_id).await {
                Ok(Some(doc)) => doc.workspace_id,
                Ok(None) => {
                    tracing::debug!(%doc_id, "change event hook: page gone before flush; skipping");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = ?e, %doc_id, "change event hook: workspace lookup failed");
                    dropped("send_failed", 1);
                    continue;
                }
            },
        };
        messages.push(WireMessage::PageChanged {
            doc_id,
            workspace_id,
            kind,
            at,
        });
    }
    if messages.is_empty() {
        return;
    }
    let n = messages.len();
    match out.try_send(messages) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => dropped("queue_full", n),
        Err(mpsc::error::TrySendError::Closed(_)) => dropped("disconnected", n),
    }
}

pub(super) async fn run(
    mut lifecycle_rx: mpsc::Receiver<Lifecycle>,
    mut content_rx: mpsc::Receiver<Uuid>,
    out: mpsc::Sender<Vec<WireMessage>>,
    connected: Arc<AtomicBool>,
    docs: Arc<dyn DocStore>,
    window: Duration,
    shutdown: CancellationToken,
) {
    let mut pending = Pending::new();
    loop {
        let deadline = pending.deadline(window);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            Some(c) = lifecycle_rx.recv() => {
                pending.push(c.doc_id, c.kind, Some(c.workspace_id), Instant::now());
            }
            Some(doc_id) = content_rx.recv() => {
                pending.push(doc_id, ChangeKind::Content, None, Instant::now());
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                flush(pending.take(), &out, &connected, docs.as_ref()).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;
    use uuid::Uuid;

    use crate::change_events::ChangeKind;

    #[test]
    fn dedupes_per_doc_and_kind_and_keeps_order() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        let mut p = Pending::new();
        p.push(a, ChangeKind::Content, None, now);
        p.push(b, ChangeKind::Created, Some(Uuid::nil()), now);
        p.push(a, ChangeKind::Content, None, now);
        p.push(a, ChangeKind::Renamed, Some(Uuid::nil()), now);
        let kinds: Vec<_> = p.take().into_iter().map(|(d, k, _)| (d, k)).collect();
        assert_eq!(
            kinds,
            vec![
                (a, ChangeKind::Content),
                (b, ChangeKind::Created),
                (a, ChangeKind::Renamed)
            ]
        );
        assert!(p.is_empty());
    }

    #[test]
    fn window_opens_at_first_change() {
        let t0 = Instant::now();
        let w = Duration::from_millis(100);
        let mut p = Pending::new();
        assert_eq!(p.deadline(w), None);
        p.push(Uuid::new_v4(), ChangeKind::Content, None, t0);
        p.push(
            Uuid::new_v4(),
            ChangeKind::Content,
            None,
            t0 + Duration::from_millis(80),
        );
        assert_eq!(p.deadline(w), Some(t0 + w));
        p.take();
        assert_eq!(p.deadline(w), None, "take closes the window");
    }

    #[test]
    fn a_known_workspace_wins_over_none() {
        let a = Uuid::new_v4();
        let ws = Uuid::new_v4();
        let mut p = Pending::new();
        p.push(a, ChangeKind::Content, None, Instant::now());
        p.push(a, ChangeKind::Content, Some(ws), Instant::now());
        assert_eq!(p.take(), vec![(a, ChangeKind::Content, Some(ws))]);
    }
}
