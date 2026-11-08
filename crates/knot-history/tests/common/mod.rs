#![allow(dead_code)]

pub mod db;
pub mod metrics;
pub mod underflow;

#[path = "../../../knot-crdt/tests/common/fixtures.rs"]
pub mod fixtures;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use fixtures::{Rng, random_edit, text_at};
use knot_storage::{DocUpdate, UpdateSource};
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Any, Doc, ID, IdSet, ReadTxn, StateVector, Subscription, Text, Transact, TransactionMut,
    Update, XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlTextPrelim,
};

/// The page every generated row belongs to.
pub const DOC_ID: Uuid = Uuid::from_u128(0x6b6e_6f74_0000_0000_0000_0000_0000_0001);

/// Author `i` (0 = A, 1 = B, 2 = C).
pub fn author(i: usize) -> Uuid {
    Uuid::from_u128(0xa000 + i as u128)
}

/// 2026-11-01 09:00 UTC plus `minutes`.
pub fn at(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap() + Duration::minutes(minutes)
}

/// Whether `doc` holds blocks past a gap in their client's clock (ported
/// from `knot-crdt/tests/effective_update.rs`): yrs 0.27.3 panics applying a
/// block that runs from inside such a gap into blocks the doc holds
/// (`block_store.rs:318`), so the generator neither resends to nor pulls
/// from a room doc that has one.
pub fn has_gaps(doc: &Doc) -> bool {
    let txn = doc.transact();
    let sv = txn.state_vector();
    let full = txn.encode_state_as_update_v1(&StateVector::default());
    let held = Update::decode_v1(&full)
        .expect("decode full state")
        .insertions(true);
    let mut past = IdSet::new();
    for (client, ranges) in held.iter() {
        let clock = sv.get(client);
        for r in ranges.iter().filter(|r| r.end > clock) {
            let start = r.start.max(clock);
            past.insert(ID::new(*client, start), r.end - start);
        }
    }
    if let Some(p) = txn.store().pending_update() {
        past.diff_with(&p.update.insertions(true));
    }
    past.iter().any(|(_, ranges)| !ranges.is_empty())
}

/// One editor: a doc with its own client id, its transactions' updates not
/// delivered yet, and updates it holds back to deliver late.
pub struct Client {
    pub doc: Doc,
    pub frag: XmlFragmentRef,
    undelivered: Arc<Mutex<Vec<Vec<u8>>>>,
    recording: Arc<AtomicBool>,
    pub held: Vec<Vec<u8>>,
    _sub: Subscription,
}

impl Client {
    pub fn new(id: u64) -> Client {
        let doc = Doc::with_client_id(id);
        let frag = doc.get_or_insert_xml_fragment("default");
        let undelivered = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(true));
        let sub = {
            let undelivered = undelivered.clone();
            let recording = recording.clone();
            doc.observe_update_v1(move |_, e| {
                if recording.load(Ordering::SeqCst) {
                    undelivered.lock().unwrap().push(e.update.clone());
                }
            })
            .expect("observe_update_v1")
        };
        Client {
            doc,
            frag,
            undelivered,
            recording,
            held: Vec::new(),
            _sub: sub,
        }
    }

    pub fn take_undelivered(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.undelivered.lock().unwrap())
    }

    /// Applies what `room` holds that this client lacks, not as its own.
    pub fn pull(&self, room: &Doc) {
        let sv = self.doc.transact().state_vector();
        let diff = room.transact().encode_state_as_update_v1(&sv);
        self.recording.store(false, Ordering::SeqCst);
        let _ = self
            .doc
            .transact_mut()
            .apply_update(Update::decode_v1(&diff).expect("decode pull"));
        self.recording.store(true, Ordering::SeqCst);
    }

    /// One transaction `f` on this client; returns its update.
    pub fn txn(&self, f: impl FnOnce(&mut TransactionMut, &XmlFragmentRef)) -> Vec<u8> {
        self.take_undelivered();
        f(&mut self.doc.transact_mut(), &self.frag);
        let own = self.take_undelivered();
        yrs::merge_updates_v1(&own).expect("merge own updates")
    }

    /// Formats a random range of a random paragraph's text bold (which
    /// `fixtures::random_edit` never does).
    fn bold(&self, rng: &mut Rng) {
        let mut txn = self.doc.transact_mut();
        let n = self.frag.len(&txn);
        if n == 0 {
            return;
        }
        let Some(t) = text_at(&self.frag, &txn, rng.below(n as usize) as u32) else {
            return;
        };
        let len = t.len(&txn);
        if len == 0 {
            return;
        }
        let from = rng.below(len as usize) as u32;
        let count = 1 + rng.below((len - from) as usize) as u32;
        let attrs: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
        t.format(&mut txn, from, count, attrs);
    }
}

/// A generated page history: three clients (authors A, B, C, client ids
/// 1..=3) editing `default`, each step emitting at most one row, and a
/// reference room doc that applies every row in order.
pub struct History {
    pub rng: Rng,
    pub clients: Vec<Client>,
    room: Doc,
    rows: Vec<DocUpdate>,
    now: DateTime<Utc>,
}

impl History {
    pub fn new(seed: u64) -> History {
        History {
            rng: Rng::new(seed),
            clients: (1..=3).map(Client::new).collect(),
            room: Doc::new(),
            rows: Vec::new(),
            now: at(0),
        }
    }

    /// `steps` random steps from `seed`.
    pub fn generate(seed: u64, steps: usize) -> History {
        let mut h = History::new(seed);
        for _ in 0..steps {
            h.step();
        }
        h
    }

    pub fn rows(&self) -> &[DocUpdate] {
        &self.rows
    }

    pub fn room(&self) -> &Doc {
        &self.room
    }

