//! Cross-replica fan-out abstraction.
//!
//! Updates carry only `(doc_id, seq)`; bytes stay in `doc_updates`.
//! Presence carries the payload inline (size-capped on emit by the room).

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum BusError {
    #[error("io: {0}")]
    Io(String),
    #[error("subscriber full")]
    SubscriberFull,
}

pub struct Subscription {
    pub updates: mpsc::Receiver<i64>,
    pub presence: mpsc::Receiver<Vec<u8>>,
}

#[async_trait]
pub trait Bus: Send + Sync + 'static {
    async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError>;
    async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError>;
    async fn subscribe(&self, doc_id: Uuid) -> Result<Subscription, BusError>;
    async fn unsubscribe(&self, doc_id: Uuid) -> Result<(), BusError>;
}
