//! Duplicate (spec D; P15-P19): `POST /api/docs/{id}/duplicate` copies a
//! page, and with `include_subpages` its live subpages, with the drawings and
//! files they show, so the copy never depends on the original. Against a
//! fresh database per test, with a real rooms registry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::auth::AuthContext;
use knot_server::duplicate::{
    self, DuplicateError, MAX_CONCURRENT_DUPLICATES, MAX_DUPLICATE_BLOB_BYTES, MAX_DUPLICATE_PAGES,
};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    AssignedTask, BlobStore, BlobStoreError, DocStore, DocTask, DocTaskInput, PgDocStore,
    PgSnapshotStore, PgUpdatesStore, SnapshotStore, TaskStore, TaskStoreError, UpdatesStore,
    WorkspaceRole,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Map, Out, ReadTxn, Text, Transact, TransactionMut, Update, Xml, XmlElementPrelim,
    XmlElementRef, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim,
};

// ---------------------------------------------------------------------------
// Harness (after `export_integration.rs`)
// ---------------------------------------------------------------------------

/// A logged-in user: cookie header, CSRF token, user id.
#[derive(Clone)]
struct Session {
    cookie: String,
    csrf: String,
    user_id: Uuid,
}

struct World {
    state: AppState,
    app: Router,
    owner: Session,
    workspace_id: Uuid,
}

const PASSWORD: &str = "hunter22hunter22";

/// Minimal valid 1×1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x01, 0x5C, 0x5B, 0x66, 0xE3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

async fn world() -> World {
    world_with(|_| {}).await
}

/// A workspace with one owner ("Olive Owner"), a real rooms registry, and
/// `configure` applied to the state before the router is built.
async fn world_with(configure: impl FnOnce(&mut AppState)) -> World {
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
        .create_local("owner@duplicate.test", "Olive Owner", &hash)
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
    configure(&mut state);
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@duplicate.test", owner.id).await;
    World {
        state,
        app,
        owner,
        workspace_id: workspace.id,
    }
}

/// A real rooms registry on the test database: pages get their content
/// through the room, as from the editor.
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

async fn login(app: &Router, email: &str, user_id: Uuid) -> Session {
    let response = send_raw(
        app,
        None,
        "POST",
        "/auth/login",
        Some("application/json"),
        json!({"email": email, "password": PASSWORD})
            .to_string()
            .into_bytes(),
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
        user_id,
    }
}

/// One request through `app`, returning the whole response. `None` sends
/// it anonymously; the CSRF header goes only with a session.
async fn send_raw(
    app: &Router,
    who: Option<&Session>,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(who) = who {
        request = request
            .header("cookie", &who.cookie)
            .header("x-csrf-token", &who.csrf);
    }
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    app.clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

/// A JSON request as `who`.
async fn send_json(app: &Router, who: &Session, method: &str, uri: &str, body: Value) -> Response {
    send_raw(
        app,
        Some(who),
        method,
        uri,
        Some("application/json"),
        body.to_string().into_bytes(),
    )
    .await
}

async fn body_bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn body_json(response: Response) -> Value {
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

async fn error_code(response: Response) -> String {
    body_json(response).await["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn id_of(doc: &Value) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn blob_url(id: Uuid) -> String {
    format!("/api/blobs/{id}")
}

impl World {
    fn pool(&self) -> &knot_storage::Pool {
        self.state.pool.as_ref().unwrap()
    }

    /// Create a page as `who`, first under `parent` (where "New page" puts
    /// it), like `archive_integration`'s helper.
    async fn create_doc(&self, who: &Session, title: &str, parent: Option<Uuid>) -> Uuid {
        let response = send_json(
            &self.app,
            who,
            "POST",
            "/api/docs",
            json!({"title": title, "parent_id": parent}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "create {title}");
        id_of(&body_json(response).await)
    }

    async fn set_icon(&self, doc_id: Uuid, icon: &str) {
        let response = send_json(
            &self.app,
            &self.owner,
            "PATCH",
            &format!("/api/docs/{doc_id}"),
            json!({"icon": icon}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "icon");
    }

    /// Replace the page's content through the room, as the import does.
    async fn set_markdown(&self, doc_id: Uuid, markdown: &str) {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            &format!("/api/docs/{doc_id}/markdown?mode=replace"),
            Some("text/markdown"),
            markdown.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "markdown import");
    }

    /// Replace the page's content with `content`'s state through the room,
    /// for shapes the markdown import cannot produce.
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
                by_user: Some(self.owner.user_id),
                reply,
            })
            .await
            .unwrap();
        done.await.unwrap().expect("replace page content");
    }

    /// Upload a PNG to `doc_id` as the owner; returns the file's id.
    async fn upload(&self, doc_id: Uuid) -> Uuid {
        const BOUNDARY: &str = "----DuplicateTestBoundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"picture.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(TINY_PNG);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            &format!("/api/docs/{doc_id}/blobs"),
            Some(&format!("multipart/form-data; boundary={BOUNDARY}")),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "upload");
        let url = body_json(response).await["url"]
            .as_str()
            .unwrap()
            .to_string();
        url.trim_start_matches("/api/blobs/").parse().unwrap()
    }

    /// Store a file directly, bypassing the upload route: a tiny PNG whose
    /// metadata claims `byte_size` bytes.
    async fn insert_blob(&self, workspace_id: Uuid, doc_id: Uuid, byte_size: i64) -> Uuid {
        let id = Uuid::new_v4();
        self.state
            .blob_meta
            .as_ref()
            .unwrap()
            .insert(&knot_storage::BlobMetadata {
                id,
                workspace_id,
                doc_id,
                content_type: "image/png".into(),
                byte_size,
                sha256: vec![0; 32],
                original_name: None,
                created_by: self.owner.user_id,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        self.state
            .blob_store
            .as_ref()
            .unwrap()
            .put(id, TINY_PNG, "image/png")
            .await
            .unwrap();
        id
    }

    /// A page in a workspace the owner is not a member of.
    async fn foreign_doc(&self) -> (Uuid, Uuid) {
        let state = &self.state;
        let other_ws = state
            .workspaces
            .as_ref()
            .unwrap()
            .create("other", "Other")
            .await
            .unwrap();
        let outsider = state
            .users
            .as_ref()
            .unwrap()
            .create_local("outsider@duplicate.test", "Outsider", "$h$")
            .await
            .unwrap();
        let doc = state
            .docs
            .as_ref()
            .unwrap()
            .create(other_ws.id, None, "Secret", "m", outsider.id)
            .await
            .unwrap();
        (other_ws.id, doc.id)
    }

    /// Invite a member with `role` ("viewer" or "editor") and log them in.
    async fn member(&self, email: &str, role: &str) -> Session {
        let response = send_json(
            &self.app,
            &self.owner,
            "POST",
            "/api/workspace/members",
            json!({"email": email, "role": role, "password": PASSWORD, "display_name": role}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "invite {email}");
        let user = self
            .state
            .users
            .as_ref()
            .unwrap()
            .find_by_email(email)
            .await
            .unwrap()
            .unwrap();
        login(&self.app, email, user.id).await
    }

    /// Move `doc_id` to the archive as the owner.
    async fn archive(&self, doc_id: Uuid) {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "DELETE",
            &format!("/api/docs/{doc_id}"),
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "archive");
    }

    /// Delete the archive root `doc_id` for good, as the owner.
    async fn purge(&self, doc_id: Uuid) {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            &format!("/api/docs/{doc_id}/purge"),
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "purge");
    }

    /// `POST /api/docs/{doc_id}/duplicate` as `who`: `{}` when
    /// `include_subpages` is `None`.
    async fn duplicate(
        &self,
        who: &Session,
        doc_id: Uuid,
        include_subpages: Option<bool>,
    ) -> Response {
        let body = match include_subpages {
            Some(include) => json!({"include_subpages": include}),
            None => json!({}),
        };
        send_json(
            &self.app,
            who,
            "POST",
            &format!("/api/docs/{doc_id}/duplicate"),
            body,
        )
        .await
    }

    /// A duplicate by the owner that must succeed; returns the copy.
    async fn duplicate_ok(&self, doc_id: Uuid, include_subpages: Option<bool>) -> Value {
        let response = self.duplicate(&self.owner, doc_id, include_subpages).await;
        let status = response.status();
        let body = body_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "duplicate: {body}");
        body
    }

    /// `GET /api/docs/{doc_id}` as the owner.
    async fn doc(&self, doc_id: Uuid) -> Value {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "GET",
            &format!("/api/docs/{doc_id}"),
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "get {doc_id}");
        body_json(response).await
    }

    /// Every live page, as the sidebar lists them.
    async fn docs(&self) -> Vec<Value> {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "GET",
            "/api/docs",
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "list");
        body_json(response).await.as_array().unwrap().clone()
    }

    /// The live pages under `parent`, in the sidebar's order.
    async fn order(&self, parent: Option<Uuid>) -> Vec<Uuid> {
        let parent = parent.map(|p| p.to_string());
        let mut children: Vec<(String, Uuid)> = self
            .docs()
            .await
            .iter()
            .filter(|d| d["parent_id"].as_str().map(str::to_string) == parent)
            .map(|d| (d["sort_key"].as_str().unwrap().to_string(), id_of(d)))
            .collect();
        children.sort();
        children.into_iter().map(|(_, id)| id).collect()
    }

    /// `GET /api/docs/{doc_id}/markdown`, which must succeed.
    async fn markdown(&self, doc_id: Uuid) -> String {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "GET",
            &format!("/api/docs/{doc_id}/markdown"),
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "markdown of {doc_id}");
        String::from_utf8(body_bytes(response).await).unwrap()
    }

    /// `GET` `uri` as the owner: its status and body.
    async fn fetch(&self, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = send_raw(&self.app, Some(&self.owner), "GET", uri, None, Vec::new()).await;
        let status = response.status();
        (status, body_bytes(response).await)
    }

    /// The page's stored content: its latest snapshot, then the updates
    /// after it.
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

    /// The ids of the files whose rows belong to `doc_id`.
    async fn blobs_of(&self, doc_id: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM blobs WHERE doc_id = $1 ORDER BY id")
            .bind(doc_id)
            .fetch_all(self.pool())
            .await
            .unwrap()
    }

    /// The ids of the boards whose rows belong to `doc_id`.
    async fn boards_of(&self, doc_id: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM boards WHERE doc_id = $1 ORDER BY id")
            .bind(doc_id)
            .fetch_all(self.pool())
            .await
            .unwrap()
    }

    /// A board on `doc_id` with a snapshot holding element `a` and an update
    /// after it holding element `b`, so its content is both, and an SVG.
    async fn board(&self, doc_id: Uuid) -> Uuid {
        let boards = self.state.boards.as_ref().unwrap();
        let board = boards
            .create(doc_id, self.owner.user_id, Some("Roadmap".into()))
            .await
            .unwrap();
        let snapshot = elements_update(None, "a", "first");
        boards.put_snapshot(board.id, 1, &snapshot).await.unwrap();
        boards
            .append_update(board.id, &elements_update(Some(&snapshot), "b", "second"))
            .await
            .unwrap();
        boards
            .set_svg(board.id, b"<svg>roadmap</svg>")
            .await
            .unwrap();
        board.id
    }

    /// A board's elements: its latest snapshot and every update, merged as
    /// the board room loads them.
    async fn board_elements(&self, board_id: Uuid) -> BTreeMap<String, String> {
        let boards = self.state.boards.as_ref().unwrap();
        let doc = yrs::Doc::new();
        if let Some((_, state)) = boards.latest_snapshot(board_id).await.unwrap() {
            apply(&doc, &state);
        }
        for update in boards.load_updates(board_id).await.unwrap() {
            apply(&doc, &update);
        }
        let elements = doc.get_or_insert_map("elements");
        let txn = doc.transact();
        elements
            .iter(&txn)
            .map(|(k, v)| (k.to_string(), v.to_string(&txn)))
            .collect()
    }

    /// Rows a duplicate writes, by table.
    async fn counts(&self) -> Vec<(&'static str, i64)> {
        let mut counts = Vec::new();
        for table in [
            "documents",
            "doc_updates",
            "doc_contributors",
            "blobs",
            "boards",
            "board_updates",
        ] {
            let n: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(self.pool())
                    .await
                    .unwrap();
            counts.push((table, n));
        }
        counts
    }

    /// The ids recorded in `blob_copy_pending`.
    async fn pending(&self) -> HashSet<Uuid> {
        sqlx::query_scalar::<_, Uuid>("SELECT blob_id FROM blob_copy_pending")
            .fetch_all(self.pool())
            .await
            .unwrap()
            .into_iter()
            .collect()
    }

    /// The request context of `who` in this workspace.
    fn ctx(&self, who: &Session, role: WorkspaceRole) -> AuthContext {
        AuthContext {
            user_id: who.user_id,
            workspace_id: self.workspace_id,
            role,
        }
    }

    /// Waits until `n` duplicate permits are free: every duplicate that
    /// held one has ended, the refresh of its copy included.
    ///
    /// # Panics
    ///
    /// When they aren't within ten seconds.
    async fn wait_for_permits(&self, n: usize) {
        for _ in 0..500 {
            if self.state.duplicate_permits.available_permits() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "{} duplicate permits free, expected {n}",
            self.state.duplicate_permits.available_permits()
        );
    }

    /// Waits until every duplicate has ended, the refresh of its copy
    /// included (the world's default permits).
    async fn wait_for_refreshes(&self) {
        self.wait_for_permits(MAX_CONCURRENT_DUPLICATES).await;
    }
}

