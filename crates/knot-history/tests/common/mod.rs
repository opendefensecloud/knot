#![allow(dead_code)]

pub mod db;
pub mod metrics;

#[path = "../../../knot-crdt/tests/common/fixtures.rs"]
pub mod fixtures;

use std::collections::HashMap;
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
    Update, Xml, XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim,
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
    shapes: Vec<(i64, Shape)>,
}

impl History {
    pub fn new(seed: u64) -> History {
        History {
            rng: Rng::new(seed),
            clients: (1..=3).map(Client::new).collect(),
            room: Doc::new(),
            rows: Vec::new(),
            now: at(0),
            shapes: Vec::new(),
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

    /// `steps` random structured steps from `seed` ([`Shape`]), over a page
    /// that A seeds (row 1) with a heading, a paragraph, a bullet list
    /// whose items carry `checked`, an ordered list (`start`), a 2×2 table
    /// (`colspan`, `colwidth`) and a closing paragraph; B and C pull it.
    /// Each step's row is delivered by `generate`'s rules (pulls, resends,
    /// held-back rows, gaps).
    pub fn generate_rich(seed: u64, steps: usize) -> History {
        let mut h = History::new(seed);
        let seed_row = h.clients[0].txn(rich::seed);
        h.edit_at(0, seed_row, at(0));
        h.pull(1);
        h.pull(2);
        for _ in 0..steps {
            h.rich_step();
        }
        h
    }

    /// The rev of each step's row and the step's kind, for
    /// [`History::generate_rich`] (empty for other histories). The seed
    /// row is no step.
    pub fn shapes(&self) -> &[(i64, Shape)] {
        &self.shapes
    }

    fn rich_step(&mut self) {
        let editor = self.rng.below(3);
        if self.rng.chance(40) && !has_gaps(&self.room) {
            self.clients[editor].pull(&self.room);
        }
        let drawn = rich::SHAPES[self.rng.below(rich::SHAPES.len())];
        let shape = {
            let c = &self.clients[editor];
            let mut txn = c.doc.transact_mut();
            rich::edit(&mut txn, &c.frag, drawn, &mut self.rng)
        };
        if let Some(rev) = self.deliver() {
            self.shapes.push((rev, shape));
        }
    }

    pub fn room(&self) -> &Doc {
        &self.room
    }

    /// The room's state as a snapshot: its blocks and delete set, without
    /// its pending content, as every room writes it from the fix on
    /// (`knot_crdt::pending::encode_without_pending`; its `pending_revs`
    /// name the rows that parked the rest). Rooms before the fix wrote
    /// `encode_state_as_update_v1`, which merges in yrs 0.27.3's encoding of
    /// the pending update, and that encoding shifts the clocks of pending
    /// blocks once the pending update holds overlapping Skips (`replay`
    /// module docs): such a snapshot names blocks the room never had, which
    /// the gap check would report as gap content (seed 251 of
    /// `snapshots_of_the_log_report_no_gap`); the builder leaves such a
    /// legacy snapshot unchecked (`SnapshotProbe::legacy`, Ruling 13).
    ///
    /// This models a fixed room that never reboots. A rebooted room's
    /// snapshot can also hold, as placed blocks, stuck content a kept row
    /// integrated at boot; knot-crdt's reboot property
    /// (`tests/pending_state.rs`) feeds those snapshots to `run_pass` and
    /// reports its counts there.
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
        self.deliver();
    }

    /// One client delivers a row: its increment, a reconnect resend, a
    /// held-back update, the previous row again, or (rarely) a restore by
    /// A. Returns the rev of the row it appended, if any.
    fn deliver(&mut self) -> Option<i64> {
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
            return Some(self.push_at(
                Some(author(0)),
                bytes,
                Some(UpdateSource::Restore),
                None,
                created_at,
            ));
        }
        if pick < 10
            && !has_gaps(&self.room)
            && let Some(last) = self.rows.last().cloned()
        {
            // A retried append that had committed: the same row again.
            return Some(self.push_at(
                last.by_user_id,
                last.update_bytes,
                last.source,
                last.meta,
                created_at,
            ));
        }
        if pick < 25 && !self.clients[c].held.is_empty() {
            let i = self.rng.below(self.clients[c].held.len());
            let bytes = self.clients[c].held.remove(i);
            return Some(self.edit_at(c, bytes, created_at));
        }
        if pick < 45 && !has_gaps(&self.room) {
            let client = &self.clients[c];
            client.take_undelivered();
            let sv = self.room.transact().state_vector();
            let bytes = client.doc.transact().encode_state_as_update_v1(&sv);
            return Some(self.edit_at(c, bytes, created_at));
        }
        let own = self.clients[c].take_undelivered();
        if own.is_empty() {
            return None;
        }
        let merged = yrs::merge_updates_v1(&own).expect("merge own updates");
        if self.rng.chance(15) {
            self.clients[c].held.push(merged);
            return None;
        }
        Some(self.edit_at(c, merged, created_at))
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

    /// B (09:00) writes a code block (attribute `language` = `rust`)
    /// holding "let x = 1;\n" and a paragraph "see "; A (09:05), on B's
    /// page, appends "let y = 2;" to the code block's text and an inline
    /// code "y" to the paragraph, its mark stored as y-tiptap stores a mark
    /// without attributes (`{}`).
    pub fn code_case() -> History {
        let mut h = History::new(1);
        let b = h.clients[1].txn(|txn, frag| {
            let code = frag.push_back(txn, XmlElementPrelim::empty("code_block"));
            code.insert_attribute(txn, "language", Any::String("rust".into()));
            code.push_back(txn, XmlTextPrelim::new("let x = 1;\n"));
            let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, XmlTextPrelim::new("see "));
        });
        h.edit_at(1, b, at(0));
        h.pull(0);
        let a = h.clients[0].txn(|txn, frag| {
            let text = |txn: &TransactionMut, i: u32| match frag.get(txn, i) {
                Some(XmlOut::Element(e)) => match e.get(txn, 0) {
                    Some(XmlOut::Text(t)) => t,
                    other => panic!("block {i} holds no text: {other:?}"),
                },
                other => panic!("no block at {i}: {other:?}"),
            };
            let code = text(txn, 0);
            let len = code.len(txn);
            code.insert(txn, len, "let y = 2;");
            let para = text(txn, 1);
            let len = para.len(txn);
            let mark: Attrs = [("code".into(), Any::Map(Arc::new(HashMap::new())))]
                .into_iter()
                .collect();
            para.insert_with_attributes(txn, len, "y", mark);
        });
        h.edit_at(0, a, at(5));
        h
    }
}

