//! In-process Bus impl for unit tests. Subscribers receive every publish
//! after their subscription. Per-doc state lives in a DashMap.

use async_trait::async_trait;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::{Bus, BusError, Subscription};

#[derive(Default)]
struct DocChannels {
    update_tx: Vec<mpsc::Sender<i64>>,
    presence_tx: Vec<mpsc::Sender<Vec<u8>>>,
}

#[derive(Clone, Default)]
pub struct MemBus {
    map: Arc<DashMap<Uuid, DocChannels>>,
}

impl MemBus {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Bus for MemBus {
    async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError> {
        if let Some(mut entry) = self.map.get_mut(&doc_id) {
            entry.update_tx.retain(|tx| tx.try_send(seq).is_ok());
        }
        Ok(())
    }

    async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError> {
        if let Some(mut entry) = self.map.get_mut(&doc_id) {
            entry
                .presence_tx
                .retain(|tx| tx.try_send(payload.clone()).is_ok());
        }
        Ok(())
    }

    async fn subscribe(&self, doc_id: Uuid) -> Result<Subscription, BusError> {
        let (ut, ur) = mpsc::channel::<i64>(256);
        let (pt, pr) = mpsc::channel::<Vec<u8>>(256);
        let mut entry = self.map.entry(doc_id).or_default();
        entry.update_tx.push(ut);
        entry.presence_tx.push(pt);
        Ok(Subscription {
            updates: ur,
            presence: pr,
        })
    }

    async fn unsubscribe(&self, _doc_id: Uuid) -> Result<(), BusError> {
        // No-op for MemBus; channels close on drop.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn publish_reaches_subscribers() {
        let bus = MemBus::new();
        let doc = Uuid::new_v4();
        let mut sub = bus.subscribe(doc).await.unwrap();
        bus.publish(doc, 42).await.unwrap();
        let got = timeout(Duration::from_millis(200), sub.updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, 42);
    }

    #[tokio::test]
    async fn presence_payload_round_trip() {
        let bus = MemBus::new();
        let doc = Uuid::new_v4();
        let mut sub = bus.subscribe(doc).await.unwrap();
        bus.publish_presence(doc, vec![1, 2, 3]).await.unwrap();
        let got = timeout(Duration::from_millis(200), sub.presence.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn no_subscriber_publish_succeeds() {
        let bus = MemBus::new();
        bus.publish(Uuid::new_v4(), 1).await.unwrap();
    }
}