fn apply(doc: &yrs::Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).unwrap())
        .unwrap();
}

/// A board update setting element `key` to `value`, on top of `base` when
/// given (only what is new since it), else from scratch.
fn elements_update(base: Option<&[u8]>, key: &str, value: &str) -> Vec<u8> {
    let doc = yrs::Doc::new();
    let elements = doc.get_or_insert_map("elements");
    if let Some(base) = base {
        apply(&doc, base);
    }
    let since = doc.transact().state_vector();
    elements.insert(&mut doc.transact_mut(), key, value);
    doc.transact().encode_state_as_update_v1(&since)
}

// ---------------------------------------------------------------------------
// Page content
// ---------------------------------------------------------------------------

/// A page whose content `build` writes into its fragment.
fn page(build: impl FnOnce(&mut TransactionMut, &XmlFragmentRef)) -> yrs::Doc {
    let doc = yrs::Doc::new();
    let fragment = doc.get_or_insert_xml_fragment("default");
    build(&mut doc.transact_mut(), &fragment);
    doc
}

fn paragraph<F: XmlFragment>(txn: &mut TransactionMut, parent: &F, text: &str) {
    let paragraph = parent.push_back(txn, XmlElementPrelim::empty("paragraph"));
    paragraph.push_back(txn, XmlTextPrelim::new(text));
}

/// An element `tag` at the end of `parent` with the attributes `attrs`.
fn node<F: XmlFragment>(
    txn: &mut TransactionMut,
    parent: &F,
    tag: &str,
    attrs: &[(&str, String)],
) -> XmlElementRef {
    let el = parent.push_back(txn, XmlElementPrelim::empty(tag));
    for (name, value) in attrs {
        el.insert_attribute(txn, *name, Any::from(value.clone()));
    }
    el
}

/// The values of attribute `attr` of every `tag` element in `doc`, in
/// document order.
fn attr_values(doc: &yrs::Doc, tag: &str, attr: &str) -> Vec<String> {
    fn walk<T: ReadTxn>(txn: &T, node: &XmlOut, tag: &str, attr: &str, out: &mut Vec<String>) {
        if let XmlOut::Element(el) = node {
            if el.tag().as_ref() == tag
                && let Some(value) = el.get_attribute(txn, attr)
            {
                out.push(value.to_string(txn));
            }
            for child in el.children(txn) {
                walk(txn, &child, tag, attr, out);
            }
        }
    }
    let txn = doc.transact();
    let mut out = Vec::new();
    if let Some(fragment) = txn.get_xml_fragment("default") {
        for child in fragment.children(&txn) {
            walk(&txn, &child, tag, attr, &mut out);
        }
    }
    out
}