/// The kind of a [`History::generate_rich`] step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Shape {
    /// A word typed into, or characters deleted from, any text; or a new
    /// paragraph.
    Text,
    /// A range of a text formatted bold or italic.
    Bold,
    /// A typed attribute set: heading `level`, list item `checked`, table
    /// cell `colwidth` or `colspan`.
    Attribute,
    /// A list item nested into its previous sibling's sub-list.
    ListNest,
    /// A nested list item moved out, after the item holding its list.
    ListUnnest,
    /// A list item deleted and reinserted with its content elsewhere in its
    /// list.
    ListMove,
    TableRowAdd,
    TableRowRemove,
    /// A paragraph split at a character.
    Split,
    /// A paragraph joined with the paragraph after it.
    Join,
    /// Never drawn: labels the revert-of-a-revert plans in the properties'
    /// per-shape print.
    Revert,
}

/// The structured edits of [`History::generate_rich`].
pub mod rich {
    use super::Shape;
    use super::fixtures::Rng;
    use std::sync::Arc;
    use yrs::types::Attrs;
    use yrs::types::text::YChange;
    use yrs::{
        Any, Out, ReadTxn, Text, TransactionMut, Xml, XmlElementPrelim, XmlElementRef, XmlFragment,
        XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
    };

    /// The shapes a step draws from.
    pub const SHAPES: [Shape; 10] = [
        Shape::Text,
        Shape::Bold,
        Shape::Attribute,
        Shape::ListNest,
        Shape::ListUnnest,
        Shape::ListMove,
        Shape::TableRowAdd,
        Shape::TableRowRemove,
        Shape::Split,
        Shape::Join,
    ];

    /// Where an element sits: the root fragment or an element.
    #[derive(Clone)]
    pub enum Parent {
        Root(XmlFragmentRef),
        El(XmlElementRef),
    }

    impl Parent {
        fn children<T: ReadTxn>(&self, txn: &T) -> Vec<XmlOut> {
            match self {
                Parent::Root(f) => f.children(txn).collect(),
                Parent::El(e) => e.children(txn).collect(),
            }
        }

        fn len<T: ReadTxn>(&self, txn: &T) -> u32 {
            match self {
                Parent::Root(f) => f.len(txn),
                Parent::El(e) => e.len(txn),
            }
        }

