//! Page export to DOCX/ODT: the export service (snapshot, asset resolution,
//! build) and its routes, against a fresh database per test, with a real
//! rooms registry.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_export::{
    Block, ExportDoc, Fallbacks, ImageAsset, ImageSource, Inline, LinkTarget, Marks, Run,
};
use knot_server::export::{self, Format};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    BlobStore, BlobStoreError, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore,
    WorkspaceRole,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::{Any, ReadTxn, Transact, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim};

// ---------------------------------------------------------------------------
// Harness
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

/// Valid 3×2 RGB PNG; the distinct sides show width and height are read.
const PNG_3X2: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0x12, 0x16, 0xF1,
    0x4D, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x38, 0x21, 0x27, 0x07,
    0x41, 0x0C, 0x70, 0x16, 0x00, 0x41, 0x00, 0x06, 0x19, 0xD4, 0x44, 0xEE, 0x19, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

const WEBP_2X2: &[u8] = include_bytes!("../../knot-export/tests/fixtures/2x2.webp");

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
        .create_local("owner@export.test", "Olive Owner", &hash)
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
    let owner = login(&app, "owner@export.test", owner.id).await;
    World {
        state,
        app,
        owner,
        workspace_id: workspace.id,
    }
}

/// A real rooms registry on the test database (copied from
/// `markdown_import_integration.rs`): export snapshots the live room.
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