/// A strict dump of `doc`'s content: one line per element (its tag and
/// every attribute, sorted, with its type) and per text chunk (its content
/// and marks, sorted). Two pages dump alike only if their content is the
/// same, attribute types included.
fn tree(doc: &yrs::Doc) -> String {
    fn dump<T: ReadTxn>(txn: &T, node: &XmlOut, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match node {
            XmlOut::Element(el) => {
                let mut attrs: Vec<String> = el
                    .attributes(txn)
                    .map(|(k, v)| format!("{k}={}", canon_out(txn, &v)))
                    .collect();
                attrs.sort();
                let _ = writeln!(out, "{pad}<{} {}>", el.tag(), attrs.join(" "));
                for child in el.children(txn) {
                    dump(txn, &child, depth + 1, out);
                }
            }
            XmlOut::Text(text) => {
                for chunk in text.diff(txn, YChange::identity) {
                    let mut marks: Vec<String> = chunk
                        .attributes
                        .iter()
                        .flat_map(|attrs| attrs.iter())
                        .map(|(k, v)| format!("{k}={}", canon(v)))
                        .collect();
                    marks.sort();
                    let _ = writeln!(
                        out,
                        "{pad}{} {}",
                        canon_out(txn, &chunk.insert),
                        marks.join(" ")
                    );
                }
            }
            XmlOut::Fragment(fragment) => {
                let _ = writeln!(out, "{pad}<#fragment>");
                for child in fragment.children(txn) {
                    dump(txn, &child, depth + 1, out);
                }
            }
        }
    }
    let txn = doc.transact();
    let mut out = String::new();
    if let Some(fragment) = txn.get_xml_fragment("default") {
        for child in fragment.children(&txn) {
            dump(&txn, &child, 0, &mut out);
        }
    }
    out
}

fn canon_out<T: ReadTxn>(txn: &T, value: &Out) -> String {
    match value {
        Out::Any(any) => canon(any),
        other => format!("shared {}", other.clone().to_string(txn)),
    }
}

