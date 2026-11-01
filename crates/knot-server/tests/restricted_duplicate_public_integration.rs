//! Restricted documents: what a duplicate does with the boards and files a
//! page shows from a page it doesn't copy, and how `/p/*` answers a link
//! whose doc was moved into a restricted subtree. Against a fresh database
//! per test, with a real rooms registry.
//!
//! See docs/superpowers/specs/2026-10-30-restricted-documents-design.md.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, ReadTxn, Transact, TransactionMut, Update, Xml, XmlElementPrelim, XmlElementRef,
    XmlFragment, XmlFragmentRef, XmlOut,
};

// ---------------------------------------------------------------------------
// Harness, copied from duplicate_integration.rs.
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

/// A workspace with one owner and a real rooms registry.
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
        .create_local("owner@restricted.test", "Olive Owner", &hash)
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
    let owner = login(&app, "owner@restricted.test", owner.id).await;
    World { state, app, owner }
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

fn id_of(doc: &Value) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn blob_url(id: Uuid) -> String {
    format!("/api/blobs/{id}")
}

/// A response as a whole: status, headers and body.
type WholeResponse = (StatusCode, HeaderMap, Vec<u8>);

impl World {
    fn pool(&self) -> &knot_storage::Pool {
        self.state.pool.as_ref().unwrap()
    }

    /// Create a page as `who`, first under `parent`.
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

    /// Replace the page's content with `content`'s state through the room.
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
        const BOUNDARY: &str = "----RestrictedTestBoundary";
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

    /// A board on `doc_id` with an SVG preview.
    async fn board(&self, doc_id: Uuid) -> Uuid {
        let boards = self.state.boards.as_ref().unwrap();
        let board = boards
            .create(doc_id, self.owner.user_id, Some("Budget".into()))
            .await
            .unwrap();
        boards
            .set_svg(board.id, b"<svg>budget</svg>")
            .await
            .unwrap();
        board.id
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

    /// Restrict `doc_id` as `who`.
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

    /// Move `doc_id` to the end of `parent`, as the owner.
    async fn move_doc(&self, doc_id: Uuid, parent: Uuid) {
        let response = send_json(
            &self.app,
            &self.owner,
            "POST",
            &format!("/api/docs/{doc_id}/move"),
            json!({"parent_id": parent}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "move {doc_id}");
    }

    /// `POST /api/docs/{doc_id}/duplicate` by the owner, which must succeed;
    /// returns the copy.
    async fn duplicate_ok(&self, doc_id: Uuid) -> Value {
        let response = send_json(
            &self.app,
            &self.owner,
            "POST",
            &format!("/api/docs/{doc_id}/duplicate"),
            json!({}),
        )
        .await;
        let status = response.status();
        let body = body_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "duplicate: {body}");
        body
    }

    /// A public link to `doc_id`, made by the owner: `(share id, token)`.
    async fn share(&self, doc_id: Uuid) -> (String, String) {
        let response = send_json(
            &self.app,
            &self.owner,
            "POST",
            &format!("/api/docs/{doc_id}/shares"),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "share {doc_id}");
        let share = body_json(response).await;
        (
            share["id"].as_str().unwrap().to_string(),
            share["token"].as_str().unwrap().to_string(),
        )
    }

    /// Revoke the public link `share_id` of `doc_id`, as the owner.
    async fn revoke_share(&self, doc_id: Uuid, share_id: &str) {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "DELETE",
            &format!("/api/docs/{doc_id}/shares/{share_id}"),
            None,
            Vec::new(),
        )
        .await;
        assert!(response.status().is_success(), "revoke {share_id}");
    }

    /// `GET` `uri` as `who`: its status and body.
    async fn fetch_as(&self, who: &Session, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = send_raw(&self.app, Some(who), "GET", uri, None, Vec::new()).await;
        let status = response.status();
        (status, body_bytes(response).await)
    }

    /// `GET` `uri` anonymously, as a whole response.
    async fn fetch_anonymously(&self, uri: &str) -> WholeResponse {
        let response = send_raw(&self.app, None, "GET", uri, None, Vec::new()).await;
        let status = response.status();
        let headers = response.headers().clone();
        (status, headers, body_bytes(response).await)
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
}

fn apply(doc: &yrs::Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).unwrap())
        .unwrap();
}

