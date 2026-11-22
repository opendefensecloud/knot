//! Change event hook: streams the UUIDs of changed pages to an external
//! WebSocket receiver (spec:
//! `docs/superpowers/specs/2026-11-22-change-event-hook-design.md`).
//!
//! Off unless `KNOT_CHANGE_EVENT_HOOK_URL` is set. Delivery is at most once.

mod coalesce;
mod conn;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What happened to a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Content,
    Created,
    Renamed,
    Moved,
    Archived,
    Restored,
    Purged,
}

/// One text frame on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Hello {
        instance_id: Uuid,
        version: &'static str,
    },
    PageChanged {
        doc_id: Uuid,
        workspace_id: Uuid,
        kind: ChangeKind,
        at: DateTime<Utc>,
    },
}

/// A lifecycle change as handlers report it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lifecycle {
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub kind: ChangeKind,
}

/// Configuration of the hook; `spawn` starts it.
#[derive(Debug, Clone)]
pub struct Hook {
    pub(crate) url: String,
    pub(crate) token: Option<String>,
    pub(crate) backoff_min: Duration,
    pub(crate) backoff_max: Duration,
    pub(crate) window: Duration,
    pub(crate) ping_interval: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) send_timeout: Duration,
    pub(crate) queue_capacity: usize,
}

impl Hook {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            window: Duration::from_secs(1),
            ping_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            send_timeout: Duration::from_secs(10),
            queue_capacity: 4096,
        }
    }
    pub fn token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }
    pub fn backoff(mut self, min: Duration, max: Duration) -> Self {
        self.backoff_min = min;
        self.backoff_max = max.max(min);
        self
    }
    pub fn window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }
    pub fn ping_interval(mut self, every: Duration) -> Self {
        self.ping_interval = every;
        self
    }
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
    pub fn send_timeout(mut self, d: Duration) -> Self {
        self.send_timeout = d;
        self
    }
    pub fn queue_capacity(mut self, n: usize) -> Self {
        self.queue_capacity = n.max(1);
        self
    }

    /// Start the hook's tasks. They end when `shutdown` is cancelled.
    pub fn spawn(
        self,
        docs: Arc<dyn knot_storage::DocStore>,
        shutdown: CancellationToken,
    ) -> ChangeEvents {
        let instance_id = Uuid::new_v4();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(self.queue_capacity);
        let (content_tx, content_rx) = mpsc::channel(self.queue_capacity);
        let (batch_tx, batch_rx) = mpsc::channel::<Vec<WireMessage>>(16);
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let auth = self.token.as_deref().and_then(|t| {
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|_| {
                    tracing::error!(
                        "change event hook: token is not a valid header value; sending none"
                    )
                })
                .ok()
        });
        let cfg = conn::ConnConfig {
            url: self.url,
            auth,
            instance_id,
            backoff_min: self.backoff_min,
            backoff_max: self.backoff_max,
            ping_interval: self.ping_interval,
            connect_timeout: self.connect_timeout,
            send_timeout: self.send_timeout,
            tls: conn::tls_config(),
        };
        tokio::spawn(coalesce::run(
            lifecycle_rx,
            content_rx,
            batch_tx,
            connected.clone(),
            docs,
            self.window,
            shutdown.clone(),
        ));
        tokio::spawn(conn::run(cfg, batch_rx, connected, shutdown));
        ChangeEvents {
            lifecycle_tx,
            content_tx,
            instance_id,
        }
    }
}

/// Handle to a running hook. Cheap to clone; emits never block.
#[derive(Debug, Clone)]
pub struct ChangeEvents {
    lifecycle_tx: mpsc::Sender<Lifecycle>,
    content_tx: mpsc::Sender<Uuid>,
    instance_id: Uuid,
}

impl ChangeEvents {
    pub fn emit(&self, workspace_id: Uuid, doc_id: Uuid, kind: ChangeKind) {
        match self.lifecycle_tx.try_send(Lifecycle {
            workspace_id,
            doc_id,
            kind,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => "queue_full")
                    .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
    pub fn emit_many(&self, workspace_id: Uuid, doc_ids: &[Uuid], kind: ChangeKind) {
        for &doc_id in doc_ids {
            self.emit(workspace_id, doc_id, kind);
        }
    }
    /// Sender for `knot_crdt::Rooms::with_local_change_tx`.
    pub fn content_sender(&self) -> mpsc::Sender<Uuid> {
        self.content_tx.clone()
    }
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }
}