/// `any` with its type, map keys sorted.
fn canon(any: &Any) -> String {
    match any {
        Any::Map(map) => {
            let mut entries: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k:?}:{}", canon(v)))
                .collect();
            entries.sort();
            format!("{{{}}}", entries.join(","))
        }
        Any::Array(items) => {
            let items: Vec<String> = items.iter().map(canon).collect();
            format!("[{}]", items.join(","))
        }
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Blob store doubles
// ---------------------------------------------------------------------------

/// Files kept in memory, outside the database: copies go through
/// `BlobStore::copy`'s default, and a purge deletes objects itself.
#[derive(Default)]
struct MemoryBlobs {
    objects: Mutex<HashMap<Uuid, Vec<u8>>>,
}

#[async_trait::async_trait]
impl BlobStore for MemoryBlobs {
    async fn put(&self, id: Uuid, bytes: &[u8], _: &str) -> Result<(), BlobStoreError> {
        self.objects.lock().unwrap().insert(id, bytes.to_vec());
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>, BlobStoreError> {
        self.objects
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(BlobStoreError::NotFound)
    }

    async fn delete(&self, id: Uuid) -> Result<(), BlobStoreError> {
        self.objects.lock().unwrap().remove(&id);
        Ok(())
    }
}

/// A store outside the database whose `copy` only records its `dst` (no
/// delegation, nothing stored) and whose `delete` records too. Its first
/// copy archives the armed source through the pool, so the duplicate's
/// final placement fails after the copies (Review Focus 5).
struct RecordingBlobStore {
    pool: knot_storage::Pool,
    objects: Mutex<HashMap<Uuid, Vec<u8>>>,
    /// (workspace, page, actor): archived on the first copy.
    armed: Mutex<Option<(Uuid, Uuid, Uuid)>>,
    copied: Mutex<Vec<Uuid>>,
    deleted: Mutex<Vec<Uuid>>,
}

impl RecordingBlobStore {
    fn new(pool: knot_storage::Pool) -> Self {
        Self {
            pool,
            objects: Mutex::default(),
            armed: Mutex::default(),
            copied: Mutex::default(),
            deleted: Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for RecordingBlobStore {
    async fn put(&self, id: Uuid, bytes: &[u8], _: &str) -> Result<(), BlobStoreError> {
        self.objects.lock().unwrap().insert(id, bytes.to_vec());
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>, BlobStoreError> {
        self.objects
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(BlobStoreError::NotFound)
    }

    async fn delete(&self, id: Uuid) -> Result<(), BlobStoreError> {
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }

    async fn copy(&self, _src: Uuid, dst: Uuid, _: &str) -> Result<(), BlobStoreError> {
        self.copied.lock().unwrap().push(dst);
        let armed = self.armed.lock().unwrap().take();
        if let Some((workspace_id, doc_id, actor)) = armed {
            PgDocStore::new(self.pool.clone())
                .archive(workspace_id, doc_id, actor)
                .await
                .map_err(|e| BlobStoreError::Backend(e.to_string()))?;
        }
        Ok(())
    }

    fn bytes_deleted_with_metadata(&self) -> bool {
        false
    }
}

/// A task store that, once armed, holds every `upsert_for_doc` (the last
/// step of a page's refresh after an edit or a duplicate) until the test
/// opens it, and announces each one on `entered`.
struct GatedTasks {
    inner: Arc<dyn TaskStore>,
    armed: AtomicBool,
    entered: Notify,
    open: Semaphore,
}

impl GatedTasks {
    fn new(inner: Arc<dyn TaskStore>) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(false),
            entered: Notify::new(),
            open: Semaphore::new(0),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Lets every held write through, and every later one.
    fn open(&self) {
        self.open.add_permits(1);
    }

    /// Waits until a write is held.
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("a task write reached the gate");
    }
}

#[async_trait::async_trait]
impl TaskStore for GatedTasks {
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<(), TaskStoreError> {
        if self.armed.load(Ordering::SeqCst) {
            self.entered.notify_one();
            // Given back at once, so one opening lets every writer through.
            drop(self.open.acquire().await);
        }
        self.inner.upsert_for_doc(workspace_id, doc_id, items).await
    }

    async fn sync_for_doc_quiet(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<(), TaskStoreError> {
        self.inner
            .sync_for_doc_quiet(workspace_id, doc_id, items)
            .await
    }

    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
        hidden: &[Uuid],
    ) -> Result<Vec<AssignedTask>, TaskStoreError> {
        self.inner
            .list_for_assignee(workspace_id, user_id, include_completed, hidden)
            .await
    }

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>, TaskStoreError> {
        self.inner.list_for_doc(doc_id).await
    }
}

/// A world whose task store is a [`GatedTasks`], and `configure` applied.
async fn gated_world(configure: impl FnOnce(&mut AppState)) -> (World, Arc<GatedTasks>) {
    let mut gate = None;
    let w = world_with(|state| {
        let gated = Arc::new(GatedTasks::new(state.tasks.take().unwrap()));
        state.tasks = Some(gated.clone());
        gate = Some(gated);
        configure(state);
    })
    .await;
    (w, gate.unwrap())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The copy's content is the source's, node for node and attribute for
/// attribute (types included), except that the file it shows is its own.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_is_lossless() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Spec", None).await;
    let file = w.upload(source).await;
    let content = page(|txn, fragment| {
        let table = node(txn, fragment, "table", &[]);
        let row = node(txn, &table, "table_row", &[]);
        // Column widths and spans as the editor stores them: numbers and an
        // array of numbers.
        let wide = node(txn, &row, "table_cell", &[]);
        wide.insert_attribute(txn, "colspan", Any::Number(2.0));
        wide.insert_attribute(txn, "rowspan", Any::Number(1.0));
        wide.insert_attribute(
            txn,
            "colwidth",
            Any::Array(Arc::from([Any::Number(120.0), Any::Number(80.0)])),
        );
        paragraph(txn, &wide, "First block");
        paragraph(txn, &wide, "Second block");
        let narrow = node(txn, &row, "table_cell", &[]);
        narrow.insert_attribute(txn, "colwidth", Any::Array(Arc::from([Any::Number(60.0)])));
        paragraph(txn, &narrow, "Narrow");
        node(
            txn,
            fragment,
            "attachment",
            &[("url", blob_url(file)), ("name", "notes.png".into())],
        );
        let list = node(txn, fragment, "bullet_list", &[]);
        let item = node(txn, &list, "list_item", &[]);
        let line = node(txn, &item, "paragraph", &[]);
        line.push_back(txn, XmlTextPrelim::new("Line one"));
        line.push_back(txn, XmlElementPrelim::empty("hard_break"));
        line.push_back(txn, XmlTextPrelim::new("Line two"));
    });
    w.set_content(source, &content).await;

    let copy = id_of(&w.duplicate_ok(source, None).await);

    let copied_files = w.blobs_of(copy).await;
    assert_eq!(copied_files.len(), 1, "the attachment's file is copied");
    let copied_file = copied_files[0];
    assert_ne!(copied_file, file);
    let source_tree = tree(&w.stored(source).await);
    assert!(
        source_tree.contains("colwidth=[Number(120.0),Number(80.0)]")
            && source_tree.contains("hard_break")
            && source_tree.contains("Second block"),
        "the source holds every shape: {source_tree}"
    );
    assert_eq!(
        tree(&w.stored(copy).await),
        source_tree.replace(&file.to_string(), &copied_file.to_string())
    );
    assert_eq!(
        w.markdown(copy).await,
        w.markdown(source)
            .await
            .replace(&file.to_string(), &copied_file.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_sits_right_after_the_source() {
    let w = world().await;
    // "New page" goes first, so the pages are created last to first.
    let after = w.create_doc(&w.owner, "After", None).await;
    let source = w.create_doc(&w.owner, "Source", None).await;
    let before = w.create_doc(&w.owner, "Before", None).await;

    let copy = w.duplicate_ok(source, None).await;
    assert_eq!(copy["parent_id"], Value::Null);
    assert_eq!(
        w.order(None).await,
        vec![before, source, id_of(&copy), after]
    );

    let parent = w.create_doc(&w.owner, "Parent", None).await;
    let second = w.create_doc(&w.owner, "Second", Some(parent)).await;
    let first = w.create_doc(&w.owner, "First", Some(parent)).await;
    let copy = w.duplicate_ok(first, None).await;
    assert_eq!(copy["parent_id"], json!(parent.to_string()));
    assert_eq!(
        w.order(Some(parent)).await,
        vec![first, id_of(&copy), second]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_title_and_icon() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    w.set_icon(source, "🚀").await;
    let child = w.create_doc(&w.owner, "Notes", Some(source)).await;
    w.set_icon(child, "📝").await;

    let copy = w.duplicate_ok(source, Some(true)).await;

    assert_eq!(copy["title"], "Plan (copy)");
    assert_eq!(copy["icon"], "🚀");
    assert_eq!(copy["is_template"], false);
    assert_eq!(copy["archived"], false);
    let children = w.order(Some(id_of(&copy))).await;
    assert_eq!(children.len(), 1);
    let child_copy = w.doc(children[0]).await;
    assert_eq!(child_copy["title"], "Notes", "subpages keep their titles");
    assert_eq!(child_copy["icon"], "📝");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_is_copied_and_independent() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let board = w.board(source).await;
    let content = page(|txn, fragment| {
        node(
            txn,
            fragment,
            "excalidraw_board",
            &[("board_id", board.to_string()), ("label", "Roadmap".into())],
        );
    });
    w.set_content(source, &content).await;

    let copy = id_of(&w.duplicate_ok(source, None).await);

    let copied = w.boards_of(copy).await;
    assert_eq!(copied.len(), 1, "one board for the copy");
    let copied_board = copied[0];
    assert_ne!(copied_board, board);
    assert_eq!(
        attr_values(&w.stored(copy).await, "excalidraw_board", "board_id"),
        vec![copied_board.to_string()]
    );
    let elements = w.board_elements(board).await;
    assert_eq!(
        elements,
        BTreeMap::from([
            ("a".to_string(), "first".to_string()),
            ("b".to_string(), "second".to_string())
        ]),
        "snapshot and update both count"
    );
    assert_eq!(w.board_elements(copied_board).await, elements);
    let boards = w.state.boards.as_ref().unwrap();
    let copied_row = boards.get(copied_board).await.unwrap();
    assert_eq!(copied_row.label.as_deref(), Some("Roadmap"));
    assert_eq!(copied_row.svg_seq, 1);
    let (status, svg) = w.fetch(&format!("/api/boards/{copied_board}/svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(svg, b"<svg>roadmap</svg>");

    // Editing the copy leaves the source as it was.
    let updates = boards.load_updates(copied_board).await.unwrap();
    assert_eq!(updates.len(), 1, "the copy's state is one update");
    boards
        .append_update(
            copied_board,
            &elements_update(Some(&updates[0]), "c", "third"),
        )
        .await
        .unwrap();
    boards
        .set_svg(copied_board, b"<svg>edited</svg>")
        .await
        .unwrap();
    assert_eq!(w.board_elements(copied_board).await.len(), 3);
    assert_eq!(w.board_elements(board).await, elements);
    assert_eq!(
        w.fetch(&format!("/api/boards/{board}/svg")).await.1,
        b"<svg>roadmap</svg>"
    );
}

/// With the files in the database and outside it: the copy's files and
/// board are its own, so they outlive the original.
#[tokio::test(flavor = "multi_thread")]
async fn images_survive_archiving_and_purging_the_original() {
    for bytes_in_database in [true, false] {
        let w = if bytes_in_database {
            world().await
        } else {
            world_with(|state| state.blob_store = Some(Arc::new(MemoryBlobs::default()))).await
        };
        let source = w.create_doc(&w.owner, "Photos", None).await;
        let image = w.upload(source).await;
        let board = w.board(source).await;
        let content = page(|txn, fragment| {
            node(txn, fragment, "image", &[("src", blob_url(image))]);
            node(
                txn,
                fragment,
                "excalidraw_board",
                &[("board_id", board.to_string())],
            );
        });
        w.set_content(source, &content).await;

        let copy = id_of(&w.duplicate_ok(source, None).await);
        assert!(w.pending().await.is_empty(), "a committed copy is kept");
        w.archive(source).await;
        w.purge(source).await;

        let copy_content = w.stored(copy).await;
        let srcs = attr_values(&copy_content, "image", "src");
        assert_eq!(srcs.len(), 1);
        assert_ne!(srcs[0], blob_url(image), "in database: {bytes_in_database}");
        let (status, bytes) = w.fetch(&srcs[0]).await;
        assert_eq!(status, StatusCode::OK, "in database: {bytes_in_database}");
        assert_eq!(bytes, TINY_PNG);
        let boards = attr_values(&copy_content, "excalidraw_board", "board_id");
        let (status, svg) = w.fetch(&format!("/api/boards/{}/svg", boards[0])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(svg, b"<svg>roadmap</svg>");
        assert_ne!(
            w.fetch(&blob_url(image)).await.0,
            StatusCode::OK,
            "the original's file is gone"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn subpages_copied_with_internal_links_remapped() {
    let w = world().await;
    let outside = w.create_doc(&w.owner, "Outside", None).await;
    let source = w.create_doc(&w.owner, "Source", None).await;
    let b = w.create_doc(&w.owner, "B", Some(source)).await;
    let a = w.create_doc(&w.owner, "A", Some(source)).await;
    let a1 = w.create_doc(&w.owner, "A1", Some(a)).await;
    w.set_markdown(
        source,
        &format!("See [A](knot://doc/{a}) and [out](knot://doc/{outside}).\n"),
    )
    .await;
    w.set_markdown(
        a1,
        &format!("Up to [top](knot://doc/{source}) and over to [B](knot://doc/{b}).\n"),
    )
    .await;

    let top = id_of(&w.duplicate_ok(source, Some(true)).await);

    let children = w.order(Some(top)).await;
    assert_eq!(children.len(), 2);
    let (a_copy, b_copy) = (children[0], children[1]);
    assert_eq!(w.doc(a_copy).await["title"], "A");
    assert_eq!(w.doc(b_copy).await["title"], "B");
    let grandchildren = w.order(Some(a_copy)).await;
    assert_eq!(grandchildren.len(), 1);
    let a1_copy = grandchildren[0];
    assert_eq!(w.doc(a1_copy).await["title"], "A1");
    assert!(w.order(Some(b_copy)).await.is_empty());

    let top_md = w.markdown(top).await;
    assert!(top_md.contains(&format!("knot://doc/{a_copy}")), "{top_md}");
    assert!(
        top_md.contains(&format!("knot://doc/{outside}")),
        "links outside the copied pages stay: {top_md}"
    );
    assert!(!top_md.contains(&format!("knot://doc/{a})")), "{top_md}");
    let a1_md = w.markdown(a1_copy).await;
    assert!(a1_md.contains(&format!("knot://doc/{top}")), "{a1_md}");
    assert!(a1_md.contains(&format!("knot://doc/{b_copy}")), "{a1_md}");
    assert!(
        w.markdown(source)
            .await
            .contains(&format!("knot://doc/{a})")),
        "the source keeps its links"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn subpages_not_copied_by_default() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Source", None).await;
    let child = w.create_doc(&w.owner, "Child", Some(source)).await;
    w.set_markdown(source, &format!("See [child](knot://doc/{child}).\n"))
        .await;
    let pages = w.docs().await.len();

    let top = id_of(&w.duplicate_ok(source, None).await);

    assert_eq!(w.docs().await.len(), pages + 1, "the page alone");
    assert!(w.order(Some(top)).await.is_empty());
    assert!(
        w.markdown(top)
            .await
            .contains(&format!("knot://doc/{child}")),
        "a link to a page not copied stays"
    );
}

/// Files and boards of pages the user can't read (another workspace) are
/// not copied: the copy references them as the source does.
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_blobs_are_not_copied() {
    let w = world().await;
    let (other_ws, foreign_page) = w.foreign_doc().await;
    let foreign_file = w.insert_blob(other_ws, foreign_page, 10).await;
    let foreign_board = w
        .state
        .boards
        .as_ref()
        .unwrap()
        .create(foreign_page, w.owner.user_id, None)
        .await
        .unwrap()
        .id;
    let source = w.create_doc(&w.owner, "Mixed", None).await;
    let own_file = w.upload(source).await;
    let content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(foreign_file))]);
        node(txn, fragment, "image", &[("src", blob_url(own_file))]);
        node(
            txn,
            fragment,
            "excalidraw_board",
            &[("board_id", foreign_board.to_string())],
        );
    });
    w.set_content(source, &content).await;

    let copy = id_of(&w.duplicate_ok(source, None).await);

    let copied = w.blobs_of(copy).await;
    assert_eq!(copied.len(), 1, "only the readable file is copied");
    let copy_content = w.stored(copy).await;
    assert_eq!(
        attr_values(&copy_content, "image", "src"),
        vec![blob_url(foreign_file), blob_url(copied[0])]
    );
    assert!(w.boards_of(copy).await.is_empty());
    assert_eq!(
        attr_values(&copy_content, "excalidraw_board", "board_id"),
        vec![foreign_board.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_source_is_409() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Old", None).await;
    w.archive(source).await;
    let pages = w.counts().await;

    let response = w.duplicate(&w.owner, source, Some(true)).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "doc.archived");
    // The same answer from the duplicate itself, behind the route's check.
    let direct = duplicate::run(
        w.state.clone(),
        w.ctx(&w.owner, WorkspaceRole::Owner),
        WorkspaceRole::Owner,
        source,
        false,
    )
    .await;
    assert!(
        matches!(direct, Err(DuplicateError::Archived)),
        "{direct:?}"
    );
    assert_eq!(w.counts().await, pages);
}

/// An unknown page, or one in another workspace (even of a member of both),
/// gets the answer every `/api/docs/{id}` route gives it: 403 `acl.no_grant`
/// from the page-role check, which tells nothing about whether it exists.
/// Behind that check, the duplicate itself finds no such page in the
/// user's workspace.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_or_foreign_source_is_refused() {
    let w = world().await;
    let (other_ws, foreign_page) = w.foreign_doc().await;
    // The owner is a member there too, but signed in to their own workspace.
    w.state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(other_ws, w.owner.user_id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let rows = w.counts().await;
    for source in [Uuid::new_v4(), foreign_page] {
        let response = w.duplicate(&w.owner, source, None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{source}");
        assert_eq!(error_code(response).await, "acl.no_grant");
        assert_eq!(w.counts().await, rows);

        let result = duplicate::run(
            w.state.clone(),
            w.ctx(&w.owner, WorkspaceRole::Owner),
            WorkspaceRole::Owner,
            source,
            false,
        )
        .await;
        assert!(
            matches!(result, Err(DuplicateError::NotFound)),
            "{result:?}"
        );
    }
}

/// P15: a viewer can't duplicate, even with an editor grant on the page,
/// since the copy is a new page in the workspace.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_is_403() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let viewer = w.member("viewer@duplicate.test", "viewer").await;
    let pages = w.counts().await;

    let response = w.duplicate(&viewer, source, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "acl.editor_required");

    let granted = send_json(
        &w.app,
        &w.owner,
        "PUT",
        &format!("/api/docs/{source}/grants/user:{}", viewer.user_id),
        json!({"role": "editor", "inherit": true}),
    )
    .await;
    assert!(granted.status().is_success(), "grant");
    let response = w.duplicate(&viewer, source, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "acl.editor_required");
    assert_eq!(w.counts().await, pages);
}

#[tokio::test(flavor = "multi_thread")]
async fn template_is_422() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Meeting notes", None).await;
    let response = send_json(
        &w.app,
        &w.owner,
        "POST",
        &format!("/api/docs/{source}/template"),
        json!({"is_template": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "make template");
    let pages = w.counts().await;

    let response = w.duplicate(&w.owner, source, None).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(response).await, "doc.is_template");
    assert_eq!(w.counts().await, pages);
}

/// The error of a 413, whose message names the caps.
fn too_large() -> Value {
    json!({
        "code": "doc.duplicate_too_large",
        "message": "over 200 pages or 200 MiB of files",
        "details": {},
    })
}

/// Above 200 pages, or 200 MiB of files, nothing is copied (413). Exactly
/// at the caps, the copy is made. The files are kept outside the database,
/// so a duplicate refused over the cap would show every object it copied,
/// and every copy it recorded for cleanup, before it checked.
#[tokio::test(flavor = "multi_thread")]
async fn too_large_is_413() {
    assert_eq!(MAX_DUPLICATE_PAGES, 200);
    assert_eq!(MAX_DUPLICATE_BLOB_BYTES, 200 * 1024 * 1024);
    let mut recording = None;
    let w = world_with(|state| {
        let store = Arc::new(RecordingBlobStore::new(state.pool.clone().unwrap()));
        state.blob_store = Some(store.clone());
        recording = Some(store);
    })
    .await;
    let store = recording.unwrap();
    let source = w.create_doc(&w.owner, "Big", None).await;
    for i in 0..MAX_DUPLICATE_PAGES - 1 {
        w.create_doc(&w.owner, &format!("Page {i}"), Some(source))
            .await;
    }
    // The page and its 199 subpages: 200 pages.
    let copy = id_of(&w.duplicate_ok(source, Some(true)).await);
    assert_eq!(w.order(Some(copy)).await.len(), MAX_DUPLICATE_PAGES - 1);

    w.create_doc(&w.owner, "One too many", Some(source)).await;
    let rows = w.counts().await;
    let response = w.duplicate(&w.owner, source, Some(true)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body_json(response).await["error"], too_large());
    assert_eq!(w.counts().await, rows);
    // Without its subpages, the page alone is small enough.
    w.duplicate_ok(source, None).await;

    // Files: the copied files' recorded sizes add up. A file holds at most
    // 10 MiB, so 20 of them reach the cap.
    const LARGEST_FILE: i64 = 10 * 1024 * 1024;
    let at_cap = w.create_doc(&w.owner, "At the cap", None).await;
    let mut files = Vec::new();
    for _ in 0..MAX_DUPLICATE_BLOB_BYTES / LARGEST_FILE {
        files.push(w.insert_blob(w.workspace_id, at_cap, LARGEST_FILE).await);
    }
    let content = page(|txn, fragment| {
        for file in &files {
            node(txn, fragment, "image", &[("src", blob_url(*file))]);
        }
    });
    w.set_content(at_cap, &content).await;
    let copy = id_of(&w.duplicate_ok(at_cap, None).await);
    assert_eq!(w.blobs_of(copy).await.len(), files.len());
    assert_eq!(
        store.copied.lock().unwrap().len(),
        files.len(),
        "the objects are copied through the store"
    );

    let over = w.create_doc(&w.owner, "Over the cap", None).await;
    let one_byte = w.insert_blob(w.workspace_id, over, 1).await;
    let content = page(|txn, fragment| {
        for file in files.iter().chain([&one_byte]) {
            node(txn, fragment, "image", &[("src", blob_url(*file))]);
        }
    });
    w.set_content(over, &content).await;
    store.copied.lock().unwrap().clear();
    let rows = w.counts().await;
    let response = w.duplicate(&w.owner, over, None).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body_json(response).await["error"], too_large());
    assert_eq!(w.counts().await, rows);
    assert!(
        store.copied.lock().unwrap().is_empty(),
        "no object is copied"
    );
    assert!(w.pending().await.is_empty(), "no copy is recorded");
}

/// Review Focus 5: the source is archived after its files were copied, so
/// the final placement fails. Nothing is written, every copied object stays
/// recorded, and the purge job's cleanup removes exactly those.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_failure_leaves_no_rows_or_objects() {
    let mut recording = None;
    let w = world_with(|state| {
        let store = Arc::new(RecordingBlobStore::new(state.pool.clone().unwrap()));
        state.blob_store = Some(store.clone());
        recording = Some(store);
    })
    .await;
    let store = recording.unwrap();
    let source = w.create_doc(&w.owner, "Source", None).await;
    let child = w.create_doc(&w.owner, "Child", Some(source)).await;
    let image = w.upload(source).await;
    let attachment = w.upload(source).await;
    let child_image = w.upload(child).await;
    let board = w.board(source).await;
    let content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(image))]);
        node(
            txn,
            fragment,
            "attachment",
            &[("url", blob_url(attachment))],
        );
        node(
            txn,
            fragment,
            "excalidraw_board",
            &[("board_id", board.to_string())],
        );
    });
    w.set_content(source, &content).await;
    let child_content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(child_image))]);
    });
    w.set_content(child, &child_content).await;
    *store.armed.lock().unwrap() = Some((w.workspace_id, source, w.owner.user_id));
    let rows = w.counts().await;

    let response = w.duplicate(&w.owner, source, Some(true)).await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        error_code(response).await,
        "doc.duplicate_conflict",
        "the final placement ran and refused"
    );
    assert_eq!(w.counts().await, rows, "no rows remain");
    let copied: HashSet<Uuid> = store.copied.lock().unwrap().iter().copied().collect();
    assert_eq!(copied.len(), 3, "every file was copied");
    assert_eq!(w.pending().await, copied, "every copy is recorded");
    assert!(
        store.deleted.lock().unwrap().is_empty(),
        "nothing is deleted on the request path"
    );

    let pool = w.pool().clone();
    let removed =
        knot_storage::duplicate::purge_stale_copies(&pool, store.as_ref(), Duration::ZERO)
            .await
            .unwrap();
    assert_eq!(removed, copied.len());
    let deleted: HashSet<Uuid> = store.deleted.lock().unwrap().iter().copied().collect();
    assert_eq!(deleted, copied);
    assert!(w.pending().await.is_empty());
}

