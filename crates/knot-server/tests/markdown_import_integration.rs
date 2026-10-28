//! Integration tests for `POST /api/docs/{id}/markdown` — the single-page
//! Markdown import behind the doc page's "Import Markdown…" control.
//!
//! The endpoint predates any caller in the web app. These tests pin the two
//! modes against each other: `append` (the wire default, which Yjs merges
//! into whatever is already there) and `replace` (clear + apply in one
//! transaction). The append-duplicates test is not aspirational — it locks
//! in the default so `from_template` keeps working.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{DocHandle, Engine, Rooms, SnapshotPolicy, YrsEngine};
use knot_markdown::legacy::compat;
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Fixture {
    app: axum::Router,
    pool: PgPool,
    doc_id: Uuid,
    owner_id: Uuid,
}

/// Workspace + owner (alice) + viewer (bob) + one doc, with a real Rooms
/// registry wired up so the import endpoint can acquire a room.
async fn fixture() -> Fixture {
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
        .create("default", "Workspace")
        .await
        .unwrap();
    let owner = state
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
        .add_member(ws.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let viewer = state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, viewer.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Import Doc", "m", owner.id)
        .await
        .unwrap();

    wire_rooms(&mut state, &db.url).await;
    let doc_id = doc.id;
    let owner_id = owner.id;
    Fixture {
        app: router_with_state(state),
        pool,
        doc_id,
        owner_id,
    }
}

/// Spin up a real Rooms registry wired to the test DB (copied from
/// `history_integration.rs` — the import endpoint needs `rooms_v2`).
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

async fn login(app: &axum::Router, email: &str) -> (String, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": "hunter22"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login failed");
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split('=')
        .nth(1)
        .unwrap()
        .to_string();
    (sid, csrf)
}

/// POST markdown, returning the raw response. `mode` is appended as a query
/// param when `Some`.
async fn post_import(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    mode: Option<&str>,
    body: impl Into<Body>,
) -> axum::response::Response {
    let uri = match mode {
        Some(m) => format!("/api/docs/{doc_id}/markdown?mode={m}"),
        None => format!("/api/docs/{doc_id}/markdown"),
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "text/markdown")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn import(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    mode: Option<&str>,
    body: impl Into<Body>,
) -> StatusCode {
    post_import(app, sid, csrf, doc_id, mode, body)
        .await
        .status()
}

/// Same as `import` but also returns the parsed error code, for negative cases.
async fn import_err(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    mode: Option<&str>,
    body: impl Into<Body>,
) -> (StatusCode, String) {
    let r = post_import(app, sid, csrf, doc_id, mode, body).await;
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
        .unwrap_or_default();
    (status, code)
}

async fn export(app: &axum::Router, sid: &str, csrf: &str, doc_id: Uuid) -> String {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{doc_id}/markdown"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "markdown export failed");
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The wire default merges. `from_template` relies on this (it always
/// targets a brand-new empty doc), so it must not change — but it is also
/// exactly why the UI cannot use the default.
#[tokio::test(flavor = "multi_thread")]
async fn append_into_non_empty_doc_keeps_both() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, None, "# First\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("append"), "# Second\n").await,
        StatusCode::NO_CONTENT
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let md = export(&f.app, &sid, &csrf, f.doc_id).await;
    assert!(md.contains("First"), "append lost the original: {md:?}");
    assert!(md.contains("Second"), "append lost the import: {md:?}");
}

/// What the UI uses: the imported file becomes the page, not an addition
/// to it.
#[tokio::test(flavor = "multi_thread")]
async fn replace_into_non_empty_doc_swaps_content() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, None, "# Original\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            "# Imported\n"
        )
        .await,
        StatusCode::NO_CONTENT
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let md = export(&f.app, &sid, &csrf, f.doc_id).await;
    assert!(md.contains("Imported"), "replace lost the import: {md:?}");
    assert!(
        !md.contains("Original"),
        "replace kept the old body: {md:?}"
    );
}

