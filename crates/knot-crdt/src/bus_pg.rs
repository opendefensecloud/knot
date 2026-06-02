//! Postgres LISTEN/NOTIFY Bus.
//!
//! One dedicated `tokio_postgres` connection per replica owns LISTEN for
//! every doc this replica has rooms for. Demuxes incoming Notifications
//! by channel name into per-doc mpsc senders.
//!
//! Channel naming:
//!   doc:<uuid>       — payload = "<seq>" as decimal text
//!   presence:<uuid>  — payload = url-safe base64 of bytes

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use futures_util::{StreamExt, stream};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::{Bus, BusError, Subscription};

const PRESENCE_PAYLOAD_CAP_B64: usize = 6 * 1024;

#[derive(Default)]
struct DocChannels {
    update_tx: Vec<mpsc::Sender<i64>>,
    presence_tx: Vec<mpsc::Sender<Vec<u8>>>,
}

#[derive(Clone)]
pub struct PgBus {
    client: Arc<tokio_postgres::Client>,
    subscriptions: Arc<DashMap<Uuid, DocChannels>>,
}

impl PgBus {
    /// Connect a dedicated tokio_postgres client and spawn the demux task.
    pub async fn connect(database_url: &str) -> Result<Self, BusError> {
        let config = database_url
            .parse::<tokio_postgres::Config>()
            .map_err(|e| BusError::Io(e.to_string()))?;
        let (client, mut connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;

        let subscriptions: Arc<DashMap<Uuid, DocChannels>> = Arc::new(DashMap::new());
        let demux_subs = subscriptions.clone();

        // Drive the connection AND surface notifications via a stream.
        tokio::spawn(async move {
            let stream = stream::poll_fn(move |cx| connection.poll_message(cx));
            tokio::pin!(stream);
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(tokio_postgres::AsyncMessage::Notification(n)) => {
                        Self::route(&demux_subs, n.channel(), n.payload());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error=?e, "pg bus connection error");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            client: Arc::new(client),
            subscriptions,
        })
    }

    fn route(subscriptions: &Arc<DashMap<Uuid, DocChannels>>, channel: &str, payload: &str) {
        if let Some(rest) = channel.strip_prefix("doc:") {
            let Ok(doc_id) = Uuid::parse_str(rest) else {
                return;
            };
            let Ok(seq) = payload.parse::<i64>() else {
                return;
            };
            if let Some(mut e) = subscriptions.get_mut(&doc_id) {
                e.update_tx.retain(|tx| tx.try_send(seq).is_ok());
            }
        } else if let Some(rest) = channel.strip_prefix("presence:") {
            let Ok(doc_id) = Uuid::parse_str(rest) else {
                return;
            };
            let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
                return;
            };
            if let Some(mut e) = subscriptions.get_mut(&doc_id) {
                e.presence_tx
                    .retain(|tx| tx.try_send(bytes.clone()).is_ok());
            }
        }
    }
}

#[async_trait]
impl Bus for PgBus {
    async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError> {
        // NOTIFY can't be parameterised; doc_id is internal Uuid, seq is i64
        // — neither can contain SQL-injection chars.
        let sql = format!("NOTIFY \"doc:{doc_id}\", '{seq}'");
        self.client
            .execute(&sql, &[])
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;
        Ok(())
    }

    async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError> {
        let encoded = URL_SAFE_NO_PAD.encode(&payload);
        if encoded.len() > PRESENCE_PAYLOAD_CAP_B64 {
            tracing::debug!(len = encoded.len(), "drop oversize presence frame");
            return Ok(());
        }
        let sql = format!("NOTIFY \"presence:{doc_id}\", '{encoded}'");
        self.client
            .execute(&sql, &[])
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;
        Ok(())
    }

    async fn subscribe(&self, doc_id: Uuid) -> Result<Subscription, BusError> {
        let (ut, ur) = mpsc::channel::<i64>(256);
        let (pt, pr) = mpsc::channel::<Vec<u8>>(256);
        let was_new = !self.subscriptions.contains_key(&doc_id);
        let mut entry = self.subscriptions.entry(doc_id).or_default();
        entry.update_tx.push(ut);
        entry.presence_tx.push(pt);
        drop(entry);
        if was_new {
            self.client
                .execute(&format!("LISTEN \"doc:{doc_id}\""), &[])
                .await
                .map_err(|e| BusError::Io(e.to_string()))?;
            self.client
                .execute(&format!("LISTEN \"presence:{doc_id}\""), &[])
                .await
                .map_err(|e| BusError::Io(e.to_string()))?;
        }
        Ok(Subscription {
            updates: ur,
            presence: pr,
        })
    }

    async fn unsubscribe(&self, doc_id: Uuid) -> Result<(), BusError> {
        let still_active = self
            .subscriptions
            .get(&doc_id)
            .map(|e| e.update_tx.iter().any(|t| !t.is_closed()))
            .unwrap_or(false);
        if !still_active {
            self.subscriptions.remove(&doc_id);
            let _ = self
                .client
                .execute(&format!("UNLISTEN \"doc:{doc_id}\""), &[])
                .await;
            let _ = self
                .client
                .execute(&format!("UNLISTEN \"presence:{doc_id}\""), &[])
                .await;
        }
        Ok(())
    }
}