/// A file whose object is gone from the store can't be copied: the
/// duplicate fails rather than leave the copy depending on the original's
/// file, and its copies stay recorded for the purge job.
#[tokio::test(flavor = "multi_thread")]
async fn missing_file_object_is_a_conflict() {
    let mut memory = None;
    let w = world_with(|state| {
        let store = Arc::new(MemoryBlobs::default());
        state.blob_store = Some(store.clone());
        memory = Some(store);
    })
    .await;
    let store = memory.unwrap();
    let source = w.create_doc(&w.owner, "Source", None).await;
    let image = w.upload(source).await;
    let content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(image))]);
    });
    w.set_content(source, &content).await;
    store.objects.lock().unwrap().remove(&image);
    let rows = w.counts().await;

    let response = w.duplicate(&w.owner, source, None).await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "doc.duplicate_conflict");
    assert_eq!(w.counts().await, rows);
    assert_eq!(
        w.pending().await.len(),
        1,
        "the planned copy stays recorded"
    );
}

/// P18: a duplicate reads its pages without booting their rooms. Rooms are
/// never evicted when idle, so booting them would keep every page copied in
/// memory. Neither pages nobody opened nor their copies have a room once the
/// duplicate, the refresh of its copies included, has ended.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_boots_no_room() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let child = w.create_doc(&w.owner, "Steps", Some(source)).await;
    // Content stored without a room: pages nobody opened since a restart.
    let updates = PgUpdatesStore::new(w.pool().clone());
    for (doc_id, text) in [(source, "The plan"), (child, "The steps")] {
        let content = page(|txn, fragment| paragraph(txn, fragment, text));
        let state = content
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        updates
            .insert_batch(doc_id, &[(Some(w.owner.user_id), state)])
            .await
            .unwrap();
    }
    let rooms = w.state.rooms_v2.as_ref().unwrap();
    assert!(rooms.get_active(source).is_none());
    assert!(rooms.get_active(child).is_none());

    let top = id_of(&w.duplicate_ok(source, Some(true)).await);
    w.wait_for_refreshes().await;

    let child_copy = w.order(Some(top)).await[0];
    for id in [source, child, top, child_copy] {
        assert!(rooms.get_active(id).is_none(), "{id} has a room");
    }
    assert!(tree(&w.stored(top).await).contains("The plan"));
    assert!(tree(&w.stored(child_copy).await).contains("The steps"));
}

