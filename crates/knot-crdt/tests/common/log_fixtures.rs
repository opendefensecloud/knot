//! Docs, the page's log and an editor joined to a room, as the ordered-log
//! tests (`tests/ordered_log.rs`, `tests/two_replicas.rs`) build and compare
//! them: the one copy of each. Those tests include this file by path
//! (`#[path = "common/log_fixtures.rs"] mod log_fixtures;`) next to
//! `common/fixtures.rs`, which they must include as `mod fixtures` at their
//! root, since this file uses `crate::fixtures`. Unlike `fixtures.rs` it
//! needs `knot_crdt`, `knot_storage` and `sqlx`, so the crate's unit tests
//! can't include it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use knot_crdt::{ConnHandle, DocHandle, Engine, Event, InMsg, RoomHandle, YrsEngine};
use knot_storage::{DocUpdate, PgUpdatesStore, RowsAfter, UpdatesStore};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, Subscription, Transact, Update, XmlFragmentRef};

use crate::fixtures::{Rng, random_edit, same_update, unwrap_frame};

// ---------------------------------------------------------------------------
// Docs
// ---------------------------------------------------------------------------

/// A fresh doc with `updates` applied in order.
pub fn doc_of(updates: &[&[u8]]) -> DocHandle {
    let doc = YrsEngine.new_doc();
    for u in updates {
        YrsEngine.apply_update(&doc, u).expect("the update applies");
    }
    doc
}

pub fn text(d: &DocHandle) -> String {
    let frag = d.inner().get_or_insert_xml_fragment("default");
    frag.get_string(&d.inner().transact())
}

/// The pending structs and pending delete set, each encoded.
pub fn pending(d: &DocHandle) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let txn = d.inner().transact();
    let store = txn.store();
    (
        store.pending_update().map(|p| p.update.encode_v1()),
        store.pending_ds().map(|ds| ds.encode_v1()),
    )
}

/// Where two docs differ (state vector, delete set, text, pending content),
/// if they do.
pub fn difference(a: &DocHandle, b: &DocHandle) -> Option<String> {
    let state = |d: &DocHandle| {
        let txn = d.inner().transact();
        (txn.state_vector(), txn.snapshot().delete_set.encode_v1())
    };
    let ((sv_a, ds_a), (sv_b, ds_b)) = (state(a), state(b));
    if sv_a != sv_b {
        return Some(format!("state vectors differ: {sv_a:?} / {sv_b:?}"));
    }
    if ds_a != ds_b {
        return Some("delete sets differ".into());
    }
    let (text_a, text_b) = (text(a), text(b));
    if text_a != text_b {
        return Some(format!("content differs:\n{text_a}\n{text_b}"));
    }
    let ((pu_a, pd_a), (pu_b, pd_b)) = (pending(a), pending(b));
    let structs_agree = match (&pu_a, &pu_b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_update(a, b),
        _ => false,
    };
    if !structs_agree {
        return Some(format!(
            "pending structs differ: {:?} / {:?}",
            pu_a.is_some(),
            pu_b.is_some()
        ));
    }
    if pd_a != pd_b {
        return Some("pending deletes differ".into());
    }
    None
}

#[track_caller]
pub fn assert_same_doc(a: &DocHandle, b: &DocHandle, what: &str) {
    if let Some(how) = difference(a, b) {
        panic!("{what}: {how}");
    }
}

// ---------------------------------------------------------------------------
// The page's log
// ---------------------------------------------------------------------------

/// The page's rows, in rev order.
pub async fn rows(pool: &PgPool, doc: Uuid) -> Vec<DocUpdate> {
    PgUpdatesStore::new(pool.clone())
        .rows_after(doc, RowsAfter::Rev(0))
        .await
        .unwrap()
}

/// The page's log replayed into a fresh doc up to and including `upto`.
pub async fn replay(pool: &PgPool, doc: Uuid, upto: i64) -> DocHandle {
    let rows = rows(pool, doc).await;
    let upto: Vec<&[u8]> = rows
        .iter()
        .filter(|r| r.rev.unwrap() <= upto)
        .map(|r| r.update_bytes.as_slice())
        .collect();
    doc_of(&upto)
}

pub async fn row_count(pool: &PgPool, doc: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// An editor
// ---------------------------------------------------------------------------

/// An editor: a yrs doc joined to a room through a connection of its own.
pub struct Client {
    pub doc: Doc,
    pub frag: XmlFragmentRef,
    pub conn: Uuid,
    /// The user its edits are credited to (`InMsg::by_user`).
    pub by_user: Option<Uuid>,
    rx: mpsc::Receiver<Vec<u8>>,
    /// The update of each local transaction since the last `take`.
    own: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Off while the client applies what it receives.
    recording: Arc<AtomicBool>,
    _sub: Subscription,
}

impl Client {
    /// An editor of client id `client` whose edits credit no user.
    pub async fn join(h: &RoomHandle, client: u64) -> Self {
        Self::join_as(h, client, None).await
    }

    /// An editor of client id `client` whose edits credit `by_user`.
    pub async fn join_as(h: &RoomHandle, client: u64, by_user: Option<Uuid>) -> Self {
        let conn = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(8192);
        let (reply, joined) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: conn,
            handle: ConnHandle::new(tx),
            reply,
        })
        .await
        .unwrap();
        let state = joined.await.unwrap().unwrap();
        let doc = Doc::with_client_id(client);
        let frag = doc.get_or_insert_xml_fragment("default");
        let own = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(true));
        let sub = {
            let (own, recording) = (own.clone(), recording.clone());
            doc.observe_update_v1(move |_, e| {
                if recording.load(Ordering::SeqCst) {
                    own.lock().unwrap().push(e.update.clone());
                }
            })
            .unwrap()
        };
        let client = Client {
            doc,
            frag,
            conn,
            by_user,
            rx,
            own,
            recording,
            _sub: sub,
        };
        client.apply(&state.update);
        client
    }

    pub fn apply(&self, update: &[u8]) {
        self.recording.store(false, Ordering::SeqCst);
        let _ = self
            .doc
            .transact_mut()
            .apply_update(Update::decode_v1(update).unwrap());
        self.recording.store(true, Ordering::SeqCst);
    }

    /// Applies every frame received so far; returns their updates.
    pub fn sync(&mut self) -> Vec<Vec<u8>> {
        let mut got = Vec::new();
        while let Ok(frame) = self.rx.try_recv() {
            let update = unwrap_frame(&frame);
            self.apply(&update);
            got.push(update);
        }
        got
    }

    /// The update of each local transaction since the last call.
    pub fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.own.lock().unwrap())
    }

    /// One random transaction ([`random_edit`]); returns its update.
    pub fn edit(&mut self, rng: &mut Rng) -> Vec<u8> {
        random_edit(&self.doc, &self.frag, rng);
        let mut own = self.take();
        assert_eq!(own.len(), 1, "one transaction, one update");
        own.pop().unwrap()
    }

    pub async fn send(&self, h: &RoomHandle, bytes: Vec<u8>) {
        h.tx.send(Event::Inbound(InMsg {
            from: self.conn,
            by_user: self.by_user,
            bytes,
        }))
        .await
        .unwrap();
    }
}