/// The `len > 0` guard in the replace arm must not trip on a fresh doc —
/// this is the common case for the UI (create a page, import into it).
#[tokio::test(flavor = "multi_thread")]
async fn replace_into_empty_doc_works() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Fresh\n").await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        export(&f.app, &sid, &csrf, f.doc_id)
            .await
            .contains("Fresh")
    );
}

/// The importing user is recorded against the update, so a full-page
/// rewrite is attributable in `doc_updates`.
#[tokio::test(flavor = "multi_thread")]
async fn replace_records_the_importing_user() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            "# Attributed\n"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let authors: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT by_user_id FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(f.doc_id)
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert!(
        authors.contains(&Some(f.owner_id)),
        "expected an update attributed to the importer, got {authors:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_mode_is_rejected() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let (status, code) = import_err(&f.app, &sid, &csrf, f.doc_id, Some("merge"), "# Nope\n").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "markdown.bad_mode");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_import() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "bob@example.com").await;

    let (status, code) =
        import_err(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Nope\n").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code, "acl.editor_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_import_is_rejected() {
    let f = fixture().await;

    // No session cookie at all. CSRF middleware sees matching header+cookie
    // values, so the request reaches the handler's auth check.
    let (status, code) = import_err(&f.app, "sid=nope", "nope", f.doc_id, None, "# Nope\n").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "auth.session_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_utf8_body_is_rejected() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let (status, code) = import_err(
        &f.app,
        &sid,
        &csrf,
        f.doc_id,
        Some("replace"),
        vec![0xffu8, 0xfe, 0xfd],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code, "markdown.not_utf8");
}

// ---------------------------------------------------------------------------
// Nested checklists and list line breaks survive every route that rebuilds a
// page from its own Markdown (spec A4, A10): GET markdown, history restore,
// from-template and workspace import.
// ---------------------------------------------------------------------------

/// A checklist nested three deep, indented by the list marker's width.
const NESTED_CHECKLIST: &str = "- [ ] a\n  - [x] b\n    - [ ] c\n";

/// POST `body` to `uri`, returning the status and the response body.
async fn post_raw(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    uri: &str,
    content_type: &str,
    body: impl Into<Body>,
) -> (StatusCode, Vec<u8>) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", content_type)
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// The page as stored: every update of `doc_id`, applied in order.
async fn stored_doc(pool: &PgPool, doc_id: Uuid) -> DocHandle {
    let updates: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT update_bytes FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(doc_id)
            .fetch_all(pool)
            .await
            .unwrap();
    let doc = YrsEngine.new_doc();
    for update in updates {
        YrsEngine.apply_update(&doc, &update).unwrap();
    }
    doc
}

fn assert_schema_valid(doc: &DocHandle) {
    assert_eq!(
        compat::schema_violations(doc.inner()),
        Vec::<String>::new(),
        "the stored page breaks the schema:\n{}",
        compat::normalised(doc.inner())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_of_nested_checklist_uses_marker_indent() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            NESTED_CHECKLIST
        )
        .await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        export(&f.app, &sid, &csrf, f.doc_id).await,
        NESTED_CHECKLIST
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_restore_keeps_nested_checklists() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    // Different current content, then a history snapshot holding the nested
    // checklist. The snapshot goes in once the room runs, so the room never
    // loads it as its base state.
    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            "# Current\n"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (snapshot, _) = knot_markdown::from_markdown::parse(NESTED_CHECKLIST).unwrap();
    let state_bytes = YrsEngine.encode_state_as_update(&snapshot, None).unwrap();
    let state_vector = YrsEngine.encode_state_vector(&snapshot).unwrap();
    PgSnapshotStore::new(f.pool.clone())
        .insert(f.doc_id, 1, &state_bytes, &state_vector)
        .await
        .unwrap();

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/{}/history/1/restore", f.doc_id),
        "application/json",
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "restore: {}",
        String::from_utf8_lossy(&body)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        export(&f.app, &sid, &csrf, f.doc_id).await,
        NESTED_CHECKLIST
    );
    let stored = stored_doc(&f.pool, f.doc_id).await;
    assert_schema_valid(&stored);
    assert_eq!(
        compat::normalised(stored.inner()),
        compat::normalised(snapshot.inner()),
        "the restore changed the checklist's structure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn from_template_keeps_nested_checklists() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            NESTED_CHECKLIST
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/{}/template", f.doc_id),
        "application/json",
        r#"{"is_template":true}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "set template: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/from-template/{}", f.doc_id),
        "application/json",
        "{}",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "from-template: {}",
        String::from_utf8_lossy(&body)
    );
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(export(&f.app, &sid, &csrf, new_id).await, NESTED_CHECKLIST);
    assert_schema_valid(&stored_doc(&f.pool, new_id).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_import_keeps_list_line_breaks_and_nested_checklists() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let md = concat!(
        "# Notes\n\n",
        "- a  \n  b\n\n",
        "## Tasks\n\n",
        "- [ ] parent\n  - [x] child\n    - [ ] grandchild\n",
    );
    let manifest = serde_json::json!({
        "knot_export_version": "2",
        "docs": [{
            "id": Uuid::new_v4().to_string(),
            "parent_id": null,
            "title": "Imported lists",
            "sort_key": "n",
            "path": "docs/Imported-lists.md",
        }],
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("docs/Imported-lists.md", opts).unwrap();
    zip.write_all(md.as_bytes()).unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        "/api/workspace/import",
        "application/zip",
        bytes,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "workspace import: {}",
        String::from_utf8_lossy(&body)
    );
    let imported: Uuid = sqlx::query_scalar("SELECT id FROM documents WHERE title = $1")
        .bind("Imported lists")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(export(&f.app, &sid, &csrf, imported).await, md);
    assert_schema_valid(&stored_doc(&f.pool, imported).await);
}

// ---------------------------------------------------------------------------
// Pages the old importer damaged export healed, and the routes that rebuild a
// page from its own Markdown heal it (spec A10, "Tolerant export").
// ---------------------------------------------------------------------------

/// The Markdown the old importer split: a list item with a line break.
const LINE_BREAK_ITEM: &str = "- first  \n  second\n";

/// The page the old importer stored for [`LINE_BREAK_ITEM`]: its list item
/// holds `paragraph, hard_break, paragraph`, which the schema forbids and
/// which failed every export.
fn damaged_page() -> DocHandle {
    let (doc, _) = knot_markdown::legacy::parse(LINE_BREAK_ITEM).unwrap();
    assert!(
        !compat::schema_violations(doc.inner()).is_empty(),
        "the old importer's page is not damaged"
    );
    doc
}

/// Stores `doc` as the page's content before any room has loaded it.
async fn seed(f: &Fixture, doc: &DocHandle) {
    let bytes = YrsEngine.encode_state_as_update(doc, None).unwrap();
    PgUpdatesStore::new(f.pool.clone())
        .insert_batch(f.doc_id, &[(Some(f.owner_id), bytes)])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_of_damaged_page_is_healed() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    // The damaged list item, and a node no version of the schema has.
    let doc = damaged_page();
    {
        use yrs::{Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let callout = frag.push_back(&mut txn, XmlElementPrelim::empty("callout"));
        let para = callout.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        para.push_back(&mut txn, XmlTextPrelim::new("note"));
    }
    seed(&f, &doc).await;

    assert_eq!(
        export(&f.app, &sid, &csrf, f.doc_id).await,
        format!("{LINE_BREAK_ITEM}\nnote\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_restore_heals_a_damaged_page() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    // As in `history_restore_keeps_nested_checklists`: other current content,
    // then the damaged page as a history snapshot once the room runs.
    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            "# Current\n"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let snapshot = damaged_page();
    let state_bytes = YrsEngine.encode_state_as_update(&snapshot, None).unwrap();
    let state_vector = YrsEngine.encode_state_vector(&snapshot).unwrap();
    PgSnapshotStore::new(f.pool.clone())
        .insert(f.doc_id, 1, &state_bytes, &state_vector)
        .await
        .unwrap();

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/{}/history/1/restore", f.doc_id),
        "application/json",
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "restore: {}",
        String::from_utf8_lossy(&body)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, LINE_BREAK_ITEM);
    let stored = stored_doc(&f.pool, f.doc_id).await;
    assert_schema_valid(&stored);
    let (healed, _) = knot_markdown::from_markdown::parse(LINE_BREAK_ITEM).unwrap();
    assert_eq!(
        compat::normalised(stored.inner()),
        compat::normalised(healed.inner()),
        "the restore didn't store the healed line break"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn from_template_heals_a_damaged_template() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    seed(&f, &damaged_page()).await;
    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/{}/template", f.doc_id),
        "application/json",
        r#"{"is_template":true}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "set template: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/from-template/{}", f.doc_id),
        "application/json",
        "{}",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "from-template: {}",
        String::from_utf8_lossy(&body)
    );
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(export(&f.app, &sid, &csrf, new_id).await, LINE_BREAK_ITEM);
    assert_schema_valid(&stored_doc(&f.pool, new_id).await);
}

/// `>` repeated 100,000 times: a page nested far deeper than the editor
/// makes one, which any editor can import.
fn deep_markdown() -> String {
    format!("{} x\n", ">".repeat(100_000))
}

/// What the export of [`deep_markdown`]'s import is: the quotes the importer
/// and the writer follow (`to_markdown::MAX_DEPTH`), the innermost holding
/// the text.
fn degraded() -> String {
    format!("{}x\n", "> ".repeat(knot_markdown::to_markdown::MAX_DEPTH))
}

/// A page nested far deeper than the editor makes one: `>` repeated 100,000
/// times. The refresh after the import renders it, and a writer that
/// recursed once per quote overflowed the stack, which aborts the whole
/// server (final review md-1). Replacing the page, as the doc page's import
/// does, deleted the 100,000 nested quotes, and yrs recursed once per quote
/// there too (F1 review). The importer stores only the quotes the writer
/// follows, so the import succeeds, and the server goes on serving the page.
#[tokio::test(flavor = "multi_thread")]
async fn deeply_nested_import_keeps_the_server_serving() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let status = import(&f.app, &sid, &csrf, f.doc_id, None, deep_markdown()).await;
    assert!(status.is_success(), "import answered {status}");
    assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, degraded());

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# After\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, "# After\n");
}

/// The doc page's import of a deeply nested page, twice: the second replaces
/// what the first stored.
#[tokio::test(flavor = "multi_thread")]
async fn deeply_nested_import_replaced_twice_keeps_the_server_serving() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    for _ in 0..2 {
        assert_eq!(
            import(
                &f.app,
                &sid,
                &csrf,
                f.doc_id,
                Some("replace"),
                deep_markdown()
            )
            .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, degraded());
    }
    let stored = stored_doc(&f.pool, f.doc_id).await;
    assert!(!knot_crdt::nesting::nests_deeper_than(
        &stored,
        knot_crdt::nesting::MAX_NESTING
    ));
}

/// A history version of a deeply nested import, restored over the page
/// while it holds such an import: the restore replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn history_restore_of_a_deeply_nested_version_keeps_the_server_serving() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(
            &f.app,
            &sid,
            &csrf,
            f.doc_id,
            Some("replace"),
            deep_markdown()
        )
        .await,
        StatusCode::NO_CONTENT
    );
    // The version, as the room snapshots the page after the import. It goes
    // in once the room runs, so the room never loads it as its base state.
    let (version, _) = knot_markdown::from_markdown::parse(&deep_markdown()).unwrap();
    let state_bytes = YrsEngine.encode_state_as_update(&version, None).unwrap();
    let state_vector = YrsEngine.encode_state_vector(&version).unwrap();
    PgSnapshotStore::new(f.pool.clone())
        .insert(f.doc_id, 1, &state_bytes, &state_vector)
        .await
        .unwrap();

    let (status, body) = post_raw(
        &f.app,
        &sid,
        &csrf,
        &format!("/api/docs/{}/history/1/restore", f.doc_id),
        "application/json",
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "restore: {}",
        String::from_utf8_lossy(&body)
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, degraded());
    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# After\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(export(&f.app, &sid, &csrf, f.doc_id).await, "# After\n");
}