#[tokio::test(flavor = "multi_thread")]
async fn byline_credits_the_duplicating_user() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    w.set_markdown(source, "Written by the owner.\n").await;
    let editor = w.member("editor@duplicate.test", "editor").await;

    let response = w.duplicate(&editor, source, None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let copy = body_json(response).await;
    assert_eq!(copy["created_by"], json!(editor.user_id.to_string()));

    let response = send_raw(
        &w.app,
        Some(&editor),
        "GET",
        &format!("/api/docs/{}/contributors", id_of(&copy)),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let byline = body_json(response).await;
    assert_eq!(
        byline["created_by"]["id"],
        json!(editor.user_id.to_string())
    );
    let contributors = byline["contributors"].as_array().unwrap();
    assert_eq!(contributors.len(), 1, "{byline}");
    assert_eq!(
        contributors[0]["user_id"],
        json!(editor.user_id.to_string())
    );
}

/// Spec D: comments, history, share links and grants stay with the source.
/// A copied grant would widen who can reach the copy, and a copied share
/// link would publish it.
/// A copy starts its own life: no comments, history or public links. Nor
/// grants: who may see the original says nothing about who may see a copy
/// someone else made of it. The copier's own `owner` grant on a copy of a
/// restricted page is the one exception (see
/// `duplicate_of_boundary_stays_restricted_and_actor_is_owner`); this page
/// is not restricted, so its copy has no grant at all.
#[tokio::test(flavor = "multi_thread")]
async fn comments_history_shares_and_grants_are_not_copied() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let viewer = w.member("viewer@duplicate.test", "viewer").await;
    let post = |uri: String, body: Value| {
        let (app, owner) = (w.app.clone(), w.owner.clone());
        async move {
            let response = send_json(&app, &owner, "POST", &uri, body).await;
            assert_eq!(response.status(), StatusCode::CREATED, "{uri}");
        }
    };
    post(
        format!("/api/docs/{source}/comments"),
        json!({"body": "Looks good", "anchor_text": "Plan"}),
    )
    .await;
    post(format!("/api/docs/{source}/shares"), json!({})).await;
    let granted = send_json(
        &w.app,
        &w.owner,
        "PUT",
        &format!("/api/docs/{source}/grants/user:{}", viewer.user_id),
        json!({"role": "editor", "inherit": true}),
    )
    .await;
    assert!(granted.status().is_success(), "grant");
    // Two history entries, each a snapshot of the page as it then was.
    let snapshots = PgSnapshotStore::new(w.pool().clone());
    let updates = PgUpdatesStore::new(w.pool().clone());
    for draft in ["First draft.\n", "Second draft.\n"] {
        w.set_markdown(source, draft).await;
        let state = w.stored(source).await;
        let txn = state.transact();
        snapshots
            .insert(
                source,
                updates.max_seq(source).await.unwrap(),
                &txn.encode_state_as_update_v1(&yrs::StateVector::default()),
                &yrs::updates::encoder::Encode::encode_v1(&txn.state_vector()),
            )
            .await
            .unwrap();
    }
    // The lists of `doc_id`, as the owner sees them.
    let world = &w;
    let lists = |doc_id: Uuid| async move {
        let mut lengths = Vec::new();
        for list in ["comments", "shares", "grants", "history"] {
            let (status, body) = world.fetch(&format!("/api/docs/{doc_id}/{list}")).await;
            assert_eq!(status, StatusCode::OK, "{list}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            lengths.push((list, body.as_array().unwrap().len()));
        }
        lengths
    };
    assert_eq!(
        lists(source).await,
        [
            ("comments", 1),
            ("shares", 1),
            ("grants", 1),
            ("history", 2)
        ]
    );

    let copy = id_of(&w.duplicate_ok(source, None).await);

    assert_eq!(
        lists(copy).await,
        [
            ("comments", 0),
            ("shares", 0),
            ("grants", 0),
            ("history", 0)
        ]
    );
    // The copy's whole history is the one update that holds its content.
    assert_eq!(updates.since(copy, 0).await.unwrap().len(), 1);
    assert!(w.markdown(copy).await.contains("Second draft."));
}

/// A copy is a copy: its checklist items are tasks again, for the same
/// assignees, and its Markdown is cached by the current renderer.
#[tokio::test(flavor = "multi_thread")]
async fn copied_tasks_are_indexed() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let child = w.create_doc(&w.owner, "Steps", Some(source)).await;
    let owner = w.owner.user_id;
    w.set_markdown(
        source,
        &format!("- [ ] [Olive Owner](knot://user/{owner}) ship it\n- [x] done already\n"),
    )
    .await;
    w.set_markdown(
        child,
        &format!("- [ ] [Olive Owner](knot://user/{owner}) review it\n"),
    )
    .await;

    let top = id_of(&w.duplicate_ok(source, Some(true)).await);
    let child_copy = w.order(Some(top)).await[0];
    // The copies are refreshed after the answer.
    w.wait_for_refreshes().await;

    let tasks = w.state.tasks.as_ref().unwrap();
    let listed = |doc: Uuid| async move {
        let mut rows: Vec<(i32, String, Option<Uuid>, bool)> = tasks
            .list_for_doc(doc)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.item_index, t.text, t.assignee_user_id, t.checked))
            .collect();
        rows.sort();
        rows
    };
    for (original, copy) in [(source, top), (child, child_copy)] {
        let tasks = listed(copy).await;
        assert!(!tasks.is_empty());
        assert_eq!(tasks, listed(original).await);
    }
    let mine: Vec<Uuid> = tasks
        .list_for_assignee(w.workspace_id, owner, false, &[])
        .await
        .unwrap()
        .into_iter()
        .map(|assigned| assigned.task.doc_id)
        .collect();
    assert!(
        mine.contains(&top) && mine.contains(&child_copy),
        "{mine:?}"
    );
    for copy in [top, child_copy] {
        let version: i32 =
            sqlx::query_scalar("SELECT renderer_version FROM doc_markdown_cache WHERE doc_id = $1")
                .bind(copy)
                .fetch_one(w.pool())
                .await
                .unwrap();
        assert_eq!(version, knot_markdown::RENDERER_VERSION);
    }
}

