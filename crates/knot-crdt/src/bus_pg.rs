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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::{Bus, BusError, Subscription};

const PRESENCE_PAYLOAD_CAP_B64: usize = 6 * 1024;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Default)]
struct DocChannels {
    update_tx: Vec<mpsc::Sender<i64>>,
    presence_tx: Vec<mpsc::Sender<Vec<u8>>>,
}

#[derive(Clone)]
pub struct PgBus {
    // Swappable so the supervisor can replace the client after a reconnect.
    // The lock is only ever held to clone the Arc out — never across an await.
    client: Arc<Mutex<Arc<tokio_postgres::Client>>>,
    subscriptions: Arc<DashMap<Uuid, DocChannels>>,
}

impl PgBus {
    /// Connect a dedicated tokio_postgres client and spawn a supervised demux
    /// task. The initial connect is fail-fast; thereafter the supervisor
    /// reconnects with backoff and re-issues LISTEN for every active
    /// subscription, so a transient DB blip no longer permanently kills
    /// cross-pod fan-out.
    pub async fn connect(database_url: &str) -> Result<Self, BusError> {
        let config = database_url
            .parse::<tokio_postgres::Config>()
            .map_err(|e| BusError::Io(e.to_string()))?;
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| BusError::Io(e.to_string()))?;

        let client_slot = Arc::new(Mutex::new(Arc::new(client)));
        let subscriptions: Arc<DashMap<Uuid, DocChannels>> = Arc::new(DashMap::new());

        let slot = client_slot.clone();
        let subs = subscriptions.clone();
        tokio::spawn(async move {
            let mut next_conn = Some(connection);
            loop {
                let mut connection = next_conn.take().expect("connection present each iteration");

                // Drive THIS connection in a dedicated task so the client can
                // make progress (tokio_postgres requires the connection to be
                // polled). It returns when the connection errors or closes.
                let driver_subs = subs.clone();
                let driver = tokio::spawn(async move {
                    let stream = stream::poll_fn(|cx| connection.poll_message(cx));
                    tokio::pin!(stream);
                    while let Some(msg) = stream.next().await {
                        match msg {
                            Ok(tokio_postgres::AsyncMessage::Notification(n)) => {
                                Self::route(&driver_subs, n.channel(), n.payload());
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error=?e, "pg bus connection error");
                                break;
                            }
                        }
                    }
                });

                // The driver is now polling, so LISTEN executes resolve. On the
                // first pass `subs` is empty (no-op); after a reconnect this
                // re-subscribes every active doc so fan-out resumes.
                let client = slot.lock().unwrap().clone();
                for entry in subs.iter() {
                    let doc_id = *entry.key();
                    let _ = client
                        .execute(&format!("LISTEN \"doc:{doc_id}\""), &[])
                        .await;
                    let _ = client
                        .execute(&format!("LISTEN \"presence:{doc_id}\""), &[])
                        .await;
                }

                // Block until the connection dies.
                let _ = driver.await;
                tracing::warn!("pg bus connection lost; reconnecting");

                // Reconnect: try immediately, back off on repeated failure.
                let (new_client, new_conn) = loop {
                    match config.connect(tokio_postgres::NoTls).await {
                        Ok(cc) => break cc,
                        Err(e) => {
                            tracing::warn!(error=?e, "pg bus reconnect failed; retrying");
                            tokio::time::sleep(RECONNECT_BACKOFF).await;
                        }
                    }
                };
                *slot.lock().unwrap() = Arc::new(new_client);
                next_conn = Some(new_conn);
                tracing::info!("pg bus reconnected");
            }
        });

        Ok(Self {
            client: client_slot,
            subscriptions,
        })
    }

    /// Current client handle. The lock is held only long enough to clone the
    /// Arc — never across the subsequent await — so a reconnect swap never
    /// blocks publishers.
    fn current_client(&self) -> Arc<tokio_postgres::Client> {
        self.client.lock().unwrap().clone()
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
        // pg_notify() binds the channel + payload as parameters, so no SQL
        // string is built from values (defence-in-depth; both are internal).
        self.current_client()
            .execute(
                "SELECT pg_notify($1, $2)",
                &[&format!("doc:{doc_id}"), &seq.to_string()],
            )
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
        self.current_client()
            .execute(
                "SELECT pg_notify($1, $2)",
                &[&format!("presence:{doc_id}"), &encoded],
            )
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
            let client = self.current_client();
            client
                .execute(&format!("LISTEN \"doc:{doc_id}\""), &[])
                .await
                .map_err(|e| BusError::Io(e.to_string()))?;
            client
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
            let client = self.current_client();
            let _ = client
                .execute(&format!("UNLISTEN \"doc:{doc_id}\""), &[])
                .await;
            let _ = client
                .execute(&format!("UNLISTEN \"presence:{doc_id}\""), &[])
                .await;
        }
        Ok(())
    }
}