/// A page whose content `build` writes into its fragment.
fn page(build: impl FnOnce(&mut TransactionMut, &XmlFragmentRef)) -> yrs::Doc {
    let doc = yrs::Doc::new();
    let fragment = doc.get_or_insert_xml_fragment("default");
    build(&mut doc.transact_mut(), &fragment);
    doc
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

// ---------------------------------------------------------------------------
// Duplicate
// ---------------------------------------------------------------------------

/// Inside one restriction boundary readers differ by branch. A page on the
/// "Legal" branch shows a board and a file of a page on the "Finance"
/// branch, which a Legal reader can't read. Its copy keeps referencing them
/// rather than getting copies of its own, which that reader could read.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_does_not_rehome_asset_from_other_branch_of_boundary() {
    let w = world().await;
    let legal_reader = w.member("legal@restricted.test", "editor").await;
    let boundary = w.create_doc(&w.owner, "Board room", None).await;
    let finance = w.create_doc(&w.owner, "Finance", Some(boundary)).await;
    let legal = w.create_doc(&w.owner, "Legal", Some(boundary)).await;
    let budget = w.create_doc(&w.owner, "Budget", Some(finance)).await;
    let budget_board = w.board(budget).await;
    let budget_file = w.upload(budget).await;
    w.restrict(&w.owner, boundary).await;
    w.grant(legal, &legal_reader, "viewer", true).await;
    let memo = w.create_doc(&w.owner, "Memo", Some(legal)).await;
    let content = page(|txn, fragment| {
        node(txn, fragment, "image", &[("src", blob_url(budget_file))]);
        node(
            txn,
            fragment,
            "excalidraw_board",
            &[("board_id", budget_board.to_string())],
        );
    });
    w.set_content(memo, &content).await;

    let assets = [
        format!("/api/boards/{budget_board}/svg"),
        blob_url(budget_file),
    ];
    for uri in &assets {
        let (status, _) = w.fetch_as(&legal_reader, uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "before the copy: {uri}");
    }

    let copy = id_of(&w.duplicate_ok(memo).await);

    let (status, _) = w
        .fetch_as(&legal_reader, &format!("/api/docs/{copy}"))
        .await;
    assert_eq!(status, StatusCode::OK, "the Legal reader reads the copy");
    assert!(w.boards_of(copy).await.is_empty(), "the board stays");
    assert!(w.blobs_of(copy).await.is_empty(), "the file stays");
    let copy_content = w.stored(copy).await;
    assert_eq!(
        attr_values(&copy_content, "image", "src"),
        vec![blob_url(budget_file)]
    );
    assert_eq!(
        attr_values(&copy_content, "excalidraw_board", "board_id"),
        vec![budget_board.to_string()]
    );
    for uri in &assets {
        let (status, _) = w.fetch_as(&legal_reader, uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "after the copy: {uri}");
    }
}

// ---------------------------------------------------------------------------
// Public links
// ---------------------------------------------------------------------------

/// A link whose doc was moved into a restricted subtree answers every `/p/*`
/// route exactly like a revoked link: same status, headers and body, so it
/// doesn't reveal that the doc still exists.
#[tokio::test(flavor = "multi_thread")]
async fn public_routes_answer_restricted_link_like_revoked_one() {
    let w = world().await;
    let shared_page = w.create_doc(&w.owner, "Shared", None).await;
    let shared_file = w.upload(shared_page).await;
    let shared_board = w.board(shared_page).await;
    let (_, moved_token) = w.share(shared_page).await;
    let revoked_page = w.create_doc(&w.owner, "Revoked", None).await;
    let (revoked_share_id, revoked_token) = w.share(revoked_page).await;
    w.revoke_share(revoked_page, &revoked_share_id).await;
    let vault = w.create_doc(&w.owner, "Vault", None).await;
    w.restrict(&w.owner, vault).await;

    let routes = |token: &str| {
        [
            format!("/p/{token}"),
            format!("/p/{token}/blobs/{shared_file}"),
            format!("/p/{token}/boards/{shared_board}/svg"),
        ]
    };
    for uri in routes(&moved_token).into_iter().skip(1) {
        let (status, _, _) = w.fetch_anonymously(&uri).await;
        assert_eq!(status, StatusCode::OK, "alive before the move: {uri}");
    }

    w.move_doc(shared_page, vault).await;

    for (moved_uri, revoked_uri) in routes(&moved_token).into_iter().zip(routes(&revoked_token)) {
        let moved_response = w.fetch_anonymously(&moved_uri).await;
        let revoked_response = w.fetch_anonymously(&revoked_uri).await;
        assert_ne!(moved_response.0, StatusCode::OK, "{moved_uri}");
        assert_eq!(moved_response, revoked_response, "{moved_uri}");
    }
}
