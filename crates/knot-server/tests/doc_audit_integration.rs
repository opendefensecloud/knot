//! The page audit log, `GET /api/docs/{id}/audit`: who may read it, which
//! exports it records (docx/odt, the Markdown zip of a page, of a subtree
//! and of the workspace) and which it doesn't, that an export it can't
//! record is refused (fail closed), what each event shows and never shows,
//! its stats, and its paging. Against a fresh database per test, with a
//! real rooms registry on a MemBus.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "hunter22hunter22";

/// Minimal valid 1×1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x01, 0x5C, 0x5B, 0x66, 0xE3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

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
    email: &'static str,
    session: Session,
}

struct Fixture {
    app: axum::Router,
    state: AppState,
    pool: knot_storage::Pool,
    workspace_id: Uuid,
    /// Workspace owner, "Olga".
    owner: Member,
    /// Workspace editor, "Eddie".
    editor: Member,
    /// Workspace viewer, "Vera".
    viewer: Member,
}

async fn fixture() -> Fixture {
    fixture_with(|_| {}).await
}

/// Postgres-backed state with doc rooms on a MemBus, `configure` applied,
/// and three logged-in members, one per workspace role.
async fn fixture_with(configure: impl FnOnce(&mut AppState)) -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    state.cookie_secure = false;

    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
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
    configure(&mut state);

    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let app = router_with_state(state.clone());
    let mut members = Vec::new();
    for (email, name, role) in [
        ("olga@audit.test", "Olga", WorkspaceRole::Owner),
        ("eddie@audit.test", "Eddie", WorkspaceRole::Editor),
        ("vera@audit.test", "Vera", WorkspaceRole::Viewer),
    ] {
        let hash = state.hasher.hash(PASSWORD).unwrap();
        let user = state
            .users
            .as_ref()
            .unwrap()
            .create_local(email, name, &hash)
            .await
            .unwrap();
        state
            .workspaces
            .as_ref()
            .unwrap()
            .add_member(workspace.id, user.id, role)
            .await
            .unwrap();
        members.push(Member {
            id: user.id,
            email,
            session: login(&app, email).await,
        });
    }
    let [owner, editor, viewer]: [Member; 3] = members.try_into().ok().unwrap();

    Fixture {
        app,
        state,
        pool,
        workspace_id: workspace.id,
        owner,
        editor,
        viewer,
    }
}

async fn login(app: &axum::Router, email: &str) -> Session {
    let response = app
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
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "login {email} failed"
    );
    let mut sid = None;
    let mut csrf = None;
    for value in response.headers().get_all("set-cookie") {
        let pair = value.to_str().unwrap().split(';').next().unwrap();
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

/// A response: status, `Cache-Control`, `Content-Disposition` and body text.
struct Answer {
    status: StatusCode,
    cache_control: Option<String>,
    content_disposition: Option<String>,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.body))
    }

    /// The body of a 200, as JSON.
    #[track_caller]
    fn ok(&self) -> Value {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        self.json()
    }

    #[track_caller]
    fn error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.json()["error"]["code"], code, "{}", self.body);
    }
}

/// `method uri` as `session` (anonymous when `None`), with `body` of
/// `content_type`.
async fn send(
    fx: &Fixture,
    session: Option<&Session>,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(session) = session {
        builder = builder
            .header("cookie", &session.cookie)
            .header("x-csrf-token", &session.csrf);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    answer_to(fx, builder.body(Body::from(body)).unwrap()).await
}

/// `method uri` with an empty body, authenticated by the API key `key`
/// (`Authorization: Bearer`, no cookie and no CSRF token).
async fn send_with_key(fx: &Fixture, key: &str, method: &str, uri: &str) -> Answer {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    answer_to(fx, request).await
}

/// What the app answers to `request`.
async fn answer_to(fx: &Fixture, request: Request<Body>) -> Answer {
    let response = fx.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cache_control = header_text(response.headers(), "cache-control");
    let content_disposition = header_text(response.headers(), "content-disposition");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        cache_control,
        content_disposition,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn header_text(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|value| value.to_str().unwrap().to_string())
}

async fn send_json(
    fx: &Fixture,
    session: &Session,
    method: &str,
    uri: &str,
    body: Value,
) -> Answer {
    send(
        fx,
        Some(session),
        method,
        uri,
        Some("application/json"),
        body.to_string().into_bytes(),
    )
    .await
}

/// `GET /api/docs/{doc}/audit?{query}` as `session`.
async fn audit_log(fx: &Fixture, session: Option<&Session>, doc: Uuid, query: &str) -> Answer {
    send(
        fx,
        session,
        "GET",
        &format!("/api/docs/{doc}/audit?{query}"),
        None,
        Vec::new(),
    )
    .await
}

/// A new page by the owner, through the API (so `doc.create` is audited).
async fn create_page(fx: &Fixture, title: &str, parent_id: Option<Uuid>) -> Uuid {
    let answer = send_json(
        fx,
        &fx.owner.session,
        "POST",
        "/api/docs",
        json!({"title": title, "parent_id": parent_id}),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::CREATED,
        "create {title}: {}",
        answer.body
    );
    answer.json()["id"].as_str().unwrap().parse().unwrap()
}

/// Makes `doc` a restriction boundary with `actor` as its only owner, so
/// nobody else (the workspace owner included) can open it.
async fn restrict_for(fx: &Fixture, doc: Uuid, actor: Uuid) {
    fx.state
        .docs
        .as_ref()
        .unwrap()
        .set_restricted(fx.workspace_id, doc, actor, true)
        .await
        .unwrap();
}

/// Moves `doc` to the archive as the owner.
async fn archive(fx: &Fixture, doc: Uuid) {
    let answer = send(
        fx,
        Some(&fx.owner.session),
        "DELETE",
        &format!("/api/docs/{doc}"),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::NO_CONTENT,
        "archive: {}",
        answer.body
    );
}

/// Replaces the page's content with `markdown`, as the owner.
async fn set_markdown(fx: &Fixture, doc: Uuid, markdown: &str) {
    let answer = send(
        fx,
        Some(&fx.owner.session),
        "POST",
        &format!("/api/docs/{doc}/markdown?mode=replace"),
        Some("text/markdown"),
        markdown.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::NO_CONTENT,
        "markdown: {}",
        answer.body
    );
}

/// A `multipart/form-data` body with one PNG part per `(name, bytes)`.
fn multipart(parts: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "----AuditTestBoundary";
    let mut body = Vec::new();
    for (name, bytes) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"picture\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// `POST /api/docs/{doc}/export/{format}` as `session` with one PNG part
/// per `(name, bytes)`, or an empty body when there are none. A 200 comes
/// only once its audit row is written, so the row is there already.
async fn export_file(
    fx: &Fixture,
    session: &Session,
    doc: Uuid,
    format: &str,
    pictures: &[(&str, &[u8])],
) -> Answer {
    let (content_type, body) = if pictures.is_empty() {
        (None, Vec::new())
    } else {
        let (content_type, body) = multipart(pictures);
        (Some(content_type), body)
    };
    let rows_before = export_row_count(&fx.pool).await;
    let answer = send(
        fx,
        Some(session),
        "POST",
        &format!("/api/docs/{doc}/export/{format}"),
        content_type.as_deref(),
        body,
    )
    .await;
    if answer.status == StatusCode::OK {
        assert_eq!(
            export_row_count(&fx.pool).await,
            rows_before + 1,
            "the docx/odt export went out before its audit row was written"
        );
    }
    answer
}

/// `GET uri` (a zip export) as `session`, asserting 200 and that its audit
/// rows (all in one INSERT) were written before the zip went out.
async fn export_zip(fx: &Fixture, session: &Session, uri: &str) {
    let rows_before = export_row_count(&fx.pool).await;
    let answer = send(fx, Some(session), "GET", uri, None, Vec::new()).await;
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "export {uri}: {}",
        answer.body
    );
    assert!(
        export_row_count(&fx.pool).await > rows_before,
        "the zip export {uri} went out before its audit rows were written"
    );
}

/// The `doc.export` rows on `doc`, oldest first: actor and stored data.
async fn export_rows(fx: &Fixture, doc: Uuid) -> Vec<(Option<Uuid>, Value)> {
    sqlx::query_as(
        "SELECT actor_id, data FROM audit_events
          WHERE target_id = $1 AND action = 'doc.export' ORDER BY id",
    )
    .bind(doc)
    .fetch_all(&fx.pool)
    .await
    .unwrap()
}

async fn export_row_count(pool: &knot_storage::Pool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'doc.export'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Inserts an audit row on `doc` straight into the table; returns its id.
async fn insert_event(
    fx: &Fixture,
    doc: Uuid,
    action: &str,
    actor_id: Option<Uuid>,
    data: Value,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data)
         VALUES ($1, $2, $3, 'doc', $4, $5) RETURNING id",
    )
    .bind(fx.workspace_id)
    .bind(actor_id)
    .bind(action)
    .bind(doc)
    .bind(data)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
}

