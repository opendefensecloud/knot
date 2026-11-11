//! Page history M1.2: every writer tags the `doc_updates` row it writes with
//! what made it (`source`), the person whose words it holds (`by_user_id`),
//! the content epoch it saw (`epoch`) and, for some sources, ids, timestamps
//! and titles (`meta`). One test per writer, each driving the real route
//! against Postgres and reading the newest row of the page.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::protocol::{YSyncMessage, decode};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    DocUpdate, PgSnapshotStore, PgUpdatesStore, RowsAfter, SnapshotStore, UpdateSource,
    UpdatesStore, WorkspaceRole, sort_key_between,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Assoc, Doc, ID, ReadTxn, StateVector, StickyIndex, Transact, Update, XmlElementPrelim,
    XmlFragment, XmlTextPrelim,
};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Every wait polls toward this and returns as soon as its condition holds.
const DEADLINE: Duration = Duration::from_secs(10);
const PASSWORD: &str = "hunter22";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Session {
    /// `sid=…; csrf=…`
    cookie: String,
    csrf: String,
}

struct Member {
    id: Uuid,
    session: Session,
}

struct Fixture {
    addr: std::net::SocketAddr,
    app: axum::Router,
    state: AppState,
    pool: knot_storage::Pool,
    ws_id: Uuid,
    /// "Labels", created by Alice.
    doc_id: Uuid,
    /// Owner.
    alice: Member,
    /// Editor.
    bob: Member,
}

/// Postgres-backed state with doc rooms on a MemBus, one page created by
/// Alice, and two logged-in members, served on an ephemeral port.
async fn fixture() -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s.cookie_secure = false;

    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    s.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        Arc::new(knot_crdt::MemBus::new()),
        updates,
        snapshots,
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(600),
        },
        Duration::from_secs(300),
    )));

    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (email, name, role) in [
        ("alice@labels.test", "Alice", WorkspaceRole::Owner),
        ("bob@labels.test", "Bob", WorkspaceRole::Editor),
    ] {
        let hash = s.hasher.hash(PASSWORD).unwrap();
        let u = s
            .users
            .as_ref()
            .unwrap()
            .create_local(email, name, &hash)
            .await
            .unwrap();
        s.workspaces
            .as_ref()
            .unwrap()
            .add_member(ws.id, u.id, role)
            .await
            .unwrap();
        ids.push((u.id, email));
    }
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(
            ws.id,
            None,
            "Labels",
            &sort_key_between(None, None),
            ids[0].0,
        )
        .await
        .unwrap();

    let app = router_with_state(s.clone());
    let mut members = Vec::new();
    for (id, email) in ids {
        members.push(Member {
            id,
            session: login(&app, email).await,
        });
    }
    let [alice, bob]: [Member; 2] = members.try_into().ok().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });

    Fixture {
        addr,
        app,
        state: s,
        pool,
        ws_id: ws.id,
        doc_id: doc.id,
        alice,
        bob,
    }
}

async fn login(app: &axum::Router, email: &str) -> Session {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login {email} failed");
    let mut sid = None;
    let mut csrf = None;
    for v in r.headers().get_all("set-cookie") {
        let pair = v.to_str().unwrap().split(';').next().unwrap();
        if let Some(value) = pair.strip_prefix("sid=") {
            sid = Some(value.to_string());
        } else if let Some(value) = pair.strip_prefix("csrf=") {
            csrf = Some(value.to_string());
        }
    }
    let (sid, csrf) = (sid.expect("sid cookie"), csrf.expect("csrf cookie"));
    Session {
        cookie: format!("sid={sid}; csrf={csrf}"),
        csrf,
    }
}

/// `method uri` as `session`, with `body` of `content_type`.
async fn send(
    app: &axum::Router,
    session: &Session,
    method: &str,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("cookie", &session.cookie)
                .header("x-csrf-token", &session.csrf)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post_json(app: &axum::Router, session: &Session, uri: &str, body: Value) -> Value {
    let (status, text) = send(
        app,
        session,
        "POST",
        uri,
        "application/json",
        body.to_string().into_bytes(),
    )
    .await;
    assert!(status.is_success(), "POST {uri}: {status} {text}");
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    }
}

