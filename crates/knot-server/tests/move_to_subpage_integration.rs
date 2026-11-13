//! Move to subpage (issue #92, `2026-11-13-move-to-subpage-design.md`):
//! `POST /api/docs/{id}/move-to-subpage` moves the selected top-level blocks
//! into a new child page, leaves a link in their place, and takes the
//! comments and suggestions anchored entirely inside them along.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, Assoc, GetString, IndexedSequence, Out, ReadTxn, StickyIndex, Text, Transact,
    TransactionMut, Update, XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim,
    XmlTextRef,
};

// ---------------------------------------------------------------------------
// Harness (after `duplicate_integration.rs`)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

struct World {
    state: AppState,
    app: Router,
    owner: Session,
}

const PASSWORD: &str = "hunter22hunter22";
const P1: &str = "First paragraph.";
const P2: &str = "Second paragraph.";
const P3: &str = "Third paragraph.";

async fn world() -> World {
    let db = knot_test_support::fresh_db().await;
    let mut state = AppState::with_pool(db.pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let hash = state.hasher.hash(PASSWORD).unwrap();
    let owner = state
        .users
        .as_ref()
        .unwrap()
        .create_local("owner@move.test", "Olive Owner", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    wire_rooms(&mut state, &db.url).await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@move.test").await;
    World { state, app, owner }
}

async fn wire_rooms(state: &mut AppState, db_url: &str) {
    let pool = state.pool.as_ref().unwrap().clone();
    let bus = Arc::new(knot_crdt::PgBus::connect(db_url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    let policy = SnapshotPolicy {
        every_n: 1000,
        idle: Duration::from_secs(3600),
    };
    let rooms = Arc::new(Rooms::new(
        Arc::new(YrsEngine),
        bus.clone(),
        updates,
        snaps,
        policy,
        Duration::from_secs(3600),
    ));
    state.bus = Some(bus);
    state.rooms_v2 = Some(rooms);
}

async fn login(app: &Router, email: &str) -> Session {
    let response = send(
        app,
        None,
        "POST",
        "/auth/login",
        Some(json!({"email": email, "password": PASSWORD})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "login {email}");
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap();
    Session {
        cookie: format!("{sid}; {csrf}"),
        csrf: csrf.trim_start_matches("csrf=").to_string(),
    }
}

async fn send(
    app: &Router,
    who: Option<&Session>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(who) = who {
        request = request
            .header("cookie", &who.cookie)
            .header("x-csrf-token", &who.csrf);
    }
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

/// `who` sends `body`; asserts `status`; returns the answer's JSON.
async fn expect(
    w: &World,
    who: &Session,
    method: &str,
    uri: &str,
    body: Option<Value>,
    status: StatusCode,
) -> Value {
    let response = send(&w.app, Some(who), method, uri, body).await;
    let got = response.status();
    let json = body_json(response).await;
    assert_eq!(got, status, "{method} {uri}: {json}");
    json
}

impl World {
    fn pool(&self) -> &knot_storage::Pool {
        self.state.pool.as_ref().unwrap()
    }

    async fn create_doc(&self, title: &str) -> Uuid {
        let doc = expect(
            self,
            &self.owner,
            "POST",
            "/api/docs",
            Some(json!({"title": title, "parent_id": null})),
            StatusCode::CREATED,
        )
        .await;
        doc["id"].as_str().unwrap().parse().unwrap()
    }

    async fn member(&self, email: &str, role: &str) -> Session {
        expect(
            self,
            &self.owner,
            "POST",
            "/api/workspace/members",
            Some(json!({"email": email, "role": role, "password": PASSWORD, "display_name": role})),
            StatusCode::CREATED,
        )
        .await;
        login(&self.app, email).await
    }

    /// Replace the page's content through the room, as the import does.
    async fn set_content(&self, doc_id: Uuid, content: &yrs::Doc) {
        let update_bytes = content
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        let room = self
            .state
            .rooms_v2
            .as_ref()
            .unwrap()
            .acquire(doc_id)
            .await
            .unwrap();
        let (reply, done) = tokio::sync::oneshot::channel();
        room.tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                by_user: None,
                source: knot_storage::UpdateSource::Import,
                meta: None,
                reply,
            })
            .await
            .unwrap();
        done.await.unwrap().expect("replace page content");
    }

    /// The page's stored content: latest snapshot, then the updates after it.
    async fn stored(&self, doc_id: Uuid) -> yrs::Doc {
        let pool = self.pool().clone();
        let doc = yrs::Doc::new();
        let mut seq = 0;
        if let Some(snap) = PgSnapshotStore::new(pool.clone())
            .latest(doc_id)
            .await
            .unwrap()
        {
            apply(&doc, &snap.state_bytes);
            seq = snap.snapshot_seq;
        }
        for update in PgUpdatesStore::new(pool).since(doc_id, seq).await.unwrap() {
            apply(&doc, &update.update_bytes);
        }
        doc
    }

    async fn comments(&self, doc_id: Uuid) -> Vec<Value> {
        let list = expect(
            self,
            &self.owner,
            "GET",
            &format!("/api/docs/{doc_id}/comments?include_resolved=true"),
            None,
            StatusCode::OK,
        )
        .await;
        list.as_array().unwrap().clone()
    }

    async fn suggestions(&self, doc_id: Uuid) -> Vec<Value> {
        let list = expect(
            self,
            &self.owner,
            "GET",
            &format!("/api/docs/{doc_id}/suggestions"),
            None,
            StatusCode::OK,
        )
        .await;
        list.as_array().unwrap().clone()
    }

    async fn comment(&self, doc_id: Uuid, body: Value) -> Value {
        expect(
            self,
            &self.owner,
            "POST",
            &format!("/api/docs/{doc_id}/comments"),
            Some(body),
            StatusCode::CREATED,
        )
        .await
    }

    async fn move_to_subpage(
        &self,
        who: &Session,
        doc_id: Uuid,
        (start, end): (String, String),
        title: &str,
    ) -> Response {
        send(
            &self.app,
            Some(who),
            "POST",
            &format!("/api/docs/{doc_id}/move-to-subpage"),
            Some(json!({"start": start, "end": end, "title": title})),
        )
        .await
    }
}

fn apply(doc: &yrs::Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).unwrap())
        .unwrap();
}

// ---------------------------------------------------------------------------
// Content and anchors
// ---------------------------------------------------------------------------

fn page(paragraphs: &[&str]) -> yrs::Doc {
    let doc = yrs::Doc::new();
    let fragment = doc.get_or_insert_xml_fragment("default");
    let mut txn = doc.transact_mut();
    for text in paragraphs {
        paragraph(&mut txn, &fragment, text);
    }
    drop(txn);
    doc
}

fn paragraph(txn: &mut TransactionMut, parent: &XmlFragmentRef, text: &str) {
    let paragraph = parent.push_back(txn, XmlElementPrelim::empty("paragraph"));
    paragraph.push_back(txn, XmlTextPrelim::new(text));
}

/// The text of top-level block `block` of `fragment`. Takes the fragment,
/// not the doc: `get_or_insert_xml_fragment` under an open read txn
/// deadlocks.
fn block_text<T: ReadTxn>(txn: &T, fragment: &XmlFragmentRef, block: u32) -> XmlTextRef {
    let Some(XmlOut::Element(el)) = fragment.get(txn, block) else {
        panic!("block {block} is not an element");
    };
    let Some(XmlOut::Text(text)) = el.get(txn, 0) else {
        panic!("block {block} has no text");
    };
    text
}

/// Base64 anchor pair, as the editor encodes a selection (start assoc
/// After at the first selected char, end assoc Before after the last):
/// from char `from` of block `a` to char `to` (exclusive) of block `b`.
fn range(doc: &yrs::Doc, (a, from): (u32, u32), (b, to): (u32, u32)) -> (String, String) {
    let fragment = doc.get_or_insert_xml_fragment("default");
    let txn = doc.transact();
    let start = block_text(&txn, &fragment, a)
        .sticky_index(&txn, from, Assoc::After)
        .unwrap();
    let end = block_text(&txn, &fragment, b)
        .sticky_index(&txn, to, Assoc::Before)
        .unwrap();
    (
        STANDARD.encode(start.encode_v1()),
        STANDARD.encode(end.encode_v1()),
    )
}

/// The whole of top-level block `block`.
fn whole(doc: &yrs::Doc, block: u32, text: &str) -> (String, String) {
    let len = text.encode_utf16().count() as u32;
    range(doc, (block, 0), (block, len))
}

/// The text of the live block `base64` resolves to in `doc`.
fn resolves_to(doc: &yrs::Doc, base64: &str) -> Option<String> {
    let bytes = STANDARD.decode(base64).unwrap();
    let index = StickyIndex::decode_v1(&bytes).unwrap();
    let txn = doc.transact();
    let offset = index.get_offset(&txn)?;
    if offset.branch.is_deleted() {
        return None;
    }
    Some(XmlTextRef::from(offset.branch).get_string(&txn))
}

/// The top-level blocks of `doc`, as plain text; a link to a page shows as
/// `[text](href)`.
fn blocks(doc: &yrs::Doc) -> Vec<String> {
    let fragment = doc.get_or_insert_xml_fragment("default");
    let txn = doc.transact();
    (0..fragment.len(&txn))
        .map(|i| {
            let text = block_text(&txn, &fragment, i);
            let mut out = String::new();
            for chunk in text.diff(&txn, YChange::identity) {
                let Out::Any(Any::String(s)) = &chunk.insert else {
                    continue;
                };
                let href = chunk
                    .attributes
                    .as_ref()
                    .and_then(|attrs| match attrs.get("link") {
                        Some(Any::Map(link)) => match link.get("href") {
                            Some(Any::String(href)) => Some(href.to_string()),
                            _ => None,
                        },
                        _ => None,
                    });
                match href {
                    Some(href) => out.push_str(&format!("[{s}]({href})")),
                    None => out.push_str(s),
                }
            }
            out
        })
        .collect()
}

fn ids(list: &[Value], key: &str) -> Vec<String> {
    let mut ids: Vec<String> = list
        .iter()
        .map(|c| c[key].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The core of issue #92: the discussion moves with the text it is about,
/// whole (replies, reactions, author, resolved state); what is not entirely
/// inside the moved blocks stays.
#[tokio::test(flavor = "multi_thread")]
async fn moves_blocks_with_their_comments_and_suggestions() {
    let w = world().await;
    let source = w.create_doc("Big page").await;
    w.set_content(source, &page(&[P1, P2, P3])).await;
    let doc = w.stored(source).await;

    // Inside P2: a thread with a reply and a reaction; a resolved thread.
    let (s, e) = whole(&doc, 1, P2);
    let inside = w
        .comment(source, json!({"body": "Is this right?", "position_y": s, "position_y_end": e, "anchor_text": P2}))
        .await;
    let inside_thread = inside["thread_id"].as_str().unwrap().to_string();
    expect(
        &w,
        &w.owner,
        "POST",
        &format!("/api/docs/{source}/comments/{inside_thread}/replies"),
        Some(json!({"body": "Yes."})),
        StatusCode::CREATED,
    )
    .await;
    expect(
        &w,
        &w.owner,
        "POST",
        &format!(
            "/api/docs/{source}/comments/{}/reactions",
            inside["id"].as_str().unwrap()
        ),
        Some(json!({"emoji": "👍"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    let (s2, e2) = range(&doc, (1, 0), (1, 6));
    let resolved = w
        .comment(source, json!({"body": "Typo fixed", "position_y": s2, "position_y_end": e2, "anchor_text": "Second"}))
        .await;
    let resolved_thread = resolved["thread_id"].as_str().unwrap().to_string();
    expect(
        &w,
        &w.owner,
        "POST",
        &format!("/api/docs/{source}/comments/{resolved_thread}/resolve"),
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    // Across P1..P2 and whole-doc: these stay.
    let (s3, e3) = range(&doc, (0, 6), (1, 6));
    let spanning = w
        .comment(
            source,
            json!({"body": "Spans two", "position_y": s3, "position_y_end": e3}),
        )
        .await;
    let whole_doc = w.comment(source, json!({"body": "General remark"})).await;
    // A pending suggestion on "Second".
    let suggestion = expect(
        &w,
        &w.owner,
        "POST",
        &format!("/api/docs/{source}/suggestions"),
        Some(json!({
            "kind": "replace", "position_y": s2, "position_y_end": e2,
            "original_text": "Second", "context_before": "", "proposed_text": "2nd",
        })),
        StatusCode::CREATED,
    )
    .await;

    let response = w
        .move_to_subpage(&w.owner, source, whole(&doc, 1, P2), "Part two")
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::CREATED, "move: {body}");
    let child: Uuid = body["doc"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["moved"], json!({"comments": 2, "suggestions": 1}));

    // The child: under the source, titled, holding P2.
    let meta = expect(
        &w,
        &w.owner,
        "GET",
        &format!("/api/docs/{child}"),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        meta["parent_id"].as_str(),
        Some(source.to_string().as_str())
    );
    assert_eq!(meta["title"], "Part two");
    let child_doc = w.stored(child).await;
    assert_eq!(blocks(&child_doc), [P2]);

    // The source: P1, a link to the child where P2 was, P3.
    let source_doc = w.stored(source).await;
    assert_eq!(
        blocks(&source_doc),
        [
            P1.to_string(),
            format!("[Part two](knot://doc/{child})"),
            P3.to_string()
        ]
    );

    // The comments: both P2 threads (with reply, reaction, resolved state)
    // on the child, anchored to the same text; the others on the source.
    let moved = w.comments(child).await;
    assert_eq!(ids(&moved, "thread_id"), {
        let mut t = vec![inside_thread.clone(), resolved_thread.clone()];
        t.sort();
        t
    });
    assert_eq!(moved.len(), 3, "two threads and one reply: {moved:?}");
    let root = moved.iter().find(|c| c["id"] == inside["id"]).unwrap();
    assert_eq!(root["author_id"], inside["author_id"]);
    assert_eq!(root["reactions"]["👍"].as_array().unwrap().len(), 1);
    assert_eq!(
        resolves_to(&child_doc, root["position_y"].as_str().unwrap()).as_deref(),
        Some(P2)
    );
    let resolved_root = moved.iter().find(|c| c["id"] == resolved["id"]).unwrap();
    assert!(!resolved_root["resolved_at"].is_null(), "still resolved");

    let stayed = w.comments(source).await;
    assert_eq!(ids(&stayed, "id"), {
        let mut t = vec![
            spanning["id"].as_str().unwrap().to_string(),
            whole_doc["id"].as_str().unwrap().to_string(),
        ];
        t.sort();
        t
    });

    // The suggestion: on the child, still pending, still anchored.
    assert!(w.suggestions(source).await.is_empty());
    let moved_suggestions = w.suggestions(child).await;
    assert_eq!(
        ids(&moved_suggestions, "id"),
        [suggestion["id"].as_str().unwrap()]
    );
    assert_eq!(moved_suggestions[0]["status"], "pending");
}

/// A selection that covers part of a block moves the whole block.
#[tokio::test(flavor = "multi_thread")]
async fn partial_selection_moves_whole_blocks() {
    let w = world().await;
    let source = w.create_doc("Big page").await;
    w.set_content(source, &page(&[P1, P2, P3])).await;
    let doc = w.stored(source).await;

    // From the middle of P1 to the middle of P2.
    let response = w
        .move_to_subpage(&w.owner, source, range(&doc, (0, 6), (1, 6)), "Part one")
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::CREATED, "move: {body}");
    let child: Uuid = body["doc"]["id"].as_str().unwrap().parse().unwrap();

    assert_eq!(blocks(&w.stored(child).await), [P1, P2]);
    assert_eq!(
        blocks(&w.stored(source).await),
        [format!("[Part one](knot://doc/{child})"), P3.to_string()]
    );
}

/// Moving content is editing: a viewer may not, and nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_is_403() {
    let w = world().await;
    let source = w.create_doc("Big page").await;
    w.set_content(source, &page(&[P1, P2])).await;
    let doc = w.stored(source).await;
    let viewer = w.member("viewer@move.test", "viewer").await;

    let response = w
        .move_to_subpage(&viewer, source, whole(&doc, 1, P2), "Nope")
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(blocks(&w.stored(source).await), [P1, P2]);
}

/// Anchors that don't resolve in the page are refused.
#[tokio::test(flavor = "multi_thread")]
async fn foreign_anchors_are_422() {
    let w = world().await;
    let source = w.create_doc("Big page").await;
    w.set_content(source, &page(&[P1, P2])).await;
    // Anchors into a doc that was never stored.
    let other = page(&[P3]);

    let response = w
        .move_to_subpage(&w.owner, source, whole(&other, 0, P3), "Nope")
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(blocks(&w.stored(source).await), [P1, P2]);
}
