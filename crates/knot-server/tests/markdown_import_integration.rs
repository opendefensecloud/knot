//! Integration tests for `POST /api/docs/{id}/markdown` — the single-page
//! Markdown import behind the doc page's "Import Markdown…" control.
//!
//! The endpoint predates any caller in the web app. These tests pin the two
//! modes against each other: `append` (the wire default, which Yjs merges
//! into whatever is already there) and `replace` (clear + apply in one
//! transaction). The append-duplicates test is not aspirational — it locks
//! in the default so `from_template` keeps working.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
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