        fn remove(&self, txn: &mut TransactionMut, i: u32) {
            match self {
                Parent::Root(f) => f.remove_range(txn, i, 1),
                Parent::El(e) => e.remove_range(txn, i, 1),
            }
        }

        fn insert_el(&self, txn: &mut TransactionMut, i: u32, tag: &str) -> XmlElementRef {
            match self {
                Parent::Root(f) => f.insert(txn, i, XmlElementPrelim::empty(tag)),
                Parent::El(e) => e.insert(txn, i, XmlElementPrelim::empty(tag)),
            }
        }

        fn insert_text(&self, txn: &mut TransactionMut, i: u32) -> XmlTextRef {
            match self {
                Parent::Root(f) => f.insert(txn, i, XmlTextPrelim::new("")),
                Parent::El(e) => e.insert(txn, i, XmlTextPrelim::new("")),
            }
        }
    }

    /// An element of the page, where it sits, and the index (in the walk's
    /// output) of the element holding it.
    pub struct Node {
        pub el: XmlElementRef,
        pub parent: Parent,
        pub index: u32,
        pub up: Option<usize>,
    }

    /// Every element under `frag`, in a fixed order.
    pub fn walk<T: ReadTxn>(txn: &T, frag: &XmlFragmentRef) -> Vec<Node> {
        let mut out = Vec::new();
        let mut stack = vec![(Parent::Root(frag.clone()), None)];
        while let Some((parent, up)) = stack.pop() {
            for (i, child) in parent.children(txn).into_iter().enumerate() {
                if let XmlOut::Element(el) = child {
                    out.push(Node {
                        el: el.clone(),
                        parent: parent.clone(),
                        index: i as u32,
                        up,
                    });
                    stack.push((Parent::El(el), Some(out.len() - 1)));
                }
            }
        }
        out
    }

    /// Every text under `frag`, in a fixed order.
    pub fn texts<T: ReadTxn>(txn: &T, frag: &XmlFragmentRef) -> Vec<XmlTextRef> {
        let mut out = Vec::new();
        let mut stack = vec![Parent::Root(frag.clone())];
        while let Some(parent) = stack.pop() {
            for child in parent.children(txn) {
                match child {
                    XmlOut::Element(el) => stack.push(Parent::El(el)),
                    XmlOut::Text(t) => out.push(t),
                    XmlOut::Fragment(_) => {}
                }
            }
        }
        out
    }

    /// An element's content, read out so it can be written elsewhere.
    pub enum Tree {
        El {
            tag: String,
            attrs: Vec<(String, Any)>,
            children: Vec<Tree>,
        },
        Text(Vec<(String, Attrs)>),
    }

    pub fn read<T: ReadTxn>(txn: &T, node: XmlOut) -> Option<Tree> {
        match node {
            XmlOut::Element(el) => {
                let mut attrs: Vec<(String, Any)> = el
                    .attributes(txn)
                    .filter_map(|(k, v)| match v {
                        Out::Any(a) => Some((k.to_string(), a)),
                        _ => None,
                    })
                    .collect();
                attrs.sort_by(|a, b| a.0.cmp(&b.0));
                Some(Tree::El {
                    tag: el.tag().to_string(),
                    attrs,
                    children: el.children(txn).filter_map(|c| read(txn, c)).collect(),
                })
            }
            XmlOut::Text(t) => Some(Tree::Text(chunks(txn, &t))),
            XmlOut::Fragment(_) => None,
        }
    }

    /// A text's string chunks with their attributes.
    pub fn chunks<T: ReadTxn>(txn: &T, t: &XmlTextRef) -> Vec<(String, Attrs)> {
        t.diff(txn, YChange::identity)
            .into_iter()
            .filter_map(|d| match d.insert {
                Out::Any(Any::String(s)) => {
                    Some((s.to_string(), d.attributes.map(|a| *a).unwrap_or_default()))
                }
                _ => None,
            })
            .collect()
    }

    fn append(txn: &mut TransactionMut, t: &XmlTextRef, chunks: Vec<(String, Attrs)>) {
        for (s, attrs) in chunks {
            let end = t.len(txn);
            t.insert_with_attributes(txn, end, &s, attrs);
        }
    }

