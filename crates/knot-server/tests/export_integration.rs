//! Page export to DOCX/ODT: the export service (snapshot, asset resolution,
//! build) against a fresh database per test, with a real rooms registry.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_export::{
    Block, ExportDoc, Fallbacks, ImageAsset, ImageSource, Inline, LinkTarget, Marks, Run,
};
use knot_server::export::{self, Format};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
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
) -> axum::response::Response {
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

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn body_json(response: axum::response::Response) -> Value {
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
        const BOUNDARY: &str = "----ExportTestBoundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"picture\"\r\n",
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
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
}

/// One zip entry's bytes, or `None` when absent.
fn zip_entry(zip: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut file = archive.by_name(name).ok()?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    Some(out)
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