/// The answer comes as soon as the copy is stored. The refresh of its
/// derived data (cached Markdown and task rows, for up to 200 pages) runs
/// after it, in the background, so a large copy can't outlast the request
/// timeout and then be made a second time by a retry.
#[tokio::test(flavor = "multi_thread")]
async fn answers_once_the_copy_is_stored() {
    let (w, gate) = gated_world(|_| {}).await;
    let owner = w.owner.user_id;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    w.set_markdown(
        source,
        &format!("- [ ] [Olive Owner](knot://user/{owner}) ship it\n"),
    )
    .await;
    gate.arm();

    let response =
        tokio::time::timeout(Duration::from_secs(10), w.duplicate(&w.owner, source, None))
            .await
            .expect("the answer doesn't wait for the refresh of the copy");
    assert_eq!(response.status(), StatusCode::CREATED);
    let copy = id_of(&body_json(response).await);
    assert_eq!(w.doc(copy).await["title"], "Plan (copy)");

    // The refresh runs on, held at the copy's task rows.
    gate.entered().await;
    let tasks = w.state.tasks.as_ref().unwrap();
    assert!(tasks.list_for_doc(copy).await.unwrap().is_empty());
    gate.open();
    w.wait_for_refreshes().await;
    let copied: Vec<String> = tasks
        .list_for_doc(copy)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.text)
        .collect();
    assert_eq!(copied, ["ship it"]);
}

/// A duplicate holds every page it copies in memory, so at most
/// `MAX_CONCURRENT_DUPLICATES` run at once. One that gets no permit within
/// the busy wait is refused with 503 `doc.duplicate_busy` and copies
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn busy_duplicate_is_503() {
    assert_eq!(MAX_CONCURRENT_DUPLICATES, 2);
    let w = world_with(|state| {
        state.duplicate_permits = Arc::new(Semaphore::new(0));
        state.duplicate_busy_wait = Duration::from_millis(50);
    })
    .await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let rows = w.counts().await;

    let response = w.duplicate(&w.owner, source, None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );
    assert_eq!(error_code(response).await, "doc.duplicate_busy");
    assert_eq!(w.counts().await, rows, "nothing is copied");

    w.state.duplicate_permits.add_permits(1);
    w.duplicate_ok(source, None).await;
}

/// The checks that need no permit come before the wait for one: while
/// every permit is taken, a viewer still gets 403 `acl.editor_required`
/// and a template 422 `doc.is_template`, never 503. RED on HEAD: both got
/// 503 `doc.duplicate_busy`.
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_server_still_answers_a_viewer_and_a_template_with_their_own_codes() {
    let w = world_with(|state| {
        state.duplicate_permits = Arc::new(Semaphore::new(0));
        state.duplicate_busy_wait = Duration::from_millis(50);
    })
    .await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let template = w.create_doc(&w.owner, "Meeting notes", None).await;
    let response = send_json(
        &w.app,
        &w.owner,
        "POST",
        &format!("/api/docs/{template}/template"),
        json!({"is_template": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "make template");
    let viewer = w.member("viewer@duplicate.test", "viewer").await;
    let rows = w.counts().await;

    let response = w.duplicate(&viewer, source, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "acl.editor_required");
    let response = w.duplicate(&w.owner, template, None).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(response).await, "doc.is_template");
    assert_eq!(w.counts().await, rows, "nothing is copied");
}

/// A duplicate keeps its permit until the refresh of its copy ends, so
/// no more copies than permitted are ever held in memory, and gives it back
/// after a success and after a failure alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_holds_its_permit_until_its_refresh_ends() {
    let (w, gate) = gated_world(|state| {
        state.duplicate_permits = Arc::new(Semaphore::new(1));
        state.duplicate_busy_wait = Duration::from_millis(50);
    })
    .await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let template = w.create_doc(&w.owner, "Meeting notes", None).await;
    let response = send_json(
        &w.app,
        &w.owner,
        "POST",
        &format!("/api/docs/{template}/template"),
        json!({"is_template": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "make template");
    gate.arm();

    let response =
        tokio::time::timeout(Duration::from_secs(10), w.duplicate(&w.owner, source, None))
            .await
            .expect("the answer doesn't wait for the refresh of the copy");
    assert_eq!(response.status(), StatusCode::CREATED);
    gate.entered().await;
    assert_eq!(
        w.state.duplicate_permits.available_permits(),
        0,
        "the refresh holds the permit"
    );
    let response = w.duplicate(&w.owner, source, None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(response).await, "doc.duplicate_busy");

    // After a success: back once the refresh ends.
    gate.open();
    w.wait_for_permits(1).await;
    // After a failure.
    let response = w.duplicate(&w.owner, template, None).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    w.wait_for_permits(1).await;
    w.duplicate_ok(source, None).await;
    w.wait_for_permits(1).await;
}

/// A writer that skips the group's lock (an older release during an
/// upgrade) takes the copy's order key while the copy is stored. The
/// duplicate answers 409 `doc.sort_key_conflict`, which a retry gets past,
/// rather than a database error, and stores nothing.
#[tokio::test(flavor = "multi_thread")]
async fn order_key_taken_meanwhile_is_409() {
    let w = world().await;
    let source = w.create_doc(&w.owner, "Plan", None).await;
    let source_key = w.doc(source).await["sort_key"]
        .as_str()
        .unwrap()
        .to_string();
    // The copy's key: right after the source, the last page at the top
    // level. The old release hasn't committed, so the copy's placement
    // doesn't see it.
    let taken = knot_storage::sort_key_between(Some(&source_key), None);
    let mut rows = w.counts().await;
    let mut old_release = w.pool().begin().await.unwrap();
    sqlx::query(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, NULL, 'Old release', $2, $3)",
    )
    .bind(w.workspace_id)
    .bind(&taken)
    .bind(w.owner.user_id)
    .execute(&mut *old_release)
    .await
    .unwrap();
    let duplicate = {
        let (app, owner) = (w.app.clone(), w.owner.clone());
        tokio::spawn(async move {
            send_json(
                &app,
                &owner,
                "POST",
                &format!("/api/docs/{source}/duplicate"),
                json!({}),
            )
            .await
        })
    };
    // The copy's final update waits for the old release's row, which holds
    // the same key.
    knot_test_support::locks::wait_for_lock_waits(w.pool(), 1).await;
    old_release.commit().await.unwrap();

    let response = duplicate.await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "doc.sort_key_conflict");
    assert_eq!(rows[0].0, "documents");
    rows[0].1 += 1; // the old release's page
    assert_eq!(w.counts().await, rows, "the copy stored nothing");
    assert_eq!(w.order(None).await.len(), 2);
}