    pub fn write(txn: &mut TransactionMut, parent: &Parent, at: u32, tree: Tree) {
        match tree {
            Tree::El {
                tag,
                attrs,
                children,
            } => {
                let el = parent.insert_el(txn, at, &tag);
                for (k, v) in attrs {
                    el.insert_attribute(txn, k, v);
                }
                let me = Parent::El(el);
                for (i, child) in children.into_iter().enumerate() {
                    write(txn, &me, i as u32, child);
                }
            }
            Tree::Text(chunks) => {
                let t = parent.insert_text(txn, at);
                append(txn, &t, chunks);
            }
        }
    }

    fn para(txn: &mut TransactionMut, parent: &Parent, at: u32, text: &str) -> XmlElementRef {
        let p = parent.insert_el(txn, at, "paragraph");
        let t = Parent::El(p.clone()).insert_text(txn, 0);
        t.insert(txn, 0, text);
        p
    }

    fn item(txn: &mut TransactionMut, list: &XmlElementRef, text: &str, checked: Option<bool>) {
        let n = list.len(txn);
        let li = list.insert(txn, n, XmlElementPrelim::empty("list_item"));
        if let Some(c) = checked {
            li.insert_attribute(txn, "checked", Any::Bool(c));
        }
        para(txn, &Parent::El(li), 0, text);
    }

    fn cell(txn: &mut TransactionMut, row: &XmlElementRef, text: &str) {
        let n = row.len(txn);
        let c = row.insert(txn, n, XmlElementPrelim::empty("table_cell"));
        c.insert_attribute(txn, "colspan", Any::Number(1.0));
        c.insert_attribute(
            txn,
            "colwidth",
            Any::Array(Arc::from(vec![Any::Number(100.0)])),
        );
        para(txn, &Parent::El(c), 0, text);
    }

    /// The seeded page.
    pub fn seed(txn: &mut TransactionMut, frag: &XmlFragmentRef) {
        let root = Parent::Root(frag.clone());
        let h = root.insert_el(txn, 0, "heading");
        h.insert_attribute(txn, "level", Any::Number(1.0));
        Parent::El(h).insert_text(txn, 0).insert(txn, 0, "Title");
        para(txn, &root, 1, "intro text here");
        let bl = root.insert_el(txn, 2, "bullet_list");
        for w in ["one", "two", "three"] {
            item(txn, &bl, w, Some(false));
        }
        let ol = root.insert_el(txn, 3, "ordered_list");
        ol.insert_attribute(txn, "start", Any::Number(1.0));
        for w in ["first", "second", "third"] {
            item(txn, &ol, w, None);
        }
        let table = root.insert_el(txn, 4, "table");
        for r in ["1", "2"] {
            let row = table.insert(txn, table.len(txn), XmlElementPrelim::empty("table_row"));
            cell(txn, &row, &format!("a{r}"));
            cell(txn, &row, &format!("b{r}"));
        }
        para(txn, &root, 5, "closing words");
    }

    fn is_list(tag: &str) -> bool {
        tag == "bullet_list" || tag == "ordered_list"
    }