impl World {
    async fn create_doc(&self, title: &str) -> Uuid {
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            "/api/docs",
            Some("application/json"),
            json!({"title": title, "parent_id": null})
                .to_string()
                .into_bytes(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "create {title}");
        let body = body_json(response).await;
        body["id"].as_str().unwrap().parse().unwrap()
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
    /// for shapes the markdown import cannot produce (unknown elements).
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

    /// Upload `bytes` as a blob of `doc_id`; returns its `/api/blobs/<id>` url.
    async fn upload(&self, doc_id: Uuid, content_type: &str, bytes: &[u8]) -> String {
        let (multipart_type, body) = multipart(&[("file", bytes)], content_type);
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            &format!("/api/docs/{doc_id}/blobs"),
            Some(&multipart_type),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "upload");
        body_json(response).await["url"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Store a blob directly, bypassing the upload route's ACL.
    async fn insert_blob(&self, workspace_id: Uuid, doc_id: Uuid, bytes: &[u8]) -> Uuid {
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
                byte_size: bytes.len() as i64,
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
            .put(id, bytes, "image/png")
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
            .create_local("outsider@export.test", "Outsider", "$h$")
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
        let response = send_raw(
            &self.app,
            Some(&self.owner),
            "POST",
            "/api/workspace/members",
            Some("application/json"),
            json!({"email": email, "role": role, "password": PASSWORD, "display_name": role})
                .to_string()
                .into_bytes(),
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

    /// `GET /api/docs/{doc_id}/export/diagrams` as `who`.
    async fn diagrams(&self, who: &Session, doc_id: Uuid) -> Response {
        send_raw(
            &self.app,
            Some(who),
            "GET",
            &format!("/api/docs/{doc_id}/export/diagrams"),
            None,
            Vec::new(),
        )
        .await
    }

    /// The diagram keys the listing gives for `doc_id`, in its order.
    async fn diagram_keys(&self, doc_id: Uuid) -> Vec<String> {
        let response = self.diagrams(&self.owner, doc_id).await;
        assert_eq!(response.status(), StatusCode::OK, "diagrams listing");
        body_json(response).await["diagrams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["key"].as_str().unwrap().to_string())
            .collect()
    }

    /// `POST /api/docs/{doc_id}/export/{format}` as `who`, with one PNG part
    /// per `(name, bytes)`, or an empty body without a content type when
    /// there are none.
    async fn export(
        &self,
        who: &Session,
        doc_id: Uuid,
        format: &str,
        pictures: &[(&str, &[u8])],
    ) -> Response {
        let (content_type, body) = if pictures.is_empty() {
            (None, Vec::new())
        } else {
            let (content_type, body) = multipart(pictures, "image/png");
            (Some(content_type), body)
        };
        send_raw(
            &self.app,
            Some(who),
            "POST",
            &format!("/api/docs/{doc_id}/export/{format}"),
            content_type.as_deref(),
            body,
        )
        .await
    }
}

/// A blob store whose reads wait for the gate: each `get` tells `entered`
/// it started, then waits until `open` has a permit. Holds an export in the
/// middle of its build for as long as a test needs.
struct GatedBlobs {
    inner: Arc<dyn BlobStore>,
    entered: Arc<Notify>,
    open: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl BlobStore for GatedBlobs {
    async fn put(&self, id: Uuid, bytes: &[u8], content_type: &str) -> Result<(), BlobStoreError> {
        self.inner.put(id, bytes, content_type).await
    }

    async fn get(&self, id: Uuid) -> Result<Vec<u8>, BlobStoreError> {
        self.entered.notify_one();
        drop(self.open.acquire().await.expect("gate"));
        self.inner.get(id).await
    }

    async fn delete(&self, id: Uuid) -> Result<(), BlobStoreError> {
        self.inner.delete(id).await
    }

    fn bytes_deleted_with_metadata(&self) -> bool {
        self.inner.bytes_deleted_with_metadata()
    }
}

/// A `multipart/form-data` body with one `content_type` part per
/// `(name, bytes)`; returns the request's content type and the body.
fn multipart(parts: &[(&str, &[u8])], content_type: &str) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "----ExportTestBoundary";
    let mut body = Vec::new();
    for (name, bytes) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"picture\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// A PNG of exactly `len` bytes: `TINY_PNG` padded after its end chunk, so
/// it still reads as a 1×1 picture.
fn padded_png(len: usize) -> Vec<u8> {
    let mut png = TINY_PNG.to_vec();
    png.resize(len, 0);
    png
}

fn header<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .to_str()
        .unwrap()
}

fn fallbacks_header(response: &Response) -> &str {
    header(response, "x-knot-export-fallbacks")
}

async fn error_code(response: Response) -> String {
    body_json(response).await["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// One zip entry's bytes, or `None` when absent.
fn zip_entry(zip: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut file = archive.by_name(name).ok()?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    Some(out)
}

/// The names of all zip entries starting with `prefix`.
fn zip_names(zip: &[u8], prefix: &str) -> Vec<String> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    archive
        .file_names()
        .filter(|name| name.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

fn zip_text(zip: &[u8], name: &str) -> String {
    String::from_utf8(zip_entry(zip, name).unwrap_or_else(|| panic!("{name} missing"))).unwrap()
}

fn image(id: Uuid) -> Block {
    Block::Image {
        source: ImageSource::Blob(id.to_string()),
        alt: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn resolve_assets_checks_blob_access_and_budget() {
    let world = world().await;
    let page = world.create_doc("Page").await;
    let (other_ws, other_doc) = world.foreign_doc().await;
    let foreign = world.insert_blob(other_ws, other_doc, PNG_3X2).await;
    let readable = world.insert_blob(world.workspace_id, page, PNG_3X2).await;
    let missing = Uuid::new_v4();
    let doc = ExportDoc {
        blocks: vec![image(foreign), image(readable), image(missing)],
    };
    let user = world.owner.user_id;

    let assets = export::resolve_assets(&world.state, user, &doc, HashMap::new())
        .await
        .unwrap();
    assert!(matches!(
        assets.images[&foreign.to_string()],
        ImageAsset::Unavailable
    ));
    assert!(matches!(
        assets.images[&missing.to_string()],
        ImageAsset::Unavailable
    ));
    match &assets.images[&readable.to_string()] {
        ImageAsset::Embedded(picture) => {
            assert_eq!((picture.width_px, picture.height_px), (3, 2));
            assert_eq!(picture.bytes, PNG_3X2);
        }
        other => panic!("readable PNG: {other:?}"),
    }

    let budget = PNG_3X2.len() as i64 - 1;
    let assets =
        export::resolve_assets_with_budget(&world.state, user, &doc, HashMap::new(), budget)
            .await
            .unwrap();
    assert!(matches!(
        assets.images[&readable.to_string()],
        ImageAsset::OverBudget
    ));
}

/// The budget is a running total over the page: once earlier images have
/// spent it, an image that alone would fit is still refused. A smaller one
/// after it that fits the remainder is embedded, so the budget skips images
/// rather than stopping at the first refusal.
#[tokio::test(flavor = "multi_thread")]
async fn image_budget_is_a_running_total() {
    let world = world().await;
    let page = world.create_doc("Pictures").await;
    let first = world.insert_blob(world.workspace_id, page, PNG_3X2).await;
    let second = world.insert_blob(world.workspace_id, page, PNG_3X2).await;
    let smaller = world.insert_blob(world.workspace_id, page, TINY_PNG).await;
    let doc = ExportDoc {
        blocks: vec![image(first), image(second), image(smaller)],
    };
    // Room for one PNG_3X2 plus TINY_PNG, but not for two PNG_3X2.
    let budget = 2 * PNG_3X2.len() as i64 - 1;
    assert!((PNG_3X2.len() + TINY_PNG.len()) as i64 <= budget);

    let user = world.owner.user_id;
    let assets =
        export::resolve_assets_with_budget(&world.state, user, &doc, HashMap::new(), budget)
            .await
            .unwrap();
    assert!(
        matches!(assets.images[&first.to_string()], ImageAsset::Embedded(_)),
        "first: {:?}",
        assets.images[&first.to_string()]
    );
    assert!(
        matches!(assets.images[&second.to_string()], ImageAsset::OverBudget),
        "second: {:?}",
        assets.images[&second.to_string()]
    );
    assert!(
        matches!(assets.images[&smaller.to_string()], ImageAsset::Embedded(_)),
        "smaller: {:?}",
        assets.images[&smaller.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_counts_each_fallback_kind() {
    let world = world().await;
    let page = world.create_doc("Fallbacks").await;
    let owner = world.owner.user_id;
    let (other_ws, other_doc) = world.foreign_doc().await;
    let foreign = world.insert_blob(other_ws, other_doc, PNG_3X2).await;

    // An unreadable image, a board nobody supplied a picture for, and an
    // element the walker does not know.
    let content = yrs::Doc::new();
    let fragment = content.get_or_insert_xml_fragment("default");
    {
        let mut txn = content.transact_mut();
        let picture = fragment.push_back(&mut txn, XmlElementPrelim::empty("image"));
        picture.insert_attribute(&mut txn, "src", Any::from(format!("/api/blobs/{foreign}")));
        let board = fragment.push_back(&mut txn, XmlElementPrelim::empty("excalidraw_board"));
        board.insert_attribute(&mut txn, "board_id", Any::from(Uuid::new_v4().to_string()));
        let callout = fragment.push_back(&mut txn, XmlElementPrelim::empty("callout"));
        callout.push_back(&mut txn, XmlTextPrelim::new("Heads up"));
    }
    world.set_content(page, &content).await;

    let snapshot = export::snapshot(&world.state, page).await.unwrap();
    let walked = knot_export::walk(snapshot.inner());
    assert_eq!(walked.unknown_elements, ["callout"]);

    for format in [Format::Docx, Format::Odt] {
        let built = export::build(&world.state, owner, page, format, HashMap::new())
            .await
            .unwrap();
        let fallbacks = built.fallbacks;
        assert_eq!(fallbacks.image_unavailable, 1, "{format:?}: {fallbacks:?}");
        assert!(fallbacks.diagram >= 1, "{format:?}: {fallbacks:?}");
        assert_eq!(fallbacks.image_over_budget, 0, "{format:?}: {fallbacks:?}");
        assert_eq!(
            fallbacks.unknown_element,
            walked.unknown_elements.len() as u32,
            "{format:?}: {fallbacks:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_assets_keeps_page_diagrams_and_names_mentions() {
    let world = world().await;
    let owner = world.owner.user_id;
    let gone = Uuid::new_v4();
    let mention = |id: Uuid| Inline::Link {
        target: LinkTarget::User(id.to_string()),
        runs: vec![Run {
            text: "@Old".into(),
            marks: Marks::default(),
        }],
    };
    let doc = ExportDoc {
        blocks: vec![
            Block::Mermaid {
                key: "mermaid-abc".into(),
                source: "graph TD; A-->B".into(),
            },
            Block::Mermaid {
                key: "mermaid-not-png".into(),
                source: "graph TD; B-->C".into(),
            },
            Block::Paragraph(vec![mention(owner), mention(gone)]),
        ],
    };
    let pictures = HashMap::from([
        ("mermaid-abc".to_string(), TINY_PNG.to_vec()),
        ("mermaid-not-png".to_string(), b"GIF89a".to_vec()),
        ("mermaid-not-on-page".to_string(), TINY_PNG.to_vec()),
    ]);

    let assets = export::resolve_assets(&world.state, owner, &doc, pictures)
        .await
        .unwrap();
    let keys: Vec<&String> = assets.diagrams.keys().collect();
    assert_eq!(keys, ["mermaid-abc"]);
    assert_eq!(assets.diagrams["mermaid-abc"].scale, 2);
    assert_eq!(
        assets.user_names,
        HashMap::from([(owner.to_string(), "Olive Owner".to_string())])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn converted_webp_is_charged_at_its_prepared_size() {
    let world = world().await;
    let page = world.create_doc("Pictures").await;
    let url = world.upload(page, "image/webp", WEBP_2X2).await;
    let id = url.trim_start_matches("/api/blobs/").to_string();
    let doc = ExportDoc {
        blocks: vec![Block::Image {
            source: ImageSource::Blob(id.clone()),
            alt: String::new(),
        }],
    };
    let prepared = knot_export::prepare_image(WEBP_2X2).unwrap();
    assert!(
        prepared.bytes.len() > WEBP_2X2.len(),
        "the PNG outgrows the WebP"
    );

    // The stored size fits; the converted PNG does not.
    let budget = WEBP_2X2.len() as i64;
    let user = world.owner.user_id;
    let assets =
        export::resolve_assets_with_budget(&world.state, user, &doc, HashMap::new(), budget)
            .await
            .unwrap();
    assert!(matches!(assets.images[&id], ImageAsset::OverBudget));

    let budget = prepared.bytes.len() as i64;
    let assets =
        export::resolve_assets_with_budget(&world.state, user, &doc, HashMap::new(), budget)
            .await
            .unwrap();
    assert!(matches!(assets.images[&id], ImageAsset::Embedded(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn webp_blob_is_converted_to_png() {
    let world = world().await;
    let page = world.create_doc("Pictures").await;
    let url = world.upload(page, "image/webp", WEBP_2X2).await;
    world.set_markdown(page, &format!("![w]({url})\n")).await;

    let built = export::build(
        &world.state,
        world.owner.user_id,
        page,
        Format::Docx,
        HashMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(built.fallbacks, Fallbacks::default());
    let png = zip_entry(&built.bytes, "word/media/image1.png").expect("word/media/image1.png");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn page_links_use_base_url() {
    let world = world_with(|s| s.base_url = "https://knot.example.test".into()).await;
    let page = world.create_doc("Links").await;
    let other = world.create_doc("Other").await;
    world
        .set_markdown(page, &format!("See [x](knot://doc/{other}).\n"))
        .await;

    let built = export::build(
        &world.state,
        world.owner.user_id,
        page,
        Format::Docx,
        HashMap::new(),
    )
    .await
    .unwrap();
    let rels = zip_text(&built.bytes, "word/_rels/document.xml.rels");
    let target = format!(r#"Target="{}/doc/{other}""#, world.state.base_url);
    assert!(rels.contains(&target), "{target} in {rels}");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_uses_live_state_and_current_display_names() {
    let world = world().await;
    let page = world.create_doc("Live page").await;
    let owner = world.owner.user_id;
    world
        .set_markdown(
            page,
            &format!("# Title\n\nHi [@Old](knot://user/{owner})\n"),
        )
        .await;

    for format in [Format::Docx, Format::Odt] {
        let built = export::build(&world.state, owner, page, format, HashMap::new())
            .await
            .unwrap();
        assert!(built.bytes.starts_with(b"PK"), "{format:?}");
        assert_eq!(built.title, "Live page");
        let body = match format {
            Format::Docx => zip_text(&built.bytes, "word/document.xml"),
            Format::Odt => zip_text(&built.bytes, "content.xml"),
        };
        assert!(body.contains("Hi "), "{format:?}: {body}");
        assert!(body.contains("@Olive Owner"), "{format:?}: {body}");
        assert!(!body.contains("@Old"), "{format:?}: {body}");
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

const DOCX_TYPE: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const ODT_TYPE: &str = "application/vnd.oasis.opendocument.text";
const NO_FALLBACKS: &str = "diagrams=0, images=0, unknown=0";

#[tokio::test(flavor = "multi_thread")]
async fn viewer_can_export_docx_and_odt() {
    let world = world().await;
    let viewer = world.member("viewer@export.test", "viewer").await;
    let page = world.create_doc("Plan").await;
    world.set_markdown(page, "# Plan\n\nHello.\n").await;

    for (format, content_type) in [("docx", DOCX_TYPE), ("odt", ODT_TYPE)] {
        let response = world.export(&viewer, page, format, &[]).await;
        assert_eq!(response.status(), StatusCode::OK, "{format}");
        assert_eq!(header(&response, "content-type"), content_type);
        let disposition = header(&response, "content-disposition");
        assert!(disposition.starts_with("attachment; "), "{disposition}");
        assert!(
            disposition.contains(&format!("filename*=UTF-8''Plan.{format}")),
            "{disposition}"
        );
        assert_eq!(fallbacks_header(&response), NO_FALLBACKS);
        assert!(body_bytes(response).await.starts_with(b"PK"), "{format}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn template_pages_export() {
    let world = world().await;
    let page = world.create_doc("Meeting notes").await;
    world
        .state
        .docs
        .as_ref()
        .unwrap()
        .set_template(world.workspace_id, page, world.owner.user_id, true)
        .await
        .unwrap();

    let response = world.export(&world.owner, page, "odt", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_bytes(response).await.starts_with(b"PK"));
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_pages_are_refused() {
    let world = world().await;
    let viewer = world.member("viewer@export.test", "viewer").await;
    let editor = world.member("editor@export.test", "editor").await;
    let page = world.create_doc("Old plan").await;
    world.archive(page).await;

    for response in [
        world.diagrams(&viewer, page).await,
        world.export(&viewer, page, "docx", &[]).await,
    ] {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_code(response).await, "doc.not_found");
    }
    for response in [
        world.diagrams(&editor, page).await,
        world.export(&editor, page, "docx", &[]).await,
    ] {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(error_code(response).await, "doc.archived");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn request_errors() {
    let world = world().await;
    let page = world.create_doc("Page").await;
    let uri = format!("/api/docs/{page}/export/docx");

    // The session cookie alone, without the CSRF header.
    let response = world
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("cookie", &world.owner.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "auth.csrf");

    let response = world.export(&world.owner, page, "pdf", &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(response).await, "export.bad_format");

    let response = send_raw(&world.app, None, "POST", &uri, None, Vec::new()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(response).await, "auth.session_required");

    let (_, foreign) = world.foreign_doc().await;
    for response in [
        world.diagrams(&world.owner, foreign).await,
        world.export(&world.owner, foreign, "docx", &[]).await,
    ] {
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(error_code(response).await, "acl.no_grant");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn body_over_limit_is_413_but_big_part_falls_back() {
    let world = world().await;
    let page = world.create_doc("Diagrams").await;
    world
        .set_markdown(
            page,
            "```mermaid\ngraph TD; A-->B\n```\n\n```mermaid\ngraph TD; B-->C\n```\n",
        )
        .await;
    let keys = world.diagram_keys(page).await;
    assert_eq!(keys.len(), 2, "{keys:?}");

    let huge = padded_png(11 * 1024 * 1024);
    let response = world
        .export(&world.owner, page, "docx", &[(&keys[0], &huge)])
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(response).await, "export.too_large");

    // A readable picture past the 2 MiB part cap is dropped; the part after
    // it is still read.
    let big = padded_png(3 * 1024 * 1024);
    let response = world
        .export(
            &world.owner,
            page,
            "docx",
            &[(&keys[0], &big), (&keys[1], TINY_PNG)],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fallbacks_header(&response),
        "diagrams=1, images=0, unknown=0"
    );

    // A part of exactly 2 MiB is kept.
    let at_cap = padded_png(2 * 1024 * 1024);
    let response = world
        .export(
            &world.owner,
            page,
            "docx",
            &[(&keys[0], &at_cap), (&keys[1], TINY_PNG)],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fallbacks_header(&response), NO_FALLBACKS);
}

#[tokio::test(flavor = "multi_thread")]
async fn diagrams_listing_matches_writer_keys() {
    let world = world().await;
    let page = world.create_doc("Flow").await;
    world
        .set_markdown(page, "```mermaid\ngraph TD; A-->B\n```\n")
        .await;
    let key = format!("mermaid-{:x}", Sha256::digest(b"graph TD; A-->B"));

    let response = world.diagrams(&world.owner, page).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({
            "limits": {
                "max_body_bytes": 10485760,
                "max_part_bytes": 2097152,
                "max_parts": 100,
                "scale": 2,
            },
            "diagrams": [{"key": key, "kind": "mermaid", "source": "graph TD; A-->B"}],
        })
    );

    // A part named after nothing on the page is ignored.
    let response = world
        .export(
            &world.owner,
            page,
            "docx",
            &[("mermaid-not-on-page", TINY_PNG), (&key, TINY_PNG)],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fallbacks_header(&response), NO_FALLBACKS);
    let docx = body_bytes(response).await;
    assert_eq!(zip_names(&docx, "word/media/").len(), 1);

    // Boards are listed with their id and label.
    let board_page = world.create_doc("Board").await;
    let board_id = Uuid::new_v4().to_string();
    let content = yrs::Doc::new();
    let fragment = content.get_or_insert_xml_fragment("default");
    {
        let mut txn = content.transact_mut();
        let board = fragment.push_back(&mut txn, XmlElementPrelim::empty("excalidraw_board"));
        board.insert_attribute(&mut txn, "board_id", Any::from(board_id.clone()));
        board.insert_attribute(&mut txn, "label", Any::from("Roadmap"));
    }
    world.set_content(board_page, &content).await;
    let response = world.diagrams(&world.owner, board_page).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["diagrams"],
        json!([{
            "key": format!("board-{board_id}"),
            "kind": "board",
            "board_id": board_id,
            "label": "Roadmap",
        }])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn third_concurrent_export_gets_503() {
    let world = world_with(|s| {
        s.export_permits = Arc::new(Semaphore::new(0));
        s.export_busy_wait = Duration::from_millis(50);
    })
    .await;
    let page = world.create_doc("Busy").await;

    let response = world.export(&world.owner, page, "docx", &[]).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, "retry-after"), "5");
    assert_eq!(error_code(response).await, "export.busy");

    // One free slot serves exports one after the other: each gives its
    // permit back when it is done.
    world.state.export_permits.add_permits(1);
    for _ in 0..2 {
        let response = world.export(&world.owner, page, "docx", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}

/// A request dropped mid-build (the 30 s request timeout, or the client
/// going away) must not hand its permit back while the build still runs and
/// holds the page's images, pictures and output: the permit goes back when
/// the build ends, so no more than the permitted builds are ever in memory.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_request_keeps_its_permit_until_the_build_ends() {
    let entered = Arc::new(Notify::new());
    let open = Arc::new(Semaphore::new(0));
    let world = world_with(|s| {
        s.blob_store = Some(Arc::new(GatedBlobs {
            inner: s.blob_store.take().unwrap(),
            entered: entered.clone(),
            open: open.clone(),
        }));
        s.export_permits = Arc::new(Semaphore::new(1));
        s.export_busy_wait = Duration::from_millis(50);
    })
    .await;
    let page = world.create_doc("Dropped").await;
    let url = world.upload(page, "image/png", PNG_3X2).await;
    world.set_markdown(page, &format!("![p]({url})\n")).await;

    // Start the export and drop the request once its build is fetching the
    // image.
    {
        let export = world.export(&world.owner, page, "docx", &[]);
        tokio::pin!(export);
        tokio::select! {
            _ = &mut export => panic!("the export finished while its image read was held"),
            () = entered.notified() => {}
        }
    }

    assert_eq!(
        world.state.export_permits.available_permits(),
        0,
        "the dropped request's build still holds the permit"
    );
    let response = world.export(&world.owner, page, "docx", &[]).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(response).await, "export.busy");

    // Let the build finish: the permit comes back, and the next export runs.
    open.add_permits(1);
    let permit = tokio::time::timeout(
        Duration::from_secs(30),
        world.state.export_permits.acquire(),
    )
    .await
    .expect("the build gives its permit back when it ends")
    .unwrap();
    drop(permit);
    let response = world.export(&world.owner, page, "docx", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fallbacks_header(&response), NO_FALLBACKS);
}

#[tokio::test(flavor = "multi_thread")]
async fn parts_beyond_one_hundred_fall_back() {
    let world = world().await;
    let page = world.create_doc("Many diagrams").await;
    let markdown: String = (0..101)
        .map(|i| format!("```mermaid\ngraph TD; A-->N{i}\n```\n\n"))
        .collect();
    world.set_markdown(page, &markdown).await;
    let keys = world.diagram_keys(page).await;
    assert_eq!(keys.len(), 101);

    let parts: Vec<(&str, &[u8])> = keys.iter().map(|key| (key.as_str(), TINY_PNG)).collect();
    let response = world.export(&world.owner, page, "docx", &parts).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fallbacks_header(&response),
        "diagrams=1, images=0, unknown=0"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn filename_is_sanitized_on_both_sides() {
    let world = world().await;
    let page = world.create_doc("Q4 / Plan").await;

    let response = world.export(&world.owner, page, "docx", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let disposition = header(&response, "content-disposition");
    assert!(
        disposition.contains("filename*=UTF-8''Q4%20-%20Plan.docx"),
        "{disposition}"
    );
    assert!(
        disposition.contains(r#"filename="Q4 - Plan.docx""#),
        "{disposition}"
    );
}