/// The hourly purge job removes the objects of duplicates that never
/// committed once they are an hour old (P16), and leaves younger ones.
#[tokio::test(flavor = "multi_thread")]
async fn purge_job_removes_stale_copies() {
    use knot_server::archive::run_purge_once;
    let mut memory = None;
    let w = world_with(|state| {
        let store = Arc::new(MemoryBlobs::default());
        state.blob_store = Some(store.clone());
        memory = Some(store);
    })
    .await;
    let store = memory.unwrap();
    let (stale, recent) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [stale, recent] {
        store.put(id, TINY_PNG, "image/png").await.unwrap();
    }
    knot_storage::duplicate::record_pending_copies(w.pool(), &[stale, recent])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE blob_copy_pending SET created_at = now() - interval '61 minutes'
          WHERE blob_id = $1",
    )
    .bind(stale)
    .execute(w.pool())
    .await
    .unwrap();

    run_purge_once(&w.state, chrono::Duration::days(30))
        .await
        .unwrap();

    assert!(store.get(stale).await.is_err(), "the stale copy is deleted");
    assert!(
        store.get(recent).await.is_ok(),
        "a running duplicate's copy stays"
    );
    assert_eq!(w.pending().await, HashSet::from([recent]));
}

// ---------------------------------------------------------------------------
// Restricted pages (spec: restricted documents, "Duplicate")
// ---------------------------------------------------------------------------

impl World {
    /// `PUT /api/docs/{doc_id}/restricted` as `who`, which must succeed.
    async fn restrict(&self, who: &Session, doc_id: Uuid) {
        let response = send_json(
            &self.app,
            who,
            "PUT",
            &format!("/api/docs/{doc_id}/restricted"),
            json!({"restricted": true}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "restrict {doc_id}");
    }

    /// Grant `grantee` `role` on `doc_id`, as the owner.
    async fn grant(&self, doc_id: Uuid, grantee: &Session, role: &str, inherit: bool) {
        let response = send_json(
            &self.app,
            &self.owner,
            "PUT",
            &format!("/api/docs/{doc_id}/grants/user:{}", grantee.user_id),
            json!({"role": role, "inherit": inherit}),
        )
        .await;
        assert!(response.status().is_success(), "grant on {doc_id}");
    }

    /// `GET` `uri` as `who`: its status and body.
    async fn fetch_as(&self, who: &Session, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = send_raw(&self.app, Some(who), "GET", uri, None, Vec::new()).await;
        let status = response.status();
        (status, body_bytes(response).await)
    }

    /// The titles of the pages `who` created.
    async fn titles_created_by(&self, who: &Session) -> Vec<String> {
        sqlx::query_scalar("SELECT title FROM documents WHERE created_by = $1 ORDER BY title")
            .bind(who.user_id)
            .fetch_all(self.pool())
            .await
            .unwrap()
    }

    /// The children of `parent` as stored, whoever can read them.
    async fn stored_children(&self, parent: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM documents WHERE parent_id = $1 ORDER BY sort_key")
            .bind(parent)
            .fetch_all(self.pool())
            .await
            .unwrap()
    }
}

/// Subpages the user can't read are left out of the copy, and so is every
/// page below them, even one the user can read through a grant of its own:
/// its parent is not copied.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_skips_unreadable_restricted_subpages() {
    let w = world().await;
    let editor = w.member("editor@duplicate.test", "editor").await;
    let source = w.create_doc(&w.owner, "Handbook", None).await;
    w.create_doc(&w.owner, "Visible chapter", Some(source))
        .await;
    let secret = w.create_doc(&w.owner, "Secret chapter", Some(source)).await;
    let below_secret = w
        .create_doc(&w.owner, "Below the secret", Some(secret))
        .await;
    w.restrict(&w.owner, secret).await;
    // Readable on its own, but its restricted parent is not.
    w.grant(below_secret, &editor, "editor", true).await;

    let response = w.duplicate(&editor, source, Some(true)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let top = id_of(&body_json(response).await);

    let copied_children = w.stored_children(top).await;
    assert_eq!(copied_children.len(), 1, "only the readable chapter");
    assert_eq!(w.doc(copied_children[0]).await["title"], "Visible chapter");
    assert!(w.stored_children(copied_children[0]).await.is_empty());
    assert_eq!(
        w.titles_created_by(&editor).await,
        ["Handbook (copy)", "Visible chapter"],
        "nothing of the restricted chapter is copied"
    );
}

/// A copy of a restricted page is restricted too, and its copier owns it:
/// the workspace role, the owner's included, gives no access to it.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_of_boundary_stays_restricted_and_actor_is_owner() {
    let w = world().await;
    let editor = w.member("editor@duplicate.test", "editor").await;
    let member = w.member("member@duplicate.test", "editor").await;
    let boundary = w.create_doc(&w.owner, "Board minutes", None).await;
    let child = w.create_doc(&w.owner, "January", Some(boundary)).await;
    w.restrict(&w.owner, boundary).await;
    w.grant(boundary, &editor, "editor", true).await;

    let response = w.duplicate(&editor, boundary, Some(true)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let copy = body_json(response).await;
    assert_eq!(copy["restricted"], true, "{copy}");
    let copy_id = id_of(&copy);
    let copied_children = w.stored_children(copy_id).await;
    assert_eq!(copied_children.len(), 1);
    assert_ne!(copied_children[0], child);

    for doc_id in [copy_id, copied_children[0]] {
        for outsider in [&member, &w.owner] {
            let (status, body) = w.fetch_as(outsider, &format!("/api/docs/{doc_id}")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{doc_id}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "acl.no_grant");
        }
    }
    let (status, grants) = w
        .fetch_as(&editor, &format!("/api/docs/{copy_id}/grants"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let grants: Value = serde_json::from_slice(&grants).unwrap();
    assert_eq!(
        grants.as_array().unwrap().len(),
        1,
        "no other grant is copied: {grants}"
    );
    assert_eq!(grants[0]["principal"], format!("user:{}", editor.user_id));
    assert_eq!(grants[0]["role"], "owner");
    assert_eq!(grants[0]["inherit"], true);
}

/// A board and a file that belong to a restricted page are shown by an
/// unrestricted one. Its copy keeps referencing them rather than getting
/// copies of its own, which the whole workspace could read.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_does_not_rehome_restricted_board_or_blob() {
    let w = world().await;
    let member = w.member("member@duplicate.test", "editor").await;
    let secret = w.create_doc(&w.owner, "Secret", None).await;
    let board = w.board(secret).await;
    let file = w.upload(secret).await;
    w.restrict(&w.owner, secret).await;
    let source = w.create_doc(&w.owner, "Public", None).await;
    let content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(file))]);
        node(
            txn,
            fragment,
            "excalidraw_board",
            &[("board_id", board.to_string())],
        );
    });
    w.set_content(source, &content).await;

    let copy = id_of(&w.duplicate_ok(source, None).await);

    assert!(w.boards_of(copy).await.is_empty(), "the board stays");
    assert!(w.blobs_of(copy).await.is_empty(), "the file stays");
    let copy_content = w.stored(copy).await;
    assert_eq!(
        attr_values(&copy_content, "image", "src"),
        vec![blob_url(file)]
    );
    assert_eq!(
        attr_values(&copy_content, "excalidraw_board", "board_id"),
        vec![board.to_string()]
    );
    for uri in [format!("/api/boards/{board}/svg"), blob_url(file)] {
        let (status, _) = w.fetch_as(&member, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

/// A copy goes under the source's parent, so the user needs to be able to
/// edit there, not to hold an editor workspace role: a viewer with an editor
/// grant on the parent may copy its subpage. A parent the user can't read
/// answers as an unknown one.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_under_a_parent_needs_editor_there() {
    let w = world().await;
    let viewer = w.member("viewer@duplicate.test", "viewer").await;
    let parent = w.create_doc(&w.owner, "Team", None).await;
    let source = w.create_doc(&w.owner, "Notes", Some(parent)).await;
    w.grant(parent, &viewer, "editor", true).await;

    let response = w.duplicate(&viewer, source, None).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // Only the subpage readable: its restricted parent is hidden.
    let hidden_parent = w.create_doc(&w.owner, "Hidden", None).await;
    let deep = w.create_doc(&w.owner, "Deep", Some(hidden_parent)).await;
    w.restrict(&w.owner, hidden_parent).await;
    w.grant(deep, &viewer, "editor", true).await;
    let pages = w.counts().await;
    let response = w.duplicate(&viewer, deep, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "doc.not_found");
    assert_eq!(w.counts().await, pages);
}
