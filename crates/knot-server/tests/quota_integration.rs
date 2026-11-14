//! Optional page and file-byte quotas (issue #88, spec test 6): each write
//! that adds pages or files refuses with 403 `quota.exceeded` at the
//! boundary and writes nothing; under the quota it works; a refusal is
//! audited at most once per user, quota and scope per window; with the
//! quotas off nothing is refused. Against a fresh database per test, with a
//! real rooms registry.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_config::Config;
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::{Any, ReadTxn, Transact, Xml, XmlElementPrelim, XmlFragment};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const PASSWORD: &str = "hunter22hunter22";
const OWNER_EMAIL: &str = "owner@quota.test";

/// A 1×1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x01, 0x5C, 0x5B, 0x66, 0xE3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

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

/// The quotas `configure` sets, rate limits off (as the constructors leave
/// them), everything else the default.
fn quota_config(configure: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config {
        rate_limit_enabled: false,
        ..Default::default()
    };
    configure(&mut config);
    config
}

/// A workspace with one owner, a real rooms registry and `config`.
async fn world(config: Config) -> World {
    let db = knot_test_support::fresh_db().await;
    let mut state = AppState::with_pool(db.pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    state.config = Arc::new(config);
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
        .create_local(OWNER_EMAIL, "Olive Owner", &hash)
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
    let owner = login(&app, OWNER_EMAIL, owner.id).await;
    World {
        state,
        app,
        owner,
        workspace_id: workspace.id,
    }
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

async fn login(app: &Router, email: &str, user_id: Uuid) -> Session {
    let response = send(
        app,
        None,
        "POST",
        "/auth/login",
        "application/json",
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

async fn send(
    app: &Router,
    who: Option<&Session>,
    method: &str,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", content_type);
    if let Some(who) = who {
        request = request
            .header("cookie", &who.cookie)
            .header("x-csrf-token", &who.csrf);
    }
    app.clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(json!({}))
}

/// Asserts `response` is the quota refusal for `quota`, `scope` and `limit`.
async fn assert_refused(response: Response, quota: &str, scope: &str, limit: u64) {
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "quota.exceeded", "{body}");
    assert_eq!(
        body["error"]["details"],
        json!({"quota": quota, "scope": scope, "limit": limit}),
        "{body}"
    );
}

fn id_of(doc: &Value) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

impl World {
    fn pool(&self) -> &sqlx::PgPool {
        self.state.pool.as_ref().unwrap()
    }

    async fn send_json(&self, who: &Session, method: &str, uri: &str, body: Value) -> Response {
        send(
            &self.app,
            Some(who),
            method,
            uri,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await
    }

    async fn create_response(&self, who: &Session, title: &str, parent: Option<Uuid>) -> Response {
        self.send_json(
            who,
            "POST",
            "/api/docs",
            json!({"title": title, "parent_id": parent}),
        )
        .await
    }

    async fn create(&self, who: &Session, title: &str, parent: Option<Uuid>) -> Uuid {
        let response = self.create_response(who, title, parent).await;
        assert_eq!(response.status(), StatusCode::CREATED, "create {title}");
        id_of(&body_json(response).await)
    }

    async fn upload_response(&self, who: &Session, doc_id: Uuid, bytes: &[u8]) -> Response {
        const BOUNDARY: &str = "----QuotaTestBoundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"dot.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        send(
            &self.app,
            Some(who),
            "POST",
            &format!("/api/docs/{doc_id}/blobs"),
            &format!("multipart/form-data; boundary={BOUNDARY}"),
            body,
        )
        .await
    }

    async fn upload(&self, who: &Session, doc_id: Uuid) -> Uuid {
        let response = self.upload_response(who, doc_id, TINY_PNG).await;
        assert_eq!(response.status(), StatusCode::CREATED, "upload");
        let url = body_json(response).await["url"]
            .as_str()
            .unwrap()
            .to_string();
        url.trim_start_matches("/api/blobs/").parse().unwrap()
    }

    async fn duplicate(&self, doc_id: Uuid, include_subpages: bool) -> Response {
        self.send_json(
            &self.owner,
            "POST",
            &format!("/api/docs/{doc_id}/duplicate"),
            json!({"include_subpages": include_subpages}),
        )
        .await
    }

    async fn import(&self, zip: Vec<u8>) -> Response {
        send(
            &self.app,
            Some(&self.owner),
            "POST",
            "/api/workspace/import",
            "application/zip",
            zip,
        )
        .await
    }

    /// Invite `email` as an editor and log them in.
    async fn editor(&self, email: &str) -> Session {
        let response = self
            .send_json(
                &self.owner,
                "POST",
                "/api/workspace/members",
                json!({"email": email, "role": "editor", "password": PASSWORD, "display_name": "Ed"}),
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

    /// Pages, files and pending file copies stored, archived or not.
    async fn stored_counts(&self) -> (i64, i64, i64) {
        sqlx::query_as(
            "SELECT (SELECT count(*) FROM documents),
                    (SELECT count(*) FROM blobs),
                    (SELECT count(*) FROM blob_copy_pending)",
        )
        .fetch_one(self.pool())
        .await
        .unwrap()
    }

    /// The `quota.exceeded` audit rows, oldest first: actor, target kind,
    /// target id and data.
    async fn quota_audits(&self) -> Vec<(Option<Uuid>, String, Uuid, Value)> {
        sqlx::query_as(
            "SELECT actor_id, target_kind, target_id, data FROM audit_events
              WHERE action = 'quota.exceeded' AND workspace_id = $1 ORDER BY id",
        )
        .bind(self.workspace_id)
        .fetch_all(self.pool())
        .await
        .unwrap()
    }

    /// Replace `doc_id`'s content through its room with one image per file
    /// of `blob_ids`, as the editor shows them.
    async fn show_images(&self, doc_id: Uuid, blob_ids: &[Uuid]) {
        let content = yrs::Doc::new();
        let fragment = content.get_or_insert_xml_fragment("default");
        {
            let mut txn = content.transact_mut();
            for blob_id in blob_ids {
                let image = fragment.push_back(&mut txn, XmlElementPrelim::empty("image"));
                image.insert_attribute(&mut txn, "src", Any::from(format!("/api/blobs/{blob_id}")));
            }
        }
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
                source: knot_storage::UpdateSource::Import,
                meta: None,
                reply,
            })
            .await
            .unwrap();
        done.await.unwrap().expect("replace page content");
    }
}

/// A zip of `manifest` as `index.json` plus `entries`.
fn zip_of(manifest: &Value, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("index.json", options).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// An export of `page_count` top-level pages, the first with one attachment
/// of `attachment_bytes` bytes.
fn export_zip(page_count: usize, attachment_bytes: usize) -> Vec<u8> {
    let page_ids: Vec<Uuid> = (0..page_count).map(|_| Uuid::new_v4()).collect();
    let docs: Vec<Value> = page_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            json!({
                "id": id.to_string(),
                "parent_id": null,
                "title": format!("Imported {index}"),
                "sort_key": "m",
                "path": format!("docs/imported-{index}.md"),
            })
        })
        .collect();
    let attachment = vec![7u8; attachment_bytes];
    let attachments = if attachment_bytes > 0 {
        vec![json!({
            "id": Uuid::new_v4().to_string(),
            "doc_id": page_ids[0].to_string(),
            "content_type": "application/octet-stream",
            "original_name": null,
            "byte_size": 0,
            "path": "attachments/file.bin",
        })]
    } else {
        vec![]
    };
    let manifest = json!({
        "knot_export_version": "2",
        "docs": docs,
        "attachments": attachments,
        "boards": [],
    });
    let entries: Vec<(&str, &[u8])> = if attachment_bytes > 0 {
        vec![("attachments/file.bin", &attachment)]
    } else {
        vec![]
    };
    zip_of(&manifest, &entries)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// The per-user page quota refuses the page past it, counts archived pages,
/// and leaves other users alone.
#[tokio::test(flavor = "multi_thread")]
async fn page_quota_per_user_refuses_create_past_it() {
    let w = world(quota_config(|config| config.quota_pages_per_user = 2)).await;
    w.create(&w.owner, "One", None).await;
    let second = w.create(&w.owner, "Two", None).await;

    let before = w.stored_counts().await;
    assert_refused(
        w.create_response(&w.owner, "Three", None).await,
        "pages",
        "user",
        2,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");

    // An archived page still counts until it is purged.
    let response = w
        .send_json(
            &w.owner,
            "DELETE",
            &format!("/api/docs/{second}"),
            json!({}),
        )
        .await;
    assert!(response.status().is_success(), "archive");
    assert_refused(
        w.create_response(&w.owner, "Three", None).await,
        "pages",
        "user",
        2,
    )
    .await;

    // Another user has a quota of their own.
    let editor = w.editor("editor@quota.test").await;
    w.create(&editor, "Editor's", None).await;
}

/// The workspace page quota counts every user's pages.
#[tokio::test(flavor = "multi_thread")]
async fn page_quota_per_workspace_counts_every_user() {
    let w = world(quota_config(|config| config.quota_pages_per_workspace = 2)).await;
    let editor = w.editor("editor@quota.test").await;
    w.create(&w.owner, "Owner's", None).await;
    w.create(&editor, "Editor's", None).await;
    let before = w.stored_counts().await;
    assert_refused(
        w.create_response(&editor, "One more", None).await,
        "pages",
        "workspace",
        2,
    )
    .await;
    assert_refused(
        w.create_response(&w.owner, "One more", None).await,
        "pages",
        "workspace",
        2,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");
}

/// A page from a template counts one page; the template counts too.
#[tokio::test(flavor = "multi_thread")]
async fn from_template_respects_the_page_quota() {
    let w = world(quota_config(|config| config.quota_pages_per_user = 2)).await;
    let template = w.create(&w.owner, "Template", None).await;
    let response = w
        .send_json(
            &w.owner,
            "POST",
            &format!("/api/docs/{template}/template"),
            json!({"is_template": true}),
        )
        .await;
    assert!(response.status().is_success(), "make template");
    let uri = format!("/api/docs/from-template/{template}");

    let response = w.send_json(&w.owner, "POST", &uri, json!({})).await;
    assert_eq!(response.status(), StatusCode::CREATED, "under the quota");

    let before = w.stored_counts().await;
    assert_refused(
        w.send_json(&w.owner, "POST", &uri, json!({})).await,
        "pages",
        "user",
        2,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");
}

/// Duplicate is refused before the permit wait when one more page is over
/// the quota, and once planned when the copy's pages are.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_counts_every_page_of_the_copy() {
    let w = world(quota_config(|config| config.quota_pages_per_user = 3)).await;
    let source = w.create(&w.owner, "Source", None).await;
    w.create(&w.owner, "Child", Some(source)).await;

    // Source and child: two pages; the copy with its subpage needs 4 > 3.
    let before = w.stored_counts().await;
    assert_refused(w.duplicate(source, true).await, "pages", "user", 3).await;
    assert_eq!(w.stored_counts().await, before, "nothing written");

    // The page alone fits: 3 pages.
    assert_eq!(
        w.duplicate(source, false).await.status(),
        StatusCode::CREATED
    );

    // At the quota, even one page is refused (the pre-check).
    let before = w.stored_counts().await;
    assert_refused(w.duplicate(source, false).await, "pages", "user", 3).await;
    assert_eq!(w.stored_counts().await, before, "nothing written");
}

/// The duplicate pre-check runs before the permit wait: with every permit
/// held by running copies, a user at the quota still gets 403 at once, not
/// a retryable 503 `doc.duplicate_busy` after the busy wait for a request
/// that can never succeed. (The planned check in `duplicate::run` gives the
/// same 403 once a permit is free, so only a busy server tells them apart.)
#[tokio::test(flavor = "multi_thread")]
async fn an_at_quota_duplicate_is_refused_before_the_permit_wait() {
    let w = world(quota_config(|config| config.quota_pages_per_user = 1)).await;
    let source = w.create(&w.owner, "Source", None).await;
    let running_copies = w
        .state
        .duplicate_permits
        .clone()
        .acquire_many_owned(w.state.duplicate_permits.available_permits() as u32)
        .await
        .unwrap();
    assert_eq!(w.state.duplicate_permits.available_permits(), 0);

    let started = std::time::Instant::now();
    let response = w.duplicate(source, false).await;
    let answered_after = started.elapsed();

    assert_refused(response, "pages", "user", 1).await;
    assert!(
        answered_after < w.state.duplicate_busy_wait,
        "answered after {answered_after:?}: the request waited for a permit"
    );
    drop(running_copies);
}

/// Import counts every page of the manifest, once, before the first is
/// created; an import over the quota is refused whole.
#[tokio::test(flavor = "multi_thread")]
async fn import_counts_every_page_up_front() {
    let w = world(quota_config(|config| config.quota_pages_per_user = 3)).await;
    w.create(&w.owner, "Home", None).await;

    let before = w.stored_counts().await;
    assert_refused(w.import(export_zip(3, 0)).await, "pages", "user", 3).await;
    assert_eq!(w.stored_counts().await, before, "nothing written");

    let response = w.import(export_zip(2, 0)).await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported_docs"], json!(2), "{body}");
}

// ---------------------------------------------------------------------------
// File bytes
// ---------------------------------------------------------------------------

/// An upload past the per-user byte quota is refused and stores nothing;
/// another user has their own quota.
#[tokio::test(flavor = "multi_thread")]
async fn upload_respects_the_per_user_byte_quota() {
    let quota = 2 * TINY_PNG.len() as u64;
    let w = world(quota_config(|config| {
        config.quota_blob_bytes_per_user = quota
    }))
    .await;
    let page = w.create(&w.owner, "Pictures", None).await;
    w.upload(&w.owner, page).await;
    w.upload(&w.owner, page).await;

    let before = w.stored_counts().await;
    assert_refused(
        w.upload_response(&w.owner, page, TINY_PNG).await,
        "blob_bytes",
        "user",
        quota,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");

    let editor = w.editor("editor@quota.test").await;
    w.upload(&editor, page).await;
}

/// The workspace byte quota counts every user's files: a file that would
/// take it one byte past is refused, one that fills it exactly is stored.
#[tokio::test(flavor = "multi_thread")]
async fn upload_respects_the_workspace_byte_quota() {
    let quota = 2 * TINY_PNG.len() as u64 + 10;
    let w = world(quota_config(|config| {
        config.quota_blob_bytes_per_workspace = quota
    }))
    .await;
    let editor = w.editor("editor@quota.test").await;
    let page = w.create(&w.owner, "Pictures", None).await;
    w.upload(&w.owner, page).await;
    w.upload(&editor, page).await;

    let before = w.stored_counts().await;
    assert_refused(
        w.upload_response(&editor, page, &[1u8; 11]).await,
        "blob_bytes",
        "workspace",
        quota,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");
    let response = w.upload_response(&editor, page, &[1u8; 10]).await;
    assert_eq!(response.status(), StatusCode::CREATED, "fills it exactly");
}

/// Duplicate counts the bytes of the files it copies for the duplicating
/// user, before any is copied.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_counts_the_copied_file_bytes() {
    let file_bytes = TINY_PNG.len() as u64;
    // The original's two files, and room for one copy of one of them.
    let quota = 3 * file_bytes;
    let w = world(quota_config(|config| {
        config.quota_blob_bytes_per_user = quota
    }))
    .await;
    let source = w.create(&w.owner, "Photos", None).await;
    let first = w.upload(&w.owner, source).await;
    let second = w.upload(&w.owner, source).await;
    w.show_images(source, &[first, second]).await;

    let before = w.stored_counts().await;
    assert_refused(
        w.duplicate(source, false).await,
        "blob_bytes",
        "user",
        quota,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written or copied");

    let single = w.create(&w.owner, "One photo", None).await;
    w.show_images(single, &[first]).await;
    assert_eq!(
        w.duplicate(single, false).await.status(),
        StatusCode::CREATED
    );
}

/// Import counts the declared bytes of its attachments before the first
/// page is created.
#[tokio::test(flavor = "multi_thread")]
async fn import_counts_the_attachment_bytes_up_front() {
    let w = world(quota_config(|config| {
        config.quota_blob_bytes_per_user = 100
    }))
    .await;
    let before = w.stored_counts().await;
    assert_refused(
        w.import(export_zip(1, 101)).await,
        "blob_bytes",
        "user",
        100,
    )
    .await;
    assert_eq!(w.stored_counts().await, before, "nothing written");

    let response = w.import(export_zip(1, 100)).await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported_attachments"], json!(1), "{body}");
}

// ---------------------------------------------------------------------------
// Audit and off
// ---------------------------------------------------------------------------

/// Repeated refusals are audited once per user, quota and scope in the
/// window, with what was used and asked for.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_audited_once_per_window() {
    let w = world(quota_config(|config| {
        config.quota_pages_per_user = 1;
        config.quota_blob_bytes_per_user = 1;
    }))
    .await;
    let page = w.create(&w.owner, "Only", None).await;
    for _ in 0..3 {
        assert_refused(
            w.create_response(&w.owner, "More", None).await,
            "pages",
            "user",
            1,
        )
        .await;
    }
    for _ in 0..2 {
        assert_refused(
            w.upload_response(&w.owner, page, TINY_PNG).await,
            "blob_bytes",
            "user",
            1,
        )
        .await;
    }

    let owner_id = w.owner.user_id;
    assert_eq!(
        w.quota_audits().await,
        [
            (
                Some(owner_id),
                "user".to_string(),
                owner_id,
                json!({"quota": "pages", "scope": "user", "limit": 1, "used": 1, "requested": 1}),
            ),
            (
                Some(owner_id),
                "user".to_string(),
                owner_id,
                json!({
                    "quota": "blob_bytes",
                    "scope": "user",
                    "limit": 1,
                    "used": 0,
                    "requested": TINY_PNG.len(),
                }),
            ),
        ]
    );
}

/// With the quotas off (the default) nothing is refused and nothing is
/// audited.
#[tokio::test(flavor = "multi_thread")]
async fn quotas_off_refuse_nothing() {
    let w = world(quota_config(|_| {})).await;
    let page = w.create(&w.owner, "Home", None).await;
    for index in 0..5 {
        w.create(&w.owner, &format!("Page {index}"), Some(page))
            .await;
        w.upload(&w.owner, page).await;
    }
    assert_eq!(w.duplicate(page, true).await.status(), StatusCode::CREATED);
    assert_eq!(w.import(export_zip(3, 10)).await.status(), StatusCode::OK);
    assert!(w.quota_audits().await.is_empty());
}