    fn pick<'a, T>(rng: &mut Rng, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[rng.below(items.len())])
    }

    /// One `shape` edit on the page in `txn`; one that finds nothing to edit
    /// does a `Text` edit instead. Returns the shape it did.
    pub fn edit(
        txn: &mut TransactionMut,
        frag: &XmlFragmentRef,
        shape: Shape,
        rng: &mut Rng,
    ) -> Shape {
        let done = match shape {
            Shape::Text => false,
            Shape::Bold => bold(txn, frag, rng),
            Shape::Attribute => attribute(txn, frag, rng),
            Shape::ListNest => list_nest(txn, frag, rng),
            Shape::ListUnnest => list_unnest(txn, frag, rng),
            Shape::ListMove => list_move(txn, frag, rng),
            Shape::TableRowAdd => table_row_add(txn, frag, rng),
            Shape::TableRowRemove => table_row_remove(txn, frag, rng),
            Shape::Split => split(txn, frag, rng),
            Shape::Join => join(txn, frag, rng),
            Shape::Revert => false,
        };
        if done {
            return shape;
        }
        text(txn, frag, rng);
        Shape::Text
    }

    fn text(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) {
        let all = texts(txn, frag);
        let choice = rng.below(4);
        match (choice, pick(rng, &all).cloned()) {
            (0 | 1, Some(t)) => {
                let at = rng.below(t.len(txn) as usize + 1) as u32;
                let word = rng.word();
                t.insert(txn, at, &word);
            }
            (2, Some(t)) if t.len(txn) > 2 => {
                let at = rng.below(t.len(txn) as usize - 2) as u32;
                t.remove_range(txn, at, 1 + rng.below(2) as u32);
            }
            _ => {
                let n = frag.len(txn);
                let at = rng.below(n as usize + 1) as u32;
                let word = rng.word();
                para(txn, &Parent::Root(frag.clone()), at, &word);
            }
        }
    }

    fn bold(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let all: Vec<XmlTextRef> = texts(txn, frag)
            .into_iter()
            .filter(|t| t.len(txn) > 0)
            .collect();
        let Some(t) = pick(rng, &all).cloned() else {
            return false;
        };
        let len = t.len(txn);
        let from = rng.below(len as usize) as u32;
        let count = 1 + rng.below((len - from) as usize) as u32;
        let mark = if rng.chance(50) { "bold" } else { "italic" };
        let attrs: Attrs = [(mark.into(), Any::Bool(true))].into_iter().collect();
        t.format(txn, from, count, attrs);
        true
    }

    fn attribute(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes: Vec<XmlElementRef> = walk(txn, frag)
            .into_iter()
            .map(|n| n.el)
            .filter(|el| matches!(el.tag().as_ref(), "heading" | "list_item" | "table_cell"))
            .collect();
        let Some(el) = pick(rng, &nodes).cloned() else {
            return false;
        };
        match el.tag().as_ref() {
            "heading" => {
                let level = 1 + rng.below(3);
                el.insert_attribute(txn, "level", Any::Number(level as f64));
            }
            "list_item" => {
                let checked = rng.chance(50);
                el.insert_attribute(txn, "checked", Any::Bool(checked));
            }
            _ => {
                if rng.chance(50) {
                    let w = 50 * (1 + rng.below(6));
                    el.insert_attribute(
                        txn,
                        "colwidth",
                        Any::Array(Arc::from(vec![Any::Number(w as f64)])),
                    );
                } else {
                    let span = 1 + rng.below(2);
                    el.insert_attribute(txn, "colspan", Any::Number(span as f64));
                }
            }
        }
        true
    }

    /// List items with their node index, in lists of the page.
    fn list_items(nodes: &[Node]) -> Vec<usize> {
        (0..nodes.len())
            .filter(|&i| {
                nodes[i].el.tag().as_ref() == "list_item"
                    && nodes[i]
                        .up
                        .is_some_and(|u| is_list(nodes[u].el.tag().as_ref()))
            })
            .collect()
    }

    fn list_nest(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes = walk(txn, frag);
        let candidates: Vec<usize> = list_items(&nodes)
            .into_iter()
            .filter(|&i| nodes[i].index > 0)
            .collect();
        let Some(&i) = pick(rng, &candidates) else {
            return false;
        };
        let n = &nodes[i];
        let Some(XmlOut::Element(prev)) = n.parent.children(txn).get(n.index as usize - 1).cloned()
        else {
            return false;
        };
        let Some(tree) = read(txn, XmlOut::Element(n.el.clone())) else {
            return false;
        };
        n.parent.remove(txn, n.index);
        let sub = prev.children(txn).find_map(|c| match c {
            XmlOut::Element(e) if e.tag().as_ref() == "bullet_list" => Some(e),
            _ => None,
        });
        let sub = match sub {
            Some(sub) => sub,
            None => {
                let end = prev.len(txn);
                prev.insert(txn, end, XmlElementPrelim::empty("bullet_list"))
            }
        };
        let end = sub.len(txn);
        write(txn, &Parent::El(sub), end, tree);
        true
    }

    fn list_unnest(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes = walk(txn, frag);
        // An item whose list sits in an item of an outer list.
        let candidates: Vec<(usize, usize, usize)> = list_items(&nodes)
            .into_iter()
            .filter_map(|i| {
                let list = nodes[i].up?;
                let gp = nodes[list].up?;
                (nodes[gp].el.tag().as_ref() == "list_item"
                    && nodes[gp]
                        .up
                        .is_some_and(|u| is_list(nodes[u].el.tag().as_ref())))
                .then_some((i, list, gp))
            })
            .collect();
        let Some(&(i, list, gp)) = pick(rng, &candidates) else {
            return false;
        };
        let Some(tree) = read(txn, XmlOut::Element(nodes[i].el.clone())) else {
            return false;
        };
        if nodes[list].el.len(txn) == 1 {
            nodes[list].parent.remove(txn, nodes[list].index);
        } else {
            nodes[i].parent.remove(txn, nodes[i].index);
        }
        write(txn, &nodes[gp].parent, nodes[gp].index + 1, tree);
        true
    }

    fn list_move(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes = walk(txn, frag);
        let candidates: Vec<usize> = list_items(&nodes)
            .into_iter()
            .filter(|&i| nodes[i].parent.len(txn) > 1)
            .collect();
        let Some(&i) = pick(rng, &candidates) else {
            return false;
        };
        let n = &nodes[i];
        let Some(tree) = read(txn, XmlOut::Element(n.el.clone())) else {
            return false;
        };
        n.parent.remove(txn, n.index);
        let len = n.parent.len(txn);
        let mut to = rng.below(len as usize + 1) as u32;
        if to == n.index {
            to = (to + 1) % (len + 1);
        }
        write(txn, &n.parent, to, tree);
        true
    }

    fn tables(txn: &TransactionMut, frag: &XmlFragmentRef) -> Vec<XmlElementRef> {
        walk(txn, frag)
            .into_iter()
            .map(|n| n.el)
            .filter(|el| el.tag().as_ref() == "table")
            .collect()
    }

    fn table_row_add(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let all = tables(txn, frag);
        let Some(table) = pick(rng, &all).cloned() else {
            return false;
        };
        let at = rng.below(table.len(txn) as usize + 1) as u32;
        let row = table.insert(txn, at, XmlElementPrelim::empty("table_row"));
        let (a, b) = (rng.word(), rng.word());
        cell(txn, &row, &a);
        cell(txn, &row, &b);
        true
    }

    fn table_row_remove(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let all: Vec<XmlElementRef> = tables(txn, frag)
            .into_iter()
            .filter(|t| t.len(txn) > 1)
            .collect();
        let Some(table) = pick(rng, &all).cloned() else {
            return false;
        };
        let at = rng.below(table.len(txn) as usize) as u32;
        table.remove_range(txn, at, 1);
        true
    }

    /// The first text of `p`, if it has one.
    fn text_of<T: ReadTxn>(txn: &T, p: &XmlElementRef) -> Option<XmlTextRef> {
        p.children(txn).find_map(|c| match c {
            XmlOut::Text(t) => Some(t),
            _ => None,
        })
    }

    fn split(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes = walk(txn, frag);
        let candidates: Vec<(usize, XmlTextRef)> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.el.tag().as_ref() == "paragraph")
            .filter_map(|(i, n)| Some((i, text_of(txn, &n.el)?)))
            .filter(|(_, t)| t.len(txn) >= 2)
            .collect();
        let Some((i, t)) = pick(rng, &candidates).cloned() else {
            return false;
        };
        let len = t.len(txn);
        let k = 1 + rng.below(len as usize - 1) as u32;
        // The texts are ASCII, so a byte offset is a character offset.
        let mut tail = Vec::new();
        let mut pos = 0u32;
        for (s, attrs) in chunks(txn, &t) {
            let n = s.len() as u32;
            if pos + n > k {
                let from = k.saturating_sub(pos) as usize;
                tail.push((s[from..].to_string(), attrs));
            }
            pos += n;
        }
        t.remove_range(txn, k, len - k);
        let n = &nodes[i];
        let p = n.parent.insert_el(txn, n.index + 1, "paragraph");
        let t2 = Parent::El(p).insert_text(txn, 0);
        append(txn, &t2, tail);
        true
    }

    fn join(txn: &mut TransactionMut, frag: &XmlFragmentRef, rng: &mut Rng) -> bool {
        let nodes = walk(txn, frag);
        let candidates: Vec<(usize, XmlElementRef)> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.el.tag().as_ref() == "paragraph")
            .filter_map(
                |(i, n)| match n.parent.children(txn).get(n.index as usize + 1) {
                    Some(XmlOut::Element(next)) if next.tag().as_ref() == "paragraph" => {
                        Some((i, next.clone()))
                    }
                    _ => None,
                },
            )
            .collect();
        let Some((i, next)) = pick(rng, &candidates).cloned() else {
            return false;
        };
        let moved = text_of(txn, &next)
            .map(|t| chunks(txn, &t))
            .unwrap_or_default();
        let n = &nodes[i];
        n.parent.remove(txn, n.index + 1);
        let t = match text_of(txn, &n.el) {
            Some(t) => t,
            None => Parent::El(n.el.clone()).insert_text(txn, 0),
        };
        append(txn, &t, moved);
        true
    }
}