/// `POST /api/docs/{doc_id}/markdown` (`?mode=…` when given) as `session`.
async fn import_markdown(fx: &Fixture, session: &Session, doc_id: Uuid, md: &str, mode: &str) {
    let (status, body) = send(
        &fx.app,
        session,
        "POST",
        &format!("/api/docs/{doc_id}/markdown{mode}"),
        "text/markdown",
        md.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "import: {body}");
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// The page's rows in rev order.
async fn rows(pool: &knot_storage::Pool, doc_id: Uuid) -> Vec<DocUpdate> {
    PgUpdatesStore::new(pool.clone())
        .rows_after(doc_id, RowsAfter::All)
        .await
        .unwrap()
}

/// The page's newest row, once it has one.
async fn newest_row(pool: &knot_storage::Pool, doc_id: Uuid) -> DocUpdate {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(row) = rows(pool, doc_id).await.pop() {
            return row;
        }
        assert!(Instant::now() < deadline, "the page {doc_id} has no row");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `row` is a new-code row of `source` by `by`, at epoch 0, with `meta`.
#[track_caller]
fn assert_labelled(row: &DocUpdate, source: UpdateSource, by: Uuid, meta: Option<Value>) {
    assert_eq!(row.source, Some(source), "{row:?}");
    assert_eq!(row.by_user_id, Some(by), "{row:?}");
    assert_eq!(row.epoch, Some(0), "{row:?}");
    assert!(row.rev.is_some(), "{row:?}");
    assert_eq!(row.meta, meta, "{row:?}");
}

// ---------------------------------------------------------------------------
// A raw y-sync client (slimmed-down `contributors_integration::Peer`)
// ---------------------------------------------------------------------------

fn sync_frame(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8, subtype];
    knot_server::protocol::append_var_uint(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

struct Peer {
    doc: Doc,
    ws: Ws,
    step2_received: usize,
}

impl Peer {
    /// Dial as `who` and complete the handshake as the web provider does.
    async fn join(fx: &Fixture, who: &Member) -> Self {
        let mut req = format!("ws://{}/collab/doc/{}", fx.addr, fx.doc_id)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "cookie",
            tungstenite::http::HeaderValue::from_str(&who.session.cookie).unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("ws connect");
        let doc = Doc::new();
        let sv = doc.transact().state_vector().encode_v1();
        ws.send(tungstenite::Message::Binary(sync_frame(0, &sv).into()))
            .await
            .unwrap();
        let mut p = Self {
            doc,
            ws,
            step2_received: 0,
        };
        p.pump_until(2).await;
        p
    }

    /// Append a paragraph holding `s`, sent as a SYNC_UPDATE frame.
    async fn type_paragraph(&mut self, s: &str) {
        let frag = self.doc.get_or_insert_xml_fragment("default");
        let before = self.doc.transact().state_vector();
        {
            let mut txn = self.doc.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(s));
        }
        let update = self.doc.transact().encode_state_as_update_v1(&before);
        self.ws
            .send(tungstenite::Message::Binary(sync_frame(2, &update).into()))
            .await
            .unwrap();
    }

    /// Process frames until `step2s` server SyncStep2s are in.
    async fn pump_until(&mut self, step2s: usize) {
        let deadline = Instant::now() + DEADLINE;
        while self.step2_received < step2s {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = tokio::time::timeout(remaining, self.ws.next())
                .await
                .expect("the handshake never completed");
            let Some(Ok(tungstenite::Message::Binary(bytes))) = frame else {
                continue;
            };
            match decode(&bytes) {
                Ok(YSyncMessage::SyncStep1(sv)) => {
                    let sv = StateVector::decode_v1(&sv).unwrap();
                    let reply = self.doc.transact().encode_state_as_update_v1(&sv);
                    self.ws
                        .send(tungstenite::Message::Binary(sync_frame(1, &reply).into()))
                        .await
                        .unwrap();
                }
                Ok(YSyncMessage::SyncStep2(u)) => {
                    let u = Update::decode_v1(&u).unwrap();
                    self.doc.transact_mut().apply_update(u).unwrap();
                    self.step2_received += 1;
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One test per writer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ws_edit_is_labelled_edit() {
    let fx = fixture().await;
    let mut bob = Peer::join(&fx, &fx.bob).await;
    bob.type_paragraph("typed").await;

    let row = newest_row(&fx.pool, fx.doc_id).await;
    assert_labelled(&row, UpdateSource::Edit, fx.bob.id, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn task_toggle_is_labelled_task() {
    let fx = fixture().await;
    import_markdown(&fx, &fx.alice.session, fx.doc_id, "- [ ] ship it\n", "").await;

    let (status, body) = send(
        &fx.app,
        &fx.bob.session,
        "PATCH",
        &format!("/api/docs/{}/tasks/0", fx.doc_id),
        "application/json",
        json!({"checked": true}).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "toggle: {body}");

    let row = newest_row(&fx.pool, fx.doc_id).await;
    assert_labelled(&row, UpdateSource::Task, fx.bob.id, None);
}

/// The suggester wrote the words, so the row is theirs; the owner who
/// accepted them is in `meta`.
#[tokio::test(flavor = "multi_thread")]
async fn accepted_suggestion_credits_the_suggester() {
    let fx = fixture().await;
    // "Hello world" from client 7: "Hello" is clocks 2..=6.
    let seed = {
        let doc = Doc::with_client_id(7);
        let frag = doc.get_or_insert_xml_fragment("default");
        {
            let mut txn = doc.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("Hello world"));
        }
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    };
    let room = fx
        .state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(fx.doc_id)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::ApplyUpdate {
            update_bytes: seed,
            by_user: Some(fx.alice.id),
            source: UpdateSource::Edit,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("seed");

    let anchor = |clock: u32, assoc: Assoc| {
        STANDARD.encode(StickyIndex::from_id(ID::new(ClientID::new(7), clock), assoc).encode_v1())
    };
    let created = post_json(
        &fx.app,
        &fx.bob.session,
        &format!("/api/docs/{}/suggestions", fx.doc_id),
        json!({
            "kind": "replace",
            "position_y": anchor(2, Assoc::After),
            "position_y_end": anchor(6, Assoc::Before),
            "original_text": "Hello",
            "context_before": "",
            "proposed_text": "Howdy",
        }),
    )
    .await;
    let suggestion_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    post_json(
        &fx.app,
        &fx.alice.session,
        &format!("/api/docs/{}/suggestions/{suggestion_id}/accept", fx.doc_id),
        json!({"seen_text": "Hello"}),
    )
    .await;

    let row = newest_row(&fx.pool, fx.doc_id).await;
    assert_labelled(
        &row,
        UpdateSource::Suggestion,
        fx.bob.id,
        Some(json!({"suggestion_id": suggestion_id, "accepted_by": fx.alice.id})),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_import_is_labelled_import() {
    let fx = fixture().await;
    import_markdown(&fx, &fx.bob.session, fx.doc_id, "# Appended\n", "").await;
    let appended = newest_row(&fx.pool, fx.doc_id).await;
    assert_labelled(
        &appended,
        UpdateSource::Import,
        fx.bob.id,
        Some(json!({"mode": "append"})),
    );

    import_markdown(
        &fx,
        &fx.alice.session,
        fx.doc_id,
        "# Replaced\n",
        "?mode=replace",
    )
    .await;
    let replaced = newest_row(&fx.pool, fx.doc_id).await;
    assert!(replaced.rev > appended.rev, "{replaced:?}");
    assert_labelled(
        &replaced,
        UpdateSource::Import,
        fx.alice.id,
        Some(json!({"mode": "replace"})),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_import_is_labelled_import() {
    let fx = fixture().await;
    let pages = ["From the archive", "Also archived"];
    let docs: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(i, title)| {
            json!({
                "id": Uuid::new_v4().to_string(),
                "parent_id": null,
                "title": title,
                "sort_key": format!("n{i}"),
                "path": format!("docs/page-{i}.md"),
            })
        })
        .collect();
    let manifest = json!({
        "knot_export_version": "2",
        "docs": docs,
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    for (i, title) in pages.iter().enumerate() {
        zip.start_file(format!("docs/page-{i}.md"), opts).unwrap();
        zip.write_all(format!("# {title}\n\nImported body.").as_bytes())
            .unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();

    let (status, body) = send(
        &fx.app,
        &fx.alice.session,
        "POST",
        "/api/workspace/import",
        "application/zip",
        bytes,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "workspace import: {body}");

    let alive = fx
        .state
        .docs
        .as_ref()
        .unwrap()
        .list_alive(fx.ws_id)
        .await
        .unwrap();
    for title in pages {
        let page = alive
            .iter()
            .find(|d| d.title == title)
            .unwrap_or_else(|| panic!("{title} was not imported"));
        let row = newest_row(&fx.pool, page.id).await;
        assert_labelled(&row, UpdateSource::Import, fx.alice.id, None);
    }
}

/// The row names the template by id and by the title it had, also when the
/// new page is given a title of its own.
#[tokio::test(flavor = "multi_thread")]
async fn from_template_is_labelled_template() {
    let fx = fixture().await;
    let docs = fx.state.docs.as_ref().unwrap();
    let template = docs
        .create(
            fx.ws_id,
            None,
            "Meeting notes",
            // After "Labels", the first page at the top level.
            &sort_key_between(Some(&sort_key_between(None, None)), None),
            fx.alice.id,
        )
        .await
        .unwrap();
    import_markdown(&fx, &fx.alice.session, template.id, "# Agenda\n", "").await;
    docs.set_template(fx.ws_id, template.id, fx.alice.id, true)
        .await
        .unwrap();

    let created = post_json(
        &fx.app,
        &fx.bob.session,
        &format!("/api/docs/from-template/{}", template.id),
        json!({"title": "Monday"}),
    )
    .await;
    let new_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    let row = newest_row(&fx.pool, new_id).await;
    assert_labelled(
        &row,
        UpdateSource::Template,
        fx.bob.id,
        Some(json!({"template_doc_id": template.id, "template_title": "Meeting notes"})),
    );
}

/// A duplicate's pages each name the page they were copied from.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_is_labelled_copy() {
    let fx = fixture().await;
    let docs = fx.state.docs.as_ref().unwrap();
    import_markdown(&fx, &fx.alice.session, fx.doc_id, "# Top\n", "").await;
    let child = docs
        .create(
            fx.ws_id,
            Some(fx.doc_id),
            "Child",
            &sort_key_between(None, None),
            fx.alice.id,
        )
        .await
        .unwrap();
    import_markdown(&fx, &fx.alice.session, child.id, "# Below\n", "").await;

    let copy = post_json(
        &fx.app,
        &fx.bob.session,
        &format!("/api/docs/{}/duplicate", fx.doc_id),
        json!({"include_subpages": true}),
    )
    .await;
    let copy_id: Uuid = copy["id"].as_str().unwrap().parse().unwrap();
    let child_copy = docs
        .list_alive(fx.ws_id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.parent_id == Some(copy_id))
        .expect("the subpage was copied");

    for (page, source, title) in [
        (copy_id, fx.doc_id, "Labels"),
        (child_copy.id, child.id, "Child"),
    ] {
        let row = newest_row(&fx.pool, page).await;
        assert_labelled(
            &row,
            UpdateSource::Copy,
            fx.bob.id,
            Some(json!({"source_doc_id": source, "source_title": title})),
        );
    }
}

/// Today's Markdown restore names the version restored to.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_restore_is_labelled_restore() {
    let fx = fixture().await;
    let (_, bytes) = knot_markdown::from_markdown::parse("# Snapshot\n\nOld text.").unwrap();
    let engine = knot_crdt::YrsEngine;
    let doc = knot_crdt::Engine::new_doc(&engine);
    knot_crdt::Engine::apply_update(&engine, &doc, &bytes).unwrap();
    let state = knot_crdt::Engine::encode_state_as_update(&engine, &doc, None).unwrap();
    let sv = knot_crdt::Engine::encode_state_vector(&engine, &doc).unwrap();
    let snapshots = PgSnapshotStore::new(fx.pool.clone());
    snapshots.insert(fx.doc_id, 1, &state, &sv).await.unwrap();
    let taken = snapshots.by_seq(fx.doc_id, 1).await.unwrap().unwrap();

    let (status, body) = send(
        &fx.app,
        &fx.bob.session,
        "POST",
        &format!("/api/docs/{}/history/1/restore", fx.doc_id),
        "application/json",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "restore: {body}");

    let row = newest_row(&fx.pool, fx.doc_id).await;
    let meta = row.meta.clone().expect("a restore names its target");
    let ended_at: chrono::DateTime<chrono::Utc> = meta["target_ended_at"]
        .as_str()
        .expect("an RFC 3339 string")
        .parse()
        .unwrap();
    assert_eq!(ended_at, taken.created_at);
    assert_labelled(
        &row,
        UpdateSource::Restore,
        fx.bob.id,
        Some(json!({
            "target_snapshot_seq": 1,
            "target_ended_at": meta["target_ended_at"],
        })),
    );
}