    /// The room's state as a snapshot: its blocks and delete set, without
    /// its pending content. A room's own snapshot today is
    /// `encode_state_as_update_v1`, which merges in yrs 0.27.3's encoding of
    /// the pending update, and that encoding shifts the clocks of pending
    /// blocks once the pending update holds overlapping Skips (`replay`
    /// module docs): such a snapshot names blocks the room never had, which
    /// the gap check would report as gap content (seed 251 of
    /// `snapshots_of_the_log_report_no_gap`). The generator models a room
    /// that does not write that encoding.
    pub fn room_snapshot(&self) -> Vec<u8> {
        let mut encoder = EncoderV1::new();
        self.room
            .transact()
            .encode_state_as_update(&StateVector::default(), &mut encoder);
        encoder.to_vec()
    }

    /// The time of the last row.
    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// Appends a row by author `who` at `created_at` and applies it to the
    /// room; returns its rev.
    pub fn push_at(
        &mut self,
        who: Option<Uuid>,
        bytes: Vec<u8>,
        source: Option<UpdateSource>,
        meta: Option<serde_json::Value>,
        created_at: DateTime<Utc>,
    ) -> i64 {
        let rev = self.rows.len() as i64 + 1;
        if let Ok(u) = Update::decode_v1(&bytes) {
            let _ = self.room.transact_mut().apply_update(u);
        }
        self.now = self.now.max(created_at);
        self.rows.push(DocUpdate {
            seq: rev,
            doc_id: DOC_ID,
            update_bytes: bytes,
            by_user_id: who,
            created_at,
            rev: Some(rev),
            source,
            meta,
            epoch: Some(0),
        });
        rev
    }

    /// An `edit` row by client `c` (0 = A) at `created_at`.
    pub fn edit_at(&mut self, c: usize, bytes: Vec<u8>, created_at: DateTime<Utc>) -> i64 {
        self.push_at(
            Some(author(c)),
            bytes,
            Some(UpdateSource::Edit),
            None,
            created_at,
        )
    }

    /// Client `c` adds a paragraph holding `text` (one transaction) and
    /// delivers it as an `edit` row at `created_at`.
    pub fn type_para(&mut self, c: usize, text: &str, created_at: DateTime<Utc>) -> i64 {
        let bytes = self.clients[c].txn(|txn, frag| {
            let n = frag.len(txn);
            let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, XmlTextPrelim::new(text));
        });
        self.edit_at(c, bytes, created_at)
    }

    /// Client `c` pulls the room.
    pub fn pull(&mut self, c: usize) {
        self.clients[c].pull(&self.room);
    }

    /// One random step: someone edits, then one client delivers a row: its
    /// increment, a reconnect resend, a held-back update, the previous row
    /// again, or (rarely) a restore by A.
    pub fn step(&mut self) {
        let editor = self.rng.below(3);
        if self.rng.chance(40) && !has_gaps(&self.room) {
            self.clients[editor].pull(&self.room);
        }
        for _ in 0..1 + self.rng.below(3) {
            if self.rng.chance(15) {
                let rng = &mut self.rng;
                self.clients[editor].bold(rng);
            } else {
                let c = &self.clients[editor];
                random_edit(&c.doc, &c.frag, &mut self.rng);
            }
        }
        let gap = Duration::seconds(1 + self.rng.below(30 * 60) as i64);
        let created_at = self.now + gap;
        let c = self.rng.below(3);
        let pick = self.rng.below(100);

        if pick < 3 && !has_gaps(&self.room) {
            // A restore by A: a whole-paragraph replace, on A's view of the
            // room.
            let a = &self.clients[0];
            a.take_undelivered();
            a.pull(&self.room);
            let word = self.rng.word();
            let bytes = a.txn(|txn, frag| {
                let n = frag.len(txn);
                if n > 0 {
                    frag.remove_range(txn, 0, 1);
                }
                let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
                p.push_back(txn, XmlTextPrelim::new(word));
            });
            self.push_at(
                Some(author(0)),
                bytes,
                Some(UpdateSource::Restore),
                None,
                created_at,
            );
            return;
        }
        if pick < 10
            && !has_gaps(&self.room)
            && let Some(last) = self.rows.last().cloned()
        {
            // A retried append that had committed: the same row again.
            self.push_at(
                last.by_user_id,
                last.update_bytes,
                last.source,
                last.meta,
                created_at,
            );
            return;
        }
        if pick < 25 && !self.clients[c].held.is_empty() {
            let i = self.rng.below(self.clients[c].held.len());
            let bytes = self.clients[c].held.remove(i);
            self.edit_at(c, bytes, created_at);
            return;
        }
        if pick < 45 && !has_gaps(&self.room) {
            let client = &self.clients[c];
            client.take_undelivered();
            let sv = self.room.transact().state_vector();
            let bytes = client.doc.transact().encode_state_as_update_v1(&sv);
            self.edit_at(c, bytes, created_at);
            return;
        }
        let own = self.clients[c].take_undelivered();
        if own.is_empty() {
            return;
        }
        let merged = yrs::merge_updates_v1(&own).expect("merge own updates");
        if self.rng.chance(15) {
            self.clients[c].held.push(merged);
            return;
        }
        self.edit_at(c, merged, created_at);
    }

    /// A rows at 09:00, 09:15, 09:30, 09:40 and B rows at 09:10, 09:20, each
    /// typing a paragraph of its own.
    pub fn overlap_case() -> History {
        let mut h = History::new(1);
        h.type_para(0, "a one", at(0));
        h.type_para(1, "b one", at(10));
        h.type_para(0, "a two", at(15));
        h.type_para(1, "b two", at(20));
        h.type_para(0, "a three", at(30));
        h.type_para(0, "a four", at(40));
        h
    }
}
