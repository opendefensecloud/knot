//! Integration: a toggle from `/tasks` flips the item the listing shows
//! (spec A11, A12). The reindex worker lists the page's tasks, and the
//! toggle `PATCH /api/docs/{id}/tasks/{item_index}` sends with the listed
//! index goes to the room as `Event::PatchTaskChecked`, as here.

use std::sync::Arc;
use std::time::Duration;

use knot_auth::{Hasher, Throttle};
use knot_crdt::{Engine, Event, RoomHandle, Rooms, SnapshotPolicy, YrsEngine};
use knot_server::AppState;
use knot_storage::{
    DocTask, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole,
};
use uuid::Uuid;
use yrs::{
    Any, GetString, ReadTxn, StateVector, Transact, TransactionMut, Xml, XmlElementPrelim,
    XmlFragment, XmlOut, XmlTextPrelim,
};

/// A node of the page to build: an element, with a `checked` attribute
/// when it is a task item, or a paragraph of text.
enum N {
    El(&'static str, Vec<N>),
    Task(Vec<N>),
    P(&'static str),
}

use N::{El, P, Task};

/// A test server: the setup of `tasks_live_reindex.rs`, with one page.
struct Harness {
    _db: knot_test_support::TestDb,
    state: AppState,
    room: Arc<RoomHandle>,
    doc_id: Uuid,
}

impl Harness {
    async fn new() -> Self {
        let db = knot_test_support::fresh_db().await;
        let pool = db.pool.clone();

        let mut state = AppState::with_pool(pool.clone());
        state.hasher = Arc::new(Hasher::fast_for_tests());
        state.throttle = Arc::new(Throttle::new());
        state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

        let hash = state.hasher.hash("hunter22").unwrap();
        let ws = state
            .workspaces
            .as_ref()
            .unwrap()
            .create("default", "WS")
            .await
            .unwrap();
        let user = state
            .users
            .as_ref()
            .unwrap()
            .create_local("alice@example.com", "Alice", &hash)
            .await
            .unwrap();
        state
            .workspaces
            .as_ref()
            .unwrap()
            .add_member(ws.id, user.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc = state
            .docs
            .as_ref()
            .unwrap()
            .create(ws.id, None, "T", "m", user.id)
            .await
            .unwrap();

        // Rooms registry with dirty-notify wired to the reindex worker.
        let bus: Arc<dyn knot_crdt::Bus> =
            Arc::new(knot_crdt::PgBus::connect(&db.url).await.unwrap());
        let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
        let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
        let policy = SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        };
        let (dirty_tx, dirty_rx) = tokio::sync::mpsc::channel::<Uuid>(64);
        let rooms = Arc::new(
            Rooms::new(
                Arc::new(YrsEngine),
                bus.clone(),
                updates,
                snaps,
                policy,
                Duration::from_secs(3600),
            )
            .with_dirty_tx(dirty_tx),
        );
        state.bus = Some(bus);
        state.rooms_v2 = Some(rooms.clone());

        knot_server::reindex::spawn(state.clone(), dirty_rx);

        let room = rooms.acquire(doc.id).await.unwrap();
        Harness {
            _db: db,
            state,
            room,
            doc_id: doc.id,
        }
    }

    /// Puts `blocks` on the page, as an edit.
    async fn load(&self, blocks: &[N]) {
        let doc = yrs::Doc::new();
        {
            let frag = doc.get_or_insert_xml_fragment("default");
            let mut txn = doc.transact_mut();
            for block in blocks {
                add(&frag, &mut txn, block);
            }
        }
        let update_bytes = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.room
            .tx
            .send(Event::ApplyUpdate {
                update_bytes,
                by_user: None,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().expect("apply failed");
    }

    /// The page's `/tasks` rows, once `ready` holds for them. The reindex
    /// worker flushes every 2 s; `Err` holds the last rows seen when it
    /// doesn't within the deadline.
    async fn listing_once(
        &self,
        ready: impl Fn(&[DocTask]) -> bool,
    ) -> Result<Vec<DocTask>, Vec<DocTask>> {
        let tasks = self.state.tasks.as_ref().unwrap().clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let mut rows = tasks.list_for_doc(self.doc_id).await.unwrap();
            rows.sort_by_key(|row| row.item_index);
            if ready(&rows) {
                return Ok(rows);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(rows);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The toggle from `/tasks`.
    async fn toggle(&self, item_index: i32, checked: bool) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.room
            .tx
            .send(Event::PatchTaskChecked {
                item_index,
                checked,
                by_user: None,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().expect("patch failed");
    }

    /// Every task item of the page in document order: its first
    /// paragraph's text, and whether it is checked.
    async fn items(&self) -> Vec<(String, bool)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.room.tx.send(Event::ExportState(tx)).await.unwrap();
        let (state, _) = rx.await.unwrap().expect("export state");
        let doc = YrsEngine.new_doc();
        YrsEngine.apply_update(&doc, &state).unwrap();
        let ydoc = doc.inner();
        let txn = ydoc.transact();
        let frag = txn.get_xml_fragment("default").expect("page content");
        let mut out = Vec::new();
        for i in 0..frag.len(&txn) {
            collect_items(&txn, &frag.get(&txn, i).unwrap(), &mut out);
        }
        out
    }
}

fn add<F: XmlFragment>(parent: &F, txn: &mut TransactionMut, node: &N) {
    match node {
        El(tag, children) => {
            let el = parent.push_back(txn, XmlElementPrelim::empty(*tag));
            for child in children {
                add(&el, txn, child);
            }
        }
        // The editor's representation: a boolean.
        Task(children) => {
            let el = parent.push_back(txn, XmlElementPrelim::empty("list_item"));
            el.insert_attribute(txn, "checked", Any::Bool(false));
            for child in children {
                add(&el, txn, child);
            }
        }
        P(text) => {
            let el = parent.push_back(txn, XmlElementPrelim::empty("paragraph"));
            el.push_back(txn, XmlTextPrelim::new(*text));
        }
    }
}

fn collect_items<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut Vec<(String, bool)>) {
    let XmlOut::Element(el) = node else {
        return;
    };
    if el.tag().as_ref() == "list_item"
        && let Some(checked) = el.get_attribute(txn, "checked")
    {
        let text = match el.get(txn, 0) {
            Some(XmlOut::Element(first)) => (0..first.len(txn))
                .filter_map(|i| match first.get(txn, i) {
                    Some(XmlOut::Text(t)) => Some(t.get_string(txn)),
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        };
        out.push((text, checked.to_string(txn) == "true"));
    }
    for i in 0..el.len(txn) {
        collect_items(txn, &el.get(txn, i).unwrap(), out);
    }
}

/// Lists the page `blocks`, toggles the task `/tasks` lists as `target`,
/// and checks that exactly the item holding `target` flipped, on the page
/// and in the listing.
async fn toggle_the_listed(blocks: &[N], target: &str) {
    let h = Harness::new().await;
    h.load(blocks).await;
    let rows = h
        .listing_once(|rows| rows.iter().any(|row| row.text == target))
        .await
        .unwrap_or_else(|rows| panic!("/tasks doesn't list {target:?}: {rows:#?}"));
    let row = rows.iter().find(|row| row.text == target).unwrap();
    assert!(!row.checked, "{target:?} starts unchecked");

    let before = h.items().await;
    h.toggle(row.item_index, true).await;
    let after = h.items().await;
    let want: Vec<(String, bool)> = before
        .iter()
        .map(|(text, checked)| (text.clone(), *checked || text == target))
        .collect();
    assert_eq!(
        after, want,
        "toggling {target:?}, listed at index {}, flipped another item",
        row.item_index
    );

    h.listing_once(|rows| rows.iter().all(|row| row.checked == (row.text == target)))
        .await
        .unwrap_or_else(|rows| panic!("/tasks doesn't show only {target:?} checked: {rows:#?}"));
}

mod toggle_from_tasks_flips_the_listed_item {
    use super::*;

    /// Before Task 5 the listing dropped the parent, so `child` was listed
    /// at index 0, the parent's toggle index.
    #[tokio::test]
    async fn nested_checklist() {
        toggle_the_listed(
            &[El(
                "bullet_list",
                vec![
                    Task(vec![
                        P("parent"),
                        El("bullet_list", vec![Task(vec![P("child")])]),
                    ]),
                    Task(vec![P("next")]),
                ],
            )],
            "child",
        )
        .await;
    }

    /// Before Task 5 a checklist in a table cell wasn't listed, though the
    /// toggles count it.
    #[tokio::test]
    async fn table_cell_checklist() {
        toggle_the_listed(
            &[
                El(
                    "table",
                    vec![
                        El("table_row", vec![El("table_header", vec![P("h")])]),
                        El(
                            "table_row",
                            vec![El(
                                "table_cell",
                                vec![El("bullet_list", vec![Task(vec![P("in a cell")])])],
                            )],
                        ),
                    ],
                ),
                El("bullet_list", vec![Task(vec![P("after")])]),
            ],
            "in a cell",
        )
        .await;
    }

    /// Regression guard: Task 3 (A8) made the export write checkboxes in
    /// ordered lists, which the listing read from it.
    #[tokio::test]
    async fn ordered_list_checklist() {
        toggle_the_listed(
            &[El(
                "ordered_list",
                vec![Task(vec![P("first")]), Task(vec![P("second")])],
            )],
            "second",
        )
        .await;
    }
}