/// The event ids (opaque tokens) in `body`.
fn event_ids(body: &Value) -> Vec<String> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_str().unwrap().to_string())
        .collect()
}

/// The token `doc`'s log shows the stored event `id` as.
fn token(fx: &Fixture, doc: Uuid, id: i64) -> String {
    knot_server::routes::api::audit::EventTokens::new(&fx.state.session_key, doc).seal(id)
}

/// The first event with `action` in `body`.
#[track_caller]
fn first_event<'body>(body: &'body Value, action: &str) -> &'body Value {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["action"] == action)
        .unwrap_or_else(|| panic!("no {action} in {body}"))
}

// ---------------------------------------------------------------------------
// Who may read it
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn editors_read_the_audit_log_and_viewers_are_refused() {
    let fx = fixture().await;
    let page = create_page(&fx, "Plans", None).await;

    for member in [&fx.owner, &fx.editor] {
        let answer = audit_log(&fx, Some(&member.session), page, "").await;
        let body = answer.ok();
        assert_eq!(answer.cache_control.as_deref(), Some("private, no-store"));
        assert_eq!(first_event(&body, "doc.create")["actor"]["name"], "Olga");
    }
    audit_log(&fx, Some(&fx.viewer.session), page, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    audit_log(&fx, None, page, "")
        .await
        .error(StatusCode::UNAUTHORIZED, "auth.session_required");

    // Archived: editors still read it, viewers lose sight of the page.
    archive(&fx, page).await;
    let body = audit_log(&fx, Some(&fx.editor.session), page, "")
        .await
        .ok();
    first_event(&body, "doc.archive");
    audit_log(&fx, Some(&fx.viewer.session), page, "")
        .await
        .error(StatusCode::NOT_FOUND, "doc.not_found");

    // A restricted page without a grant: the page's effective role decides,
    // so the workspace owner is refused like the editor, and the refusal
    // names nothing.
    let secret = create_page(&fx, "Secret plans", None).await;
    restrict_for(&fx, secret, fx.viewer.id).await;
    for member in [&fx.owner, &fx.editor] {
        let answer = audit_log(&fx, Some(&member.session), secret, "").await;
        answer.error(StatusCode::FORBIDDEN, "acl.no_grant");
        assert!(
            !answer.body.contains(&secret.to_string()),
            "{}",
            answer.body
        );
        assert!(!answer.body.contains("Secret"), "{}", answer.body);
    }
    // An unknown page is the same uniform refusal.
    audit_log(&fx, Some(&fx.owner.session), Uuid::new_v4(), "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.no_grant");

    // The page's role, not the workspace role, decides either way: the
    // workspace viewer owns `secret` and reads its log...
    let body = audit_log(&fx, Some(&fx.viewer.session), secret, "")
        .await
        .ok();
    first_event(&body, "doc.create");
    // ...and a workspace editor who only views a restricted page is refused
    // as a viewer.
    let vault = create_page(&fx, "Vault", None).await;
    restrict_for(&fx, vault, fx.owner.id).await;
    let answer = send_json(
        &fx,
        &fx.owner.session,
        "PUT",
        &format!("/api/docs/{vault}/grants/user:{}", fx.editor.id),
        json!({"role": "viewer", "inherit": true}),
    )
    .await;
    assert!(answer.status.is_success(), "grant: {}", answer.body);
    audit_log(&fx, Some(&fx.editor.session), vault, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    audit_log(&fx, Some(&fx.owner.session), vault, "")
        .await
        .ok();
}

// ---------------------------------------------------------------------------
// What is recorded
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_viewers_docx_export_is_recorded_with_who_and_format() {
    let fx = fixture().await;
    let page = create_page(&fx, "Flow", None).await;
    set_markdown(&fx, page, "```mermaid\ngraph TD; A-->B\n```\n").await;
    let diagram_key = format!("mermaid-{:x}", Sha256::digest(b"graph TD; A-->B"));

    let answer = export_file(
        &fx,
        &fx.viewer.session,
        page,
        "docx",
        &[(&diagram_key, TINY_PNG)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    let body = audit_log(&fx, Some(&fx.editor.session), page, "")
        .await
        .ok();
    let export = first_event(&body, "doc.export");
    assert_eq!(export["category"], "exports");
    assert_eq!(export["actor_kind"], "user");
    assert_eq!(
        export["actor"],
        json!({"key": fx.viewer.id, "name": "Vera", "former": false})
    );
    assert_eq!(
        export["details"],
        json!({"format": "docx", "scope": "page", "diagram_pictures": 1})
    );
    let exports = &body["stats"]["exports"];
    assert_eq!(exports["total"], 1);
    assert_eq!(exports["people"], 1);
    assert_eq!(exports["last_30_days"], 1);
    assert_eq!(exports["last"]["format"], "docx");
    assert_eq!(exports["last"]["scope"], "page");
    assert_eq!(exports["last"]["actor"]["name"], "Vera");

    // The operators' copy keeps the fallbacks.
    let rows = export_rows(&fx, page).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(fx.viewer.id));
    assert_eq!(
        rows[0].1["fallbacks"],
        json!({"diagram": 0, "image_unavailable": 0, "image_over_budget": 0, "unknown_element": 0})
    );

    // And as OpenDocument, without pictures.
    let answer = export_file(&fx, &fx.viewer.session, page, "odt", &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let body = audit_log(&fx, Some(&fx.owner.session), page, "").await.ok();
    assert_eq!(
        first_event(&body, "doc.export")["details"],
        json!({"format": "odt", "scope": "page", "diagram_pictures": 0})
    );
    let exports = &body["stats"]["exports"];
    assert_eq!(exports["total"], 2);
    assert_eq!(exports["people"], 1);
    assert_eq!(exports["by_format"], json!({"docx": 1, "odt": 1}));
    assert_eq!(exports["by_scope"], json!({"page": 2}));
    assert_eq!(exports["last"]["format"], "odt");
    assert_eq!(exports["by_person"][0]["count"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_diagrams_preflight_and_markdown_read_record_nothing() {
    let fx = fixture().await;
    let page = create_page(&fx, "Read only", None).await;
    for uri in [
        format!("/api/docs/{page}/export/diagrams"),
        format!("/api/docs/{page}/markdown"),
    ] {
        let answer = send(&fx, Some(&fx.viewer.session), "GET", &uri, None, Vec::new()).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.body);
    }
    assert_eq!(export_row_count(&fx.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_and_failed_exports_record_nothing() {
    let fx = fixture().await;
    let page = create_page(&fx, "Page", None).await;

    export_file(&fx, &fx.owner.session, page, "pdf", &[])
        .await
        .error(StatusCode::BAD_REQUEST, "export.bad_format");
    let huge = {
        let mut png = TINY_PNG.to_vec();
        png.resize(11 * 1024 * 1024, 0);
        png
    };
    export_file(&fx, &fx.owner.session, page, "docx", &[("picture", &huge)])
        .await
        .error(StatusCode::PAYLOAD_TOO_LARGE, "export.too_large");
    let archived = create_page(&fx, "Old", None).await;
    archive(&fx, archived).await;
    export_file(&fx, &fx.owner.session, archived, "docx", &[])
        .await
        .error(StatusCode::CONFLICT, "doc.archived");
    let answer = send(
        &fx,
        Some(&fx.owner.session),
        "GET",
        &format!("/api/docs/{archived}/export"),
        None,
        Vec::new(),
    )
    .await;
    assert!(answer.status.is_client_error(), "{}", answer.body);
    assert_eq!(export_row_count(&fx.pool).await, 0);

    let busy = fixture_with(|state| {
        state.export_permits = Arc::new(Semaphore::new(0));
        state.export_busy_wait = Duration::from_millis(50);
    })
    .await;
    let busy_page = create_page(&busy, "Busy", None).await;
    export_file(&busy, &busy.owner.session, busy_page, "docx", &[])
        .await
        .error(StatusCode::SERVICE_UNAVAILABLE, "export.busy");
    assert_eq!(export_row_count(&busy.pool).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zip_page_export_is_recorded_once() {
    let fx = fixture().await;
    let page = create_page(&fx, "Notes", None).await;
    let child = create_page(&fx, "Child", Some(page)).await;

    export_zip(&fx, &fx.editor.session, &format!("/api/docs/{page}/export")).await;

    let rows = export_rows(&fx, page).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (actor_id, data) = &rows[0];
    assert_eq!(*actor_id, Some(fx.editor.id));
    assert_eq!(data["format"], "markdown_zip");
    assert_eq!(data["scope"], "page");
    assert_eq!(data["doc_count"], 1);
    Uuid::parse_str(data["export_id"].as_str().unwrap()).expect("export_id is a uuid");
    assert!(
        export_rows(&fx, child).await.is_empty(),
        "the child was not in it"
    );

    let body = audit_log(&fx, Some(&fx.owner.session), page, "").await.ok();
    let export = first_event(&body, "doc.export");
    assert_eq!(
        export["details"],
        json!({"format": "markdown_zip", "scope": "page"})
    );
    assert_eq!(export["actor"]["name"], "Eddie");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subtree_export_records_every_included_page_and_no_restricted_one() {
    let fx = fixture().await;
    let root = create_page(&fx, "Handbook", None).await;
    let chapter = create_page(&fx, "Chapter", Some(root)).await;
    let section = create_page(&fx, "Section", Some(chapter)).await;
    let hidden = create_page(&fx, "Hidden", Some(root)).await;
    restrict_for(&fx, hidden, fx.viewer.id).await;

    export_zip(
        &fx,
        &fx.owner.session,
        &format!("/api/docs/{root}/export?descendants=true"),
    )
    .await;

    let mut export_ids = Vec::new();
    for page in [root, chapter, section] {
        let rows = export_rows(&fx, page).await;
        assert_eq!(rows.len(), 1, "{page}: {rows:?}");
        let (actor_id, data) = &rows[0];
        assert_eq!(*actor_id, Some(fx.owner.id));
        assert_eq!(data["format"], "markdown_zip");
        assert_eq!(data["scope"], "subtree");
        assert_eq!(data["root_id"], root.to_string());
        assert_eq!(data["doc_count"], 3);
        export_ids.push(data["export_id"].clone());
    }
    assert!(
        export_ids.iter().all(|id| *id == export_ids[0]),
        "{export_ids:?}"
    );
    assert!(
        export_rows(&fx, hidden).await.is_empty(),
        "a page the exporter can't read has no row"
    );

    // On the root the row says no more; below it, it names the root.
    let body = audit_log(&fx, Some(&fx.editor.session), root, "")
        .await
        .ok();
    assert_eq!(
        first_event(&body, "doc.export")["details"],
        json!({"format": "markdown_zip", "scope": "subtree"})
    );
    let body = audit_log(&fx, Some(&fx.editor.session), section, "")
        .await
        .ok();
    assert_eq!(
        first_event(&body, "doc.export")["details"],
        json!({"format": "markdown_zip", "scope": "subtree",
               "root": {"id": root, "title": "Handbook"}, "root_hidden": false})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subtree_row_hides_a_root_the_reader_cannot_open() {
    let fx = fixture().await;
    let vault = create_page(&fx, "Vault", None).await;
    let shared = create_page(&fx, "Shared", Some(vault)).await;
    restrict_for(&fx, vault, fx.owner.id).await;
    let answer = send_json(
        &fx,
        &fx.owner.session,
        "PUT",
        &format!("/api/docs/{shared}/grants/user:{}", fx.editor.id),
        json!({"role": "editor", "inherit": true}),
    )
    .await;
    assert!(answer.status.is_success(), "grant: {}", answer.body);
    export_zip(
        &fx,
        &fx.owner.session,
        &format!("/api/docs/{vault}/export?descendants=true"),
    )
    .await;

    audit_log(&fx, Some(&fx.editor.session), vault, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.no_grant");
    let answer = audit_log(&fx, Some(&fx.editor.session), shared, "").await;
    let body = answer.ok();
    assert_eq!(
        first_event(&body, "doc.export")["details"],
        json!({"format": "markdown_zip", "scope": "subtree", "root": null, "root_hidden": true})
    );
    assert!(!answer.body.contains(&vault.to_string()), "{}", answer.body);
    assert!(!answer.body.contains("Vault"), "{}", answer.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_export_records_each_readable_page() {
    let fx = fixture().await;
    let first_page = create_page(&fx, "First", None).await;
    let second_page = create_page(&fx, "Second", Some(first_page)).await;
    let hidden = create_page(&fx, "Hidden", None).await;
    restrict_for(&fx, hidden, fx.viewer.id).await;

    export_zip(&fx, &fx.owner.session, "/api/workspace/export").await;

    let mut export_ids = Vec::new();
    for page in [first_page, second_page] {
        let rows = export_rows(&fx, page).await;
        assert_eq!(rows.len(), 1, "{page}: {rows:?}");
        let (actor_id, data) = &rows[0];
        assert_eq!(*actor_id, Some(fx.owner.id));
        assert_eq!(data["scope"], "workspace");
        assert_eq!(data["format"], "markdown_zip");
        assert_eq!(data["doc_count"], 2);
        assert!(data.get("root_id").is_none(), "{data}");
        export_ids.push(data["export_id"].clone());
    }
    assert_eq!(export_ids[0], export_ids[1]);
    assert!(export_rows(&fx, hidden).await.is_empty());

    let body = audit_log(&fx, Some(&fx.editor.session), second_page, "")
        .await
        .ok();
    assert_eq!(
        first_event(&body, "doc.export")["details"],
        json!({"format": "markdown_zip", "scope": "workspace"})
    );
    assert_eq!(
        body["stats"]["exports"]["by_scope"],
        json!({"workspace": 1})
    );
}

// ---------------------------------------------------------------------------
// Fail closed: an export that can't be recorded is refused
// ---------------------------------------------------------------------------

/// `knot_audit_write_failures_total` by `action`, from a recorder installed
/// once for this whole test binary. The count is shared by every test in
/// it, so a test that makes `doc.export` inserts fail and checks the count
/// holds [`failing_export_writes`] throughout.
fn audit_failures(action: &str) -> u64 {
    audit_failure_counts()
        .lock()
        .unwrap()
        .get(action)
        .map_or(0, |count| count.load(Ordering::Relaxed))
}

type FailureCounts = Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Held by each test that makes `doc.export` inserts fail, so no other one
/// adds to the shared count it reads.
async fn failing_export_writes() -> tokio::sync::MutexGuard<'static, ()> {
    static FAILING_EXPORT_WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    FAILING_EXPORT_WRITES.lock().await
}

fn audit_failure_counts() -> &'static FailureCounts {
    static COUNTS: OnceLock<FailureCounts> = OnceLock::new();
    COUNTS.get_or_init(|| {
        let counts = FailureCounts::default();
        metrics::set_global_recorder(AuditFailureRecorder(Arc::clone(&counts)))
            .expect("no other recorder in this test binary");
        counts
    })
}

struct AuditFailureRecorder(FailureCounts);

impl metrics::Recorder for AuditFailureRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        if key.name() != "knot_audit_write_failures_total" {
            return metrics::Counter::noop();
        }
        let action = key
            .labels()
            .find(|label| label.key() == "action")
            .map(|label| label.value().to_owned())
            .unwrap_or_default();
        let count = Arc::clone(self.0.lock().unwrap().entry(action).or_default());
        metrics::Counter::from_arc(count)
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

/// Makes every `doc.export` insert into this test's `audit_events` fail, as
/// an audit database that can't take the write would. Other actions still
/// land.
async fn refuse_export_rows(pool: &knot_storage::Pool) {
    sqlx::query(
        "CREATE FUNCTION refuse_export_rows() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.action = 'doc.export' THEN
             RAISE EXCEPTION 'doc.export rows refused by the test';
           END IF;
           RETURN NEW;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_export_rows BEFORE INSERT ON audit_events
         FOR EACH ROW EXECUTE FUNCTION refuse_export_rows()",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A refusal for an export the audit log couldn't take: the error envelope
/// and no file.
#[track_caller]
fn assert_refused_unaudited(answer: &Answer, what: &str) {
    answer.error(StatusCode::SERVICE_UNAVAILABLE, "audit.unavailable");
    assert!(
        answer.content_disposition.is_none(),
        "{what} still offered a file: {:?}",
        answer.content_disposition
    );
    assert!(
        !answer.body.starts_with("PK"),
        "{what} sent the file's bytes"
    );
}

/// An export whose `doc.export` rows can't be written is refused with
/// `503 audit.unavailable` and no file: docx, odt and each Markdown zip.
/// Nothing is recorded, and every refusal counts in
/// `knot_audit_write_failures_total{action="doc.export"}`.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_that_cannot_be_recorded_is_refused() {
    let _only_failing_test = failing_export_writes().await;
    let failures_before = audit_failures("doc.export");
    let fx = fixture().await;
    let root = create_page(&fx, "Unaudited", None).await;
    create_page(&fx, "Below", Some(root)).await;
    refuse_export_rows(&fx.pool).await;

    for format in ["docx", "odt"] {
        let answer = export_file(&fx, &fx.viewer.session, root, format, &[]).await;
        assert_refused_unaudited(&answer, format);
    }
    for uri in [
        format!("/api/docs/{root}/export"),
        format!("/api/docs/{root}/export?descendants=true"),
        "/api/workspace/export".to_string(),
    ] {
        let answer = send(&fx, Some(&fx.owner.session), "GET", &uri, None, Vec::new()).await;
        assert_refused_unaudited(&answer, &uri);
    }

    assert_eq!(export_row_count(&fx.pool).await, 0);
    assert_eq!(audit_failures("doc.export") - failures_before, 5);

    // HEAD sends no file, so it isn't recorded and still answers.
    let answer = send(
        &fx,
        Some(&fx.owner.session),
        "HEAD",
        &format!("/api/docs/{root}/export"),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    // Once the audit log takes the rows again, the export goes out.
    sqlx::query("DROP TRIGGER refuse_export_rows ON audit_events")
        .execute(&fx.pool)
        .await
        .unwrap();
    let answer = export_file(&fx, &fx.viewer.session, root, "docx", &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    export_zip(&fx, &fx.owner.session, &format!("/api/docs/{root}/export")).await;
}

/// The export waits for its audit insert: while the insert is held up (here
/// on a lock) the file doesn't go out, and once the insert lands it does,
/// with the row already written.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_waits_for_a_stalled_audit_write_then_goes_out() {
    let fx = Arc::new(fixture().await);
    let page = create_page(&fx, "Stalled", None).await;

    for (method, uri) in [
        ("POST", format!("/api/docs/{page}/export/docx")),
        ("GET", format!("/api/docs/{page}/export")),
    ] {
        let rows_before = export_rows(&fx, page).await.len();
        // SHARE blocks every INSERT into audit_events until the rollback.
        let mut lock = fx.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE audit_events IN SHARE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
        let export = tokio::spawn({
            let fx = Arc::clone(&fx);
            let uri = uri.clone();
            async move { send(&fx, Some(&fx.owner.session), method, &uri, None, Vec::new()).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !export.is_finished(),
            "{method} {uri} went out before its audit row was written"
        );
        lock.rollback().await.unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(10), export)
            .await
            .expect("the export never went out once the audit insert landed")
            .unwrap();
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "{method} {uri}: {}",
            answer.body
        );
        assert!(
            answer.content_disposition.is_some(),
            "{method} {uri}: no file"
        );
        assert_eq!(
            export_rows(&fx, page).await.len(),
            rows_before + 1,
            "{method} {uri}: the row is there by the time the file is"
        );
    }
}

/// An audit insert that hangs (here on a lock) instead of failing refuses
/// the export the same way, with `503 audit.unavailable`, counted, once
/// `EXPORT_AUDIT_WRITE_TIMEOUT` has passed: well before the 30 s request
/// timeout would turn it into an uncounted 408. The abandoned insert never
/// lands once the lock goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_audit_write_refuses_the_export_before_the_request_timeout() {
    let _only_failing_test = failing_export_writes().await;
    let fx = fixture().await;
    let page = create_page(&fx, "Stalled for good", None).await;
    let failures_before = audit_failures("doc.export");
    // SHARE blocks every INSERT into audit_events until the rollback.
    let mut lock_holder = fx.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE audit_events IN SHARE MODE")
        .execute(&mut *lock_holder)
        .await
        .unwrap();

    for (method, uri) in [
        ("POST", format!("/api/docs/{page}/export/docx")),
        ("GET", format!("/api/docs/{page}/export")),
    ] {
        let started = std::time::Instant::now();
        let answer = send(&fx, Some(&fx.owner.session), method, &uri, None, Vec::new()).await;
        let waited = started.elapsed();
        assert_refused_unaudited(&answer, &format!("{method} {uri}"));
        assert!(
            waited < knot_storage::audit::EXPORT_AUDIT_WRITE_TIMEOUT + Duration::from_secs(5),
            "{method} {uri}: refused only after {waited:?}"
        );
    }
    assert_eq!(audit_failures("doc.export") - failures_before, 2);

    lock_holder.rollback().await.unwrap();
    // Long enough for an insert still queued on the server to commit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        export_row_count(&fx.pool).await,
        0,
        "a refused export was recorded after all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn head_on_a_zip_export_records_nothing() {
    let fx = fixture().await;
    let page = create_page(&fx, "Head", None).await;
    for uri in [
        format!("/api/docs/{page}/export"),
        format!("/api/docs/{page}/export?descendants=true"),
        "/api/workspace/export".to_string(),
    ] {
        let answer = send(&fx, Some(&fx.owner.session), "HEAD", &uri, None, Vec::new()).await;
        assert_eq!(answer.status, StatusCode::OK, "HEAD {uri}");
    }
    assert_eq!(export_row_count(&fx.pool).await, 0);
}

// ---------------------------------------------------------------------------
// What is shown
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn operator_fields_never_reach_the_client() {
    let fx = fixture().await;
    let root = create_page(&fx, "Root", None).await;
    let child = create_page(&fx, "Child", Some(root)).await;
    let answer = export_file(&fx, &fx.owner.session, child, "docx", &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    export_zip(
        &fx,
        &fx.owner.session,
        &format!("/api/docs/{root}/export?descendants=true"),
    )
    .await;
    export_zip(&fx, &fx.owner.session, "/api/workspace/export").await;
    let (key_id, key) = mint_key(&fx, fx.owner.id, fx.owner.id, &["read"]).await;
    for (method, uri) in [
        ("POST", format!("/api/docs/{child}/export/docx")),
        ("GET", format!("/api/docs/{root}/export?descendants=true")),
    ] {
        assert_exported(&send_with_key(&fx, &key, method, &uri).await, &uri);
    }
    let answer = send_json(
        &fx,
        &fx.owner.session,
        "POST",
        &format!("/api/docs/{child}/duplicate"),
        json!({}),
    )
    .await;
    assert!(answer.status.is_success(), "duplicate: {}", answer.body);
    let copy: Uuid =
        sqlx::query_scalar("SELECT target_id FROM audit_events WHERE action = 'doc.duplicate'")
            .fetch_one(&fx.pool)
            .await
            .unwrap();

    let key_id_text = key_id.to_string();
    for page in [root, child, copy] {
        let answer = audit_log(&fx, Some(&fx.editor.session), page, "").await;
        answer.ok();
        for operator_field in [
            "doc_count",
            "export_id",
            "fallbacks",
            "source_id",
            "api_key_id",
            &key_id_text,
        ] {
            assert!(
                !answer.body.contains(operator_field),
                "{operator_field} on {page}: {}",
                answer.body
            );
        }
        assert!(!answer.body.contains('@'), "an email: {}", answer.body);
    }
    let body = audit_log(&fx, Some(&fx.editor.session), copy, "")
        .await
        .ok();
    assert_eq!(first_event(&body, "doc.duplicate")["details"], json!({}));
    assert_eq!(first_event(&body, "doc.duplicate")["category"], "changes");
}

/// One export writes its rows in one statement, so their ids are
/// consecutive: the log must not send them, or the gaps between the pages a
/// reader can open would count the ones they can't (what `doc_count` is
/// withheld for).
#[tokio::test(flavor = "multi_thread")]
async fn event_ids_and_cursors_do_not_reveal_the_stored_ids() {
    let fx = fixture().await;
    let root = create_page(&fx, "Root", None).await;
    let first_child = create_page(&fx, "A", Some(root)).await;
    let hidden = create_page(&fx, "B", Some(root)).await;
    let last_child = create_page(&fx, "C", Some(root)).await;
    // The workspace viewer owns `hidden` and exports all four pages.
    restrict_for(&fx, hidden, fx.viewer.id).await;
    export_zip(
        &fx,
        &fx.viewer.session,
        &format!("/api/docs/{root}/export?descendants=true"),
    )
    .await;
    assert_eq!(export_rows(&fx, hidden).await.len(), 1);

    let stored_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM audit_events")
        .fetch_all(&fx.pool)
        .await
        .unwrap();
    let mut shown_tokens = Vec::new();
    for page in [root, first_child, last_child] {
        let body = audit_log(&fx, Some(&fx.editor.session), page, "limit=1")
            .await
            .ok();
        let event_id = &first_event(&body, "doc.export")["id"];
        let token = event_id
            .as_str()
            .unwrap_or_else(|| panic!("an event id is an opaque string: {event_id}"));
        assert_eq!(token.len(), 16, "{token}");
        let cursor = body["next_cursor"]
            .as_str()
            .unwrap_or_else(|| panic!("the cursor is an opaque string: {body}"));
        assert_eq!(cursor, token, "the cursor is the last event's id");
        for stored_id in &stored_ids {
            assert_ne!(token, stored_id.to_string(), "{body}");
            assert_ne!(token.trim_start_matches('0'), format!("{stored_id:x}"));
        }
        shown_tokens.push(token.to_string());
    }
    shown_tokens.sort();
    shown_tokens.dedup();
    assert_eq!(shown_tokens.len(), 3, "{shown_tokens:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_on_the_first_page_only_and_independent_of_category() {
    let fx = fixture().await;
    let page = create_page(&fx, "Stats", None).await;
    let answer = export_file(&fx, &fx.viewer.session, page, "docx", &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    for title in ["Stats 2", "Stats 3"] {
        let answer = send_json(
            &fx,
            &fx.owner.session,
            "PATCH",
            &format!("/api/docs/{page}"),
            json!({"title": title}),
        )
        .await;
        assert!(answer.status.is_success(), "rename: {}", answer.body);
    }

    let everything = audit_log(&fx, Some(&fx.editor.session), page, "")
        .await
        .ok();
    let stats = &everything["stats"];
    assert_eq!(stats["total"], 4);
    let by_action: Vec<(String, i64)> = stats["by_action"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| {
            (
                action["action"].as_str().unwrap().to_string(),
                action["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_action,
        vec![
            ("doc.rename".to_string(), 2),
            ("doc.create".to_string(), 1),
            ("doc.export".to_string(), 1)
        ]
    );
    for category in ["changes", "exports", "access", "all"] {
        let body = audit_log(
            &fx,
            Some(&fx.editor.session),
            page,
            &format!("category={category}"),
        )
        .await
        .ok();
        assert_eq!(body["stats"], *stats, "{category}");
    }
    let access = audit_log(&fx, Some(&fx.editor.session), page, "category=access")
        .await
        .ok();
    assert_eq!(access["events"], json!([]));

    let first_page = audit_log(&fx, Some(&fx.editor.session), page, "limit=1")
        .await
        .ok();
    assert!(first_page["stats"].is_object());
    let cursor = first_page["next_cursor"].as_str().expect("a next cursor");
    let next_page = audit_log(
        &fx,
        Some(&fx.editor.session),
        page,
        &format!("cursor={cursor}"),
    )
    .await
    .ok();
    assert_eq!(next_page["stats"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn paging_and_bad_input() {
    let fx = fixture().await;
    let page = create_page(&fx, "Paged", None).await;
    for _ in 0..4 {
        insert_event(&fx, page, "doc.rename", Some(fx.owner.id), json!({})).await;
    }
    let stored_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM audit_events WHERE target_id = $1 ORDER BY id DESC")
            .bind(page)
            .fetch_all(&fx.pool)
            .await
            .unwrap();
    assert_eq!(stored_ids.len(), 5);
    let all_ids: Vec<String> = stored_ids.iter().map(|id| token(&fx, page, *id)).collect();

    let mut seen = Vec::new();
    let mut query = "limit=1".to_string();
    loop {
        let body = audit_log(&fx, Some(&fx.editor.session), page, &query)
            .await
            .ok();
        let ids = event_ids(&body);
        assert_eq!(ids.len(), 1, "{body}");
        seen.extend(ids);
        match body["next_cursor"].as_str() {
            Some(cursor) => {
                assert_eq!(cursor, seen.last().unwrap());
                query = format!("limit=1&cursor={cursor}");
            }
            None => break,
        }
    }
    assert_eq!(seen, all_ids, "no gaps, no repeats, newest first");

    let two = audit_log(&fx, Some(&fx.editor.session), page, "limit=2")
        .await
        .ok();
    let rest = audit_log(
        &fx,
        Some(&fx.editor.session),
        page,
        &format!("cursor={}", two["next_cursor"].as_str().unwrap()),
    )
    .await
    .ok();
    let mut joined = event_ids(&two);
    joined.extend(event_ids(&rest));
    assert_eq!(joined, all_ids);
    assert_eq!(rest["next_cursor"], Value::Null);

    // A stored id is no cursor.
    let stored_id_cursor = format!("cursor={}", stored_ids[1]);
    for (query, message) in [
        ("cursor=abc", "bad cursor"),
        ("cursor=0", "bad cursor"),
        (stored_id_cursor.as_str(), "bad cursor"),
        ("limit=0", "bad limit"),
        ("limit=201", "bad limit"),
        ("category=foo", "bad category"),
    ] {
        let answer = audit_log(&fx, Some(&fx.editor.session), page, query).await;
        answer.error(StatusCode::BAD_REQUEST, "audit.bad_request");
        assert_eq!(answer.json()["error"]["message"], message, "{query}");
    }
    audit_log(&fx, Some(&fx.editor.session), page, "limit=200&cursor=")
        .await
        .ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn category_filter_keeps_paging_correct() {
    let fx = fixture().await;
    let page = create_page(&fx, "Mixed", None).await;
    let mut export_ids = Vec::new();
    for round in 0..3 {
        let export_id = insert_event(
            &fx,
            page,
            "doc.export",
            Some(fx.viewer.id),
            json!({"format": "docx", "scope": "page", "round": round}),
        )
        .await;
        export_ids.push(token(&fx, page, export_id));
        insert_event(&fx, page, "doc.rename", Some(fx.owner.id), json!({})).await;
    }
    export_ids.reverse();

    let first = audit_log(
        &fx,
        Some(&fx.editor.session),
        page,
        "category=exports&limit=2",
    )
    .await
    .ok();
    let cursor = first["next_cursor"].as_str().expect("more exports");
    let rest = audit_log(
        &fx,
        Some(&fx.editor.session),
        page,
        &format!("category=exports&limit=2&cursor={cursor}"),
    )
    .await
    .ok();
    assert_eq!(rest["next_cursor"], Value::Null);
    let mut seen = event_ids(&first);
    seen.extend(event_ids(&rest));
    assert_eq!(seen, export_ids);
    for body in [&first, &rest] {
        for event in body["events"].as_array().unwrap() {
            assert_eq!(event["category"], "exports");
            assert!(event["details"].get("round").is_none(), "{event}");
        }
    }

    let changes = audit_log(&fx, Some(&fx.editor.session), page, "category=changes")
        .await
        .ok();
    let actions: Vec<&str> = changes["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        vec!["doc.rename", "doc.rename", "doc.rename", "doc.create"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn system_and_former_actors() {
    let fx = fixture().await;
    let page = create_page(&fx, "Actors", None).await;
    knot_server::admin::grant(&fx.pool, page, fx.editor.email, "owner", false)
        .await
        .expect("break-glass grant");
    insert_event(&fx, page, "doc.purge", None, json!({})).await;
    let (ghost, first_former, second_former) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_event(&fx, page, "doc.rename", Some(ghost), json!({})).await;
    for exporter in [first_former, second_former, fx.viewer.id, first_former] {
        insert_event(
            &fx,
            page,
            "doc.export",
            Some(exporter),
            json!({"format": "odt", "scope": "page"}),
        )
        .await;
    }

    let answer = audit_log(&fx, Some(&fx.editor.session), page, "").await;
    let body = answer.ok();
    let break_glass = first_event(&body, "admin.grant");
    assert_eq!(break_glass["actor"], Value::Null);
    assert_eq!(break_glass["actor_kind"], "admin_cli");
    assert_eq!(break_glass["category"], "access");
    assert_eq!(
        break_glass["details"],
        json!({"principal": {"kind": "user",
                             "person": {"key": fx.editor.id, "name": "Eddie", "former": false}},
               "role": "owner", "inherit": true, "break_glass": true})
    );
    let purge = first_event(&body, "doc.purge");
    assert_eq!(purge["actor"], Value::Null);
    assert_eq!(purge["actor_kind"], "retention");
    let rename = first_event(&body, "doc.rename");
    assert_eq!(rename["actor_kind"], "user");
    assert_eq!(
        rename["actor"],
        json!({"key": null, "name": null, "former": true})
    );
    let create = first_event(&body, "doc.create");
    assert_eq!(create["actor"]["name"], "Olga");

    let exports = &body["stats"]["exports"];
    assert_eq!(exports["total"], 4);
    assert_eq!(exports["people"], 2, "two former exporters count once");
    let by_person = exports["by_person"].as_array().unwrap();
    assert_eq!(by_person.len(), 2, "{by_person:?}");
    assert_eq!(
        by_person[0],
        json!({"actor": {"key": null, "name": null, "former": true}, "count": 3,
               "last_at": by_person[0]["last_at"]})
    );
    assert_eq!(by_person[1]["actor"]["name"], "Vera");
    assert_eq!(by_person[1]["count"], 1);
    assert_eq!(exports["last"]["actor"]["former"], true);

    for hidden in [ghost, first_former, second_former] {
        assert!(
            !answer.body.contains(&hidden.to_string()),
            "{}",
            answer.body
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_rows_name_the_principal() {
    let fx = fixture().await;
    let page = create_page(&fx, "Granted", None).await;
    let principal = format!("user:{}", fx.editor.id);
    let answer = send_json(
        &fx,
        &fx.owner.session,
        "PUT",
        &format!("/api/docs/{page}/grants/{principal}"),
        json!({"role": "editor", "inherit": false}),
    )
    .await;
    assert!(answer.status.is_success(), "grant: {}", answer.body);
    let answer = send(
        &fx,
        Some(&fx.owner.session),
        "DELETE",
        &format!("/api/docs/{page}/grants/{principal}"),
        None,
        Vec::new(),
    )
    .await;
    assert!(answer.status.is_success(), "revoke: {}", answer.body);

    let body = audit_log(&fx, Some(&fx.editor.session), page, "category=access")
        .await
        .ok();
    let eddie = json!({"key": fx.editor.id, "name": "Eddie", "former": false});
    let grant = first_event(&body, "doc.grant");
    assert_eq!(grant["actor"]["name"], "Olga");
    assert_eq!(
        grant["details"],
        json!({"principal": {"kind": "user", "person": eddie}, "role": "editor", "inherit": false})
    );
    assert_eq!(
        first_event(&body, "doc.grant.delete")["details"],
        json!({"principal": {"kind": "user", "person": eddie}})
    );
}

// ---------------------------------------------------------------------------
// API keys and bots
// ---------------------------------------------------------------------------

/// A live API key of `user_id` with `scopes`, minted by `minter` (the user
/// itself, or the workspace owner for a bot) straight through the store:
/// its id and its secret.
async fn mint_key(fx: &Fixture, user_id: Uuid, minter: Uuid, scopes: &[&str]) -> (Uuid, String) {
    let secret = knot_auth::ApiKeySecret::generate();
    let scope_names: Vec<String> = scopes.iter().map(|scope| scope.to_string()).collect();
    let scopes = knot_storage::api_key_store::Scopes::from_names(&scope_names).unwrap();
    let key = fx
        .state
        .api_keys
        .as_ref()
        .unwrap()
        .create(knot_storage::api_key_store::NewApiKey {
            workspace_id: fx.workspace_id,
            user_id,
            name: "audit-test",
            key_hash: &knot_auth::api_key::hash(secret.as_str(), &fx.state.session_key),
            scopes: &scopes,
            created_by: minter,
            expires_at: None,
            session_hash: None,
        })
        .await
        .unwrap();
    (key.id, secret.as_str().to_string())
}

/// A bot member (`users.kind = 'bot'`) of the workspace with `role`.
async fn bot_member(fx: &Fixture, email: &str, name: &str, role: WorkspaceRole) -> Uuid {
    fx.state
        .users
        .as_ref()
        .unwrap()
        .create_bot_member(fx.workspace_id, email, name, role)
        .await
        .unwrap()
        .id
}

/// The export goes out with the key, and its rows are written first.
#[track_caller]
fn assert_exported(answer: &Answer, what: &str) {
    assert_eq!(answer.status, StatusCode::OK, "{what}: {}", answer.body);
}

/// An export made with an API key is the key's user's export (main's
/// convention: audit rows name only the key's user), and the row also keeps
/// the key's id for operators. The log says only that a key was used, never
/// which. Catches an export path that drops the key, one that names the key
/// as the actor, and the key id reaching the client.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_made_with_an_api_key_names_its_user_and_flags_the_key() {
    let fx = fixture().await;
    let root = create_page(&fx, "Runbook", None).await;
    let child = create_page(&fx, "Steps", Some(root)).await;
    let (editor_key_id, editor_key) = mint_key(&fx, fx.editor.id, fx.editor.id, &["read"]).await;
    let (owner_key_id, owner_key) = mint_key(&fx, fx.owner.id, fx.owner.id, &["read"]).await;

    for format in ["docx", "odt"] {
        let uri = format!("/api/docs/{child}/export/{format}");
        assert_exported(&send_with_key(&fx, &editor_key, "POST", &uri).await, &uri);
    }
    for uri in [
        format!("/api/docs/{child}/export"),
        format!("/api/docs/{root}/export?descendants=true"),
    ] {
        assert_exported(&send_with_key(&fx, &editor_key, "GET", &uri).await, &uri);
    }
    assert_exported(
        &send_with_key(&fx, &owner_key, "GET", "/api/workspace/export").await,
        "workspace export",
    );
    // The same person's session export, for contrast.
    let answer = export_file(&fx, &fx.editor.session, child, "docx", &[]).await;
    assert_exported(&answer, "session export");

    let rows = export_rows(&fx, child).await;
    let expected_rows = [
        (fx.editor.id, Some(editor_key_id), "docx"),
        (fx.editor.id, Some(editor_key_id), "odt"),
        (fx.editor.id, Some(editor_key_id), "markdown_zip"),
        (fx.editor.id, Some(editor_key_id), "markdown_zip"),
        (fx.owner.id, Some(owner_key_id), "markdown_zip"),
        (fx.editor.id, None, "docx"),
    ];
    assert_eq!(rows.len(), expected_rows.len(), "{rows:?}");
    for ((actor_id, data), (expected_actor, expected_key_id, expected_format)) in
        rows.iter().zip(expected_rows)
    {
        assert_eq!(*actor_id, Some(expected_actor), "{data}");
        assert_eq!(data["format"], expected_format, "{data}");
        assert_eq!(
            data.get("api_key_id").and_then(Value::as_str),
            expected_key_id.map(|key_id| key_id.to_string()).as_deref(),
            "{data}"
        );
    }

    let answer = audit_log(&fx, Some(&fx.owner.session), child, "category=exports").await;
    let body = answer.ok();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), expected_rows.len(), "{body}");
    // Newest first: the session export, then the five made with a key.
    assert_eq!(
        events[0]["details"],
        json!({"format": "docx", "scope": "page", "diagram_pictures": 0}),
        "a session export has no via_api_key"
    );
    for event in &events[1..] {
        assert_eq!(event["details"]["via_api_key"], true, "{event}");
        assert_eq!(event["actor_kind"], "user", "{event}");
    }
    assert_eq!(
        events[1]["details"],
        json!({"format": "markdown_zip", "scope": "workspace", "via_api_key": true})
    );
    assert_eq!(events[1]["actor"]["name"], "Olga");
    assert_eq!(
        events[5]["details"],
        json!({"format": "docx", "scope": "page", "diagram_pictures": 0, "via_api_key": true})
    );
    assert_eq!(
        events[5]["actor"],
        json!({"key": fx.editor.id, "name": "Eddie", "former": false})
    );
    for key_id in [editor_key_id, owner_key_id] {
        assert!(
            !answer.body.contains(&key_id.to_string()),
            "a key id reached the client: {}",
            answer.body
        );
    }
    assert!(!answer.body.contains("api_key_id"), "{}", answer.body);
}

/// A bot can export and hold page access like a person; on the audit log it
/// is marked as a bot wherever it is named, and people are not. Catches the
/// mark missing from the events, the grant principal or the stats, and a
/// human marked as one.
#[tokio::test(flavor = "multi_thread")]
async fn a_bot_is_marked_as_a_bot_on_the_audit_log() {
    let fx = fixture().await;
    let page = create_page(&fx, "Status", None).await;
    let bot = bot_member(&fx, "ci@audit.test", "CI bot", WorkspaceRole::Editor).await;
    let (_, bot_key) = mint_key(&fx, bot, fx.owner.id, &["read"]).await;
    let answer = send_json(
        &fx,
        &fx.owner.session,
        "PUT",
        &format!("/api/docs/{page}/grants/user:{bot}"),
        json!({"role": "editor", "inherit": true}),
    )
    .await;
    assert!(answer.status.is_success(), "grant: {}", answer.body);
    let answer = export_file(&fx, &fx.viewer.session, page, "odt", &[]).await;
    assert_exported(&answer, "the viewer's export");
    let uri = format!("/api/docs/{page}/export/docx");
    assert_exported(&send_with_key(&fx, &bot_key, "POST", &uri).await, &uri);

    let answer = audit_log(&fx, Some(&fx.editor.session), page, "").await;
    let body = answer.ok();
    let bot_view = json!({"key": bot, "name": "CI bot", "former": false, "bot": true});
    let vera = json!({"key": fx.viewer.id, "name": "Vera", "former": false});
    let export = first_event(&body, "doc.export");
    assert_eq!(export["actor"], bot_view);
    assert_eq!(export["details"]["via_api_key"], true);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events[1]["action"], "doc.export");
    assert_eq!(events[1]["actor"], vera, "a person is not marked");
    assert_eq!(
        first_event(&body, "doc.grant")["details"]["principal"]["person"],
        bot_view
    );
    assert_eq!(first_event(&body, "doc.create")["actor"]["name"], "Olga");
    assert!(
        first_event(&body, "doc.create")["actor"]
            .get("bot")
            .is_none()
    );

    let exports = &body["stats"]["exports"];
    assert_eq!(exports["last"]["actor"], bot_view);
    assert_eq!(exports["by_person"][0]["actor"], bot_view);
    assert_eq!(exports["by_person"][1]["actor"], vera);
    assert!(!answer.body.contains('@'), "an email: {}", answer.body);
}

/// The audit log is a read: a key needs `read`, and then its user's
/// effective role on the page decides, as for a session. Catches the route
/// becoming session-only, needing a write scope, or a key (a bot's
/// included) skipping the editor check.
#[tokio::test(flavor = "multi_thread")]
async fn api_keys_read_the_audit_log_by_their_users_role_on_the_page() {
    let fx = fixture().await;
    let page = create_page(&fx, "Plans", None).await;
    let uri = format!("/api/docs/{page}/audit");

    let (_, editor_read_key) = mint_key(&fx, fx.editor.id, fx.editor.id, &["read"]).await;
    let answer = send_with_key(&fx, &editor_read_key, "GET", &uri).await;
    let body = answer.ok();
    assert_eq!(answer.cache_control.as_deref(), Some("private, no-store"));
    assert_eq!(first_event(&body, "doc.create")["actor"]["name"], "Olga");

    let editor_bot = bot_member(&fx, "agent@audit.test", "Agent", WorkspaceRole::Editor).await;
    let (_, editor_bot_key) = mint_key(&fx, editor_bot, fx.owner.id, &["read"]).await;
    send_with_key(&fx, &editor_bot_key, "GET", &uri).await.ok();

    let (_, editor_write_key) = mint_key(
        &fx,
        fx.editor.id,
        fx.editor.id,
        &["create", "update", "delete"],
    )
    .await;
    let answer = send_with_key(&fx, &editor_write_key, "GET", &uri).await;
    answer.error(StatusCode::FORBIDDEN, "api_key.scope_denied");
    assert_eq!(answer.json()["error"]["details"]["scope"], "read");

    let (_, viewer_key) = mint_key(&fx, fx.viewer.id, fx.viewer.id, &ALL_SCOPES).await;
    send_with_key(&fx, &viewer_key, "GET", &uri)
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    let viewer_bot = bot_member(&fx, "watch@audit.test", "Watcher", WorkspaceRole::Viewer).await;
    let (_, viewer_bot_key) = mint_key(&fx, viewer_bot, fx.owner.id, &ALL_SCOPES).await;
    send_with_key(&fx, &viewer_bot_key, "GET", &uri)
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
}

const ALL_SCOPES: [&str; 4] = ["read", "create", "update", "delete"];
