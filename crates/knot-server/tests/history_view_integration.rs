//! `GET /api/docs/{id}/history/entries/{eid}/view` (page history M1.4):
//! who may view a past version of a page, the response it answers (a
//! rendered ProseMirror doc in a per-request envelope), how `?changes=`
//! is read, and how each `ViewError` reaches the client. Entries are
//! built by the real builder (`Builder::build_page`) from rows the stores
//! wrote; a test that changes the views' limits or hooks swaps
//! `AppState::history_views` and builds a new router from that state.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_history::builder::{
    PASS_MAX_ROW_BYTES, PASS_MAX_ROWS, PASS_ROWS_BUDGET, PASS_WORDS_BUDGET, REPLAY_TIMEOUT,
    WORDS_TIMEOUT,
};
use knot_history::{
    BUILDER_VERSION, BuildLimits, Builder, CHECKPOINT_FLOOR_BYTES, HistoryPermits, PassResult,
    TestHooks, ViewLimits, Views, history_permits,
};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    NewUpdate, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdateSource, UpdatesStore,
    WorkspaceRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::{Doc, ReadTxn, StateVector, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

const PASSWORD: &str = "hunter22";

/// Long enough that a version's replay input is over 64 bytes.
const TYPED: &str = "The quick brown fox jumps over the lazy dog, and then once more.";

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
    app: Router,
    state: AppState,
    pool: knot_storage::Pool,
    ws_id: Uuid,
    updates: Arc<PgUpdatesStore>,
    /// Workspace owner.
    owner: Member,
    /// Workspace editor.
    editor: Member,
    /// Workspace viewer.
    viewer: Member,
    pages: AtomicUsize,
}

/// Postgres-backed state with doc rooms on a MemBus and three logged-in
/// members, one per workspace role.
async fn fixture() -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s.cookie_secure = false;

    let updates = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    s.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        Arc::new(knot_crdt::MemBus::new()),
        updates.clone() as Arc<dyn UpdatesStore>,
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
    let app = router_with_state(s.clone());
    let mut members = Vec::new();
    for (email, name, role) in [
        ("olga@view.test", "Olga", WorkspaceRole::Owner),
        ("eddie@view.test", "Eddie", WorkspaceRole::Editor),
        ("vera@view.test", "Vera", WorkspaceRole::Viewer),
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
        members.push(Member {
            id: u.id,
            session: login(&app, email).await,
        });
    }
    let [owner, editor, viewer]: [Member; 3] = members.try_into().ok().unwrap();

    Fixture {
        app,
        state: s,
        pool,
        ws_id: ws.id,
        updates,
        owner,
        editor,
        viewer,
        pages: AtomicUsize::new(0),
    }
}

async fn login(app: &Router, email: &str) -> Session {
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

/// A response: status, headers and body text.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    fn cache_control(&self) -> Option<&str> {
        self.header("cache-control")
    }

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

    /// A JSON error with `status` and `code`.
    #[track_caller]
    fn error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status, "{}", self.body);
        assert!(
            self.header("content-type")
                .is_some_and(|t| t.starts_with("application/json")),
            "{:?}",
            self.headers
        );
        assert_eq!(self.json()["error"]["code"], code, "{}", self.body);
    }

    /// A 503 `history.busy` that asks the client to retry in 5 s.
    #[track_caller]
    fn busy(&self) {
        self.error(StatusCode::SERVICE_UNAVAILABLE, "history.busy");
        assert_eq!(self.header("retry-after"), Some("5"));
    }
}

async fn answer(r: axum::response::Response) -> Answer {
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// `method uri` through `app` as `session` (anonymous when `None`), with
/// a JSON `body`.
async fn send_on(
    app: &Router,
    session: Option<&Session>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Answer {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(s) = session {
        b = b
            .header("cookie", &s.cookie)
            .header("x-csrf-token", &s.csrf);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    answer(app.clone().oneshot(req).await.unwrap()).await
}

/// [`send_on`] through the fixture's router.
async fn send(
    fx: &Fixture,
    session: Option<&Session>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Answer {
    send_on(&fx.app, session, method, uri, body).await
}

fn view_uri(doc: Uuid, eid: impl std::fmt::Display, query: &str) -> String {
    format!("/api/docs/{doc}/history/entries/{eid}/view{query}")
}

/// `GET …/history/entries/{eid}/view{query}` through `app` as `session`.
async fn view_on(
    app: &Router,
    session: Option<&Session>,
    doc: Uuid,
    eid: impl std::fmt::Display,
    query: &str,
) -> Answer {
    send_on(app, session, "GET", &view_uri(doc, eid, query), None).await
}

/// [`view_on`] through the fixture's router.
async fn view(
    fx: &Fixture,
    session: Option<&Session>,
    doc: Uuid,
    eid: impl std::fmt::Display,
    query: &str,
) -> Answer {
    view_on(&fx.app, session, doc, eid, query).await
}

/// A router over the fixture's state whose views use `permits` and the
/// default limits as `f` changes them.
fn router_with_views(
    fx: &Fixture,
    permits: HistoryPermits,
    f: impl FnOnce(&mut ViewLimits),
) -> Router {
    let mut limits = ViewLimits::default();
    f(&mut limits);
    let mut s = fx.state.clone();
    s.history_views = Some(Arc::new(Views::new(fx.pool.clone(), permits, limits)));
    router_with_state(s)
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn count(c: &Arc<AtomicUsize>) -> usize {
    c.load(Ordering::SeqCst)
}

/// Silences the panics the test hooks raise (each nextest test is a
/// process of its own).
fn quiet_hook_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if !msg.starts_with("test hook") {
            default(info);
        }
    }));
}

/// A new top-level page by the owner.
async fn page(fx: &Fixture, title: &str) -> Uuid {
    let n = fx.pages.fetch_add(1, Ordering::SeqCst);
    fx.state
        .docs
        .as_ref()
        .unwrap()
        .create(fx.ws_id, None, title, &format!("k{n:06}"), fx.owner.id)
        .await
        .unwrap()
        .id
}

async fn exec(fx: &Fixture, sql: &str, doc: Uuid) {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(doc)
        .execute(&fx.pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

// ---------------------------------------------------------------------------
// Rows and builds
// ---------------------------------------------------------------------------

/// A client typing paragraphs on its own doc.
struct Typist(Doc);

impl Typist {
    fn new(client: u64) -> Self {
        Typist(Doc::with_client_id(client))
    }

    /// Appends a paragraph holding `text`; returns the update.
    fn para(&self, text: &str) -> Vec<u8> {
        let frag = self.0.get_or_insert_xml_fragment("default");
        let before = self.0.transact().state_vector();
        {
            let mut txn = self.0.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(text));
        }
        self.0.transact().encode_state_as_update_v1(&before)
    }

    fn state(&self) -> Vec<u8> {
        self.0
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }
}

/// Appends one `edit` row per update, by `who`, now.
async fn append(fx: &Fixture, doc: Uuid, who: Uuid, updates: Vec<Vec<u8>>) {
    let rows: Vec<NewUpdate> = updates
        .into_iter()
        .map(|bytes| NewUpdate {
            by_user_id: Some(who),
            bytes,
            source: UpdateSource::Edit,
            meta: None,
        })
        .collect();
    fx.updates.append(doc, 0, &rows).await.expect("append");
}

/// Inserts a row with its own `created_at` (raw, without a rev: the
/// trigger numbers it).
async fn insert_row(
    fx: &Fixture,
    doc: Uuid,
    who: Option<Uuid>,
    bytes: &[u8],
    source: Option<UpdateSource>,
    created_at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO doc_updates (doc_id, by_user_id, update_bytes, source, meta, epoch, created_at)
         VALUES ($1, $2, $3, $4, NULL, 0, $5)",
    )
    .bind(doc)
    .bind(who)
    .bind(bytes)
    .bind(source.map(|s| s.as_str()))
    .bind(created_at)
    .execute(&fx.pool)
    .await
    .expect("insert row");
}

async fn history_since(fx: &Fixture, doc: Uuid) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT history_since FROM documents WHERE id = $1")
        .bind(doc)
        .fetch_one(&fx.pool)
        .await
        .unwrap()
}

/// The tests' build limits: no debounce, no pacing.
fn limits() -> BuildLimits {
    BuildLimits {
        max_rows: PASS_MAX_ROWS,
        max_row_bytes: PASS_MAX_ROW_BYTES,
        max_input_bytes: 8 * 1024 * 1024,
        words_max_input_bytes: 8 * 1024 * 1024,
        rows_budget: PASS_ROWS_BUDGET,
        words_budget: PASS_WORDS_BUDGET,
        lease: Duration::from_secs(5 * 60),
        replay_timeout: REPLAY_TIMEOUT,
        words_timeout: WORDS_TIMEOUT,
        checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
        debounce: Duration::ZERO,
        cpu_share: 1.0,
        compute_wait: Duration::from_secs(5 * 60),
        hooks: TestHooks::default(),
    }
}

fn builder(fx: &Fixture, limits: BuildLimits) -> Arc<Builder> {
    Arc::new(Builder::new(
        fx.pool.clone(),
        fx.updates.clone(),
        limits,
        history_permits(2),
    ))
}

/// Builds `doc` until a claim finds nothing left to do.
async fn build(fx: &Fixture, doc: Uuid) {
    let b = builder(fx, limits());
    for _ in 0..50 {
        match b.build_page(doc).await {
            PassResult::Clean => return,
            PassResult::Built { .. } | PassResult::Genesis { .. } => {}
            other => panic!("build {doc}: {other:?}"),
        }
    }
    panic!("{doc} never came clean");
}

/// The id of `doc`'s only session entry by `author`.
async fn session_of(fx: &Fixture, doc: Uuid, author: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT id FROM doc_history_entries
          WHERE doc_id = $1 AND kind = 'session' AND author_id = $2",
    )
    .bind(doc)
    .bind(author)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
}

/// A built page where each of `authors` typed a paragraph (one session
/// each, in order); returns the page and the session ids.
async fn typed_page(fx: &Fixture, authors: &[Uuid]) -> (Uuid, Vec<i64>) {
    let doc = page(fx, "Typed").await;
    for (i, who) in authors.iter().enumerate() {
        let t = Typist::new(10 + i as u64);
        append(fx, doc, *who, vec![t.para(&format!("{TYPED} ({i})"))]).await;
    }
    build(fx, doc).await;
    let mut ids = Vec::new();
    for who in authors {
        ids.push(session_of(fx, doc, *who).await);
    }
    (doc, ids)
}

/// A built page with the editor's one session; returns it and its id.
async fn edited_page(fx: &Fixture) -> (Uuid, i64) {
    let (doc, ids) = typed_page(fx, &[fx.editor.id]).await;
    (doc, ids[0])
}

/// A page created a day before its `history_since` with a baseline and a
/// legacy row on top (so the rollout entry differs from it), built;
/// returns the page, the baseline's entry id and its `baseline_id`.
async fn legacy_page(fx: &Fixture) -> (Uuid, i64, i64) {
    let doc = page(fx, "Legacy").await;
    exec(
        fx,
        "UPDATE documents SET history_since = now() - interval '1 day',
                              created_at = now() - interval '2 days'
          WHERE id = $1",
        doc,
    )
    .await;
    let since = history_since(fx, doc).await;
    let t = Typist::new(40);
    t.para("before the rollout");
    let baseline_id: i64 = sqlx::query_scalar(
        "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
         VALUES ($1, 1, $2, '\\x00', $3)
         RETURNING id",
    )
    .bind(doc)
    .bind(t.state())
    .bind(since - chrono::Duration::hours(10))
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    let legacy = t.para("legacy row");
    insert_row(
        fx,
        doc,
        None,
        &legacy,
        None,
        since - chrono::Duration::hours(1),
    )
    .await;
    build(fx, doc).await;
    let entry_id: i64 = sqlx::query_scalar(
        "SELECT id FROM doc_history_entries WHERE doc_id = $1 AND baseline_id = $2",
    )
    .bind(doc)
    .bind(baseline_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    (doc, entry_id, baseline_id)
}

// ---------------------------------------------------------------------------
// The doc
// ---------------------------------------------------------------------------

/// Every text node of a ProseMirror doc, with its marks' types.
fn texts(v: &Value) -> Vec<(String, Vec<String>)> {
    fn walk(v: &Value, out: &mut Vec<(String, Vec<String>)>) {
        if v["type"] == "text" {
            let marks = v["marks"]
                .as_array()
                .map(|m| {
                    m.iter()
                        .map(|m| m["type"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            out.push((v["text"].as_str().unwrap_or_default().to_string(), marks));
        }
        if let Some(c) = v["content"].as_array() {
            for x in c {
                walk(x, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

/// Whether some text node of `doc` carries mark `mark`.
fn has_mark(doc: &Value, mark: &str) -> bool {
    texts(doc).iter().any(|(_, m)| m.iter().any(|x| x == mark))
}

/// Whether some text node of `doc` holds `s`.
fn has_text(doc: &Value, s: &str) -> bool {
    texts(doc).iter().any(|(t, _)| t.contains(s))
}

// ---------------------------------------------------------------------------
// Access
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn editors_and_owners_view_entries_and_viewers_are_refused() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;

    for m in [&fx.owner, &fx.editor] {
        view(&fx, Some(&m.session), doc, eid, "").await.ok();
    }
    view(&fx, Some(&fx.viewer.session), doc, eid, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    view(&fx, None, doc, eid, "")
        .await
        .error(StatusCode::UNAUTHORIZED, "auth.session_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restricted_page_refuses_view_without_a_grant() {
    let fx = fixture().await;
    let (doc, ids) = typed_page(&fx, &[fx.owner.id]).await;
    let eid = ids[0];
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{doc}/restricted"),
        Some(json!({"restricted": true})),
    )
    .await;
    assert!(a.status.is_success(), "restrict: {} {}", a.status, a.body);

    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let a = view_on(&app, Some(&fx.editor.session), doc, eid, "").await;
    a.error(StatusCode::FORBIDDEN, "acl.no_grant");
    assert!(!a.body.contains("doc\":"), "{}", a.body);
    assert_eq!(count(&renders), 0);
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .ok();
    assert_eq!(count(&renders), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_key_views_an_entry() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let key = |scopes: Vec<&'static str>| {
        let fx = &fx;
        async move {
            let a = send(
                fx,
                Some(&fx.owner.session),
                "POST",
                "/api/auth/api-keys",
                Some(json!({"name": "h", "scopes": scopes})),
            )
            .await;
            assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
            a.json()["key"].as_str().unwrap().to_string()
        }
    };
    let with_key = |key: String| {
        let app = fx.app.clone();
        async move {
            answer(
                app.oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(view_uri(doc, eid, ""))
                        .header("authorization", format!("Bearer {key}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
            )
            .await
        }
    };

    let by_session = view(&fx, Some(&fx.owner.session), doc, eid, "").await.ok();
    let by_key = with_key(key(vec!["read"]).await).await.ok();
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&by_key), keys(&by_session));
    assert_eq!(by_key, by_session);

    let a = with_key(key(vec!["update"]).await).await;
    a.error(StatusCode::FORBIDDEN, "api_key.scope_denied");
    assert_eq!(a.json()["error"]["details"]["scope"], "read", "{}", a.body);
}

// ---------------------------------------------------------------------------
// The response
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_view_body_is_the_rendered_version() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;

    for m in [&fx.owner, &fx.editor] {
        let a = view(&fx, Some(&m.session), doc, eid, "").await;
        let body = a.ok();
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "can_erase",
                "can_restore",
                "changes",
                "doc",
                "entry_id",
                "kind"
            ]
        );
        assert_eq!(body["entry_id"], json!(eid));
        assert_eq!(body["kind"], "session");
        assert_eq!(body["can_restore"], json!(true));
        // Erase does not exist until M3, for owners too (Rulings).
        assert_eq!(body["can_erase"], json!(false));
        assert_eq!(body["doc"]["type"], "doc");
        assert!(has_text(&body["doc"], TYPED), "{}", body["doc"]);

        assert_eq!(a.cache_control(), Some("private, no-store"));
        assert!(
            a.header("content-type")
                .is_some_and(|t| t.starts_with("application/json")),
            "{:?}",
            a.headers
        );
        assert_eq!(a.header("etag"), None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_defaults_on_and_is_off_for_baselines() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let owner = Some(&fx.owner.session);

    let on = view(&fx, owner, doc, eid, "").await.ok();
    assert_eq!(on["changes"], json!(true));
    assert!(has_mark(&on["doc"], "historyInsert"), "{}", on["doc"]);
    let off = view(&fx, owner, doc, eid, "?changes=0").await.ok();
    assert_eq!(off["changes"], json!(false));
    assert!(!has_mark(&off["doc"], "historyInsert"), "{}", off["doc"]);
    assert!(has_text(&off["doc"], TYPED), "{}", off["doc"]);

    let (legacy, baseline, _) = legacy_page(&fx).await;
    let a = view(&fx, owner, legacy, baseline, "?changes=1").await.ok();
    assert_eq!(a["kind"], "baseline");
    assert_eq!(a["changes"], json!(false));
    assert!(has_text(&a["doc"], "before the rollout"), "{}", a["doc"]);
    assert!(!has_mark(&a["doc"], "historyInsert"), "{}", a["doc"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_changes_or_entry_id_is_a_400() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let owner = Some(&fx.owner.session);

    for query in [
        "?changes=2",
        "?changes=abc",
        "?changes=true",
        "?changes=1&changes=0",
        "?changes=1&changes=1",
    ] {
        let a = view(&fx, owner, doc, eid, query).await;
        a.error(StatusCode::BAD_REQUEST, "history.bad_request");
    }
    view(&fx, owner, doc, "abc", "")
        .await
        .error(StatusCode::BAD_REQUEST, "history.bad_request");

    // Other keys are ignored; an empty value is no value.
    let a = view(&fx, owner, doc, eid, "?changes=0&other=x").await.ok();
    assert_eq!(a["changes"], json!(false));
    let empty = view(&fx, owner, doc, eid, "?changes=").await.ok();
    let one = view(&fx, owner, doc, eid, "?changes=1").await.ok();
    assert_eq!(empty["changes"], json!(true));
    assert_eq!(empty, one);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_of_another_page_is_a_404() {
    let fx = fixture().await;
    let (a, _) = edited_page(&fx).await;
    let (_, of_b) = edited_page(&fx).await;
    view(&fx, Some(&fx.owner.session), a, of_b, "")
        .await
        .error(StatusCode::NOT_FOUND, "history.not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archived_page_views_with_can_restore_false() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "DELETE",
        &format!("/api/docs/{doc}"),
        None,
    )
    .await;
    assert!(a.status.is_success(), "archive: {} {}", a.status, a.body);
    for m in [&fx.owner, &fx.editor] {
        let body = view(&fx, Some(&m.session), doc, eid, "").await.ok();
        assert_eq!(body["can_restore"], json!(false));
        assert_eq!(body["can_erase"], json!(false));
        assert!(has_text(&body["doc"], TYPED), "{}", body["doc"]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_view_answers_the_same_body() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let first = view_on(&app, Some(&fx.owner.session), doc, eid, "").await;
    let second = view_on(&app, Some(&fx.editor.session), doc, eid, "").await;
    first.ok();
    second.ok();
    assert_eq!(first.body, second.body);
    assert_eq!(count(&renders), 1, "the second view is a cache hit");
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_view_over_the_cap_is_a_409() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let owner = Some(&fx.owner.session);

    // Over the diff cap only: the plain version.
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_changes_input_bytes = 64;
    });
    let a = view_on(&app, owner, doc, eid, "?changes=1").await.ok();
    assert_eq!(a["changes"], json!(false));
    assert!(!has_mark(&a["doc"], "historyInsert"), "{}", a["doc"]);
    assert!(has_text(&a["doc"], TYPED), "{}", a["doc"]);
    view_on(&app, owner, doc, eid, "?changes=0").await.ok();

    // Over the plain cap: no version at all.
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_changes_input_bytes = 64;
        l.max_plain_input_bytes = 64;
    });
    for query in ["?changes=1", "?changes=0"] {
        view_on(&app, owner, doc, eid, query)
            .await
            .error(StatusCode::CONFLICT, "history.too_large");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skipped_baseline_is_a_500() {
    let fx = fixture().await;
    let (doc, eid, baseline_id) = legacy_page(&fx).await;
    sqlx::query("UPDATE doc_history_state SET skip_baselines = ARRAY[$2] WHERE doc_id = $1")
        .bind(doc)
        .bind(baseline_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .error(StatusCode::INTERNAL_SERVER_ERROR, "history.view_failed");
    assert_eq!(count(&renders), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_view_is_a_503_with_retry_after() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.permit_wait = Duration::from_millis(200);
    });
    let held = fx.state.history_permits.compute().await;
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .busy();
    drop(held);
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_render_is_a_500() {
    quiet_hook_panics();
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.panic_render = true;
    });
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .error(StatusCode::INTERNAL_SERVER_ERROR, "history.view_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_diff_answers_the_plain_version() {
    quiet_hook_panics();
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.panic_in_diff = true;
    });
    let a = view_on(&app, Some(&fx.owner.session), doc, eid, "?changes=1")
        .await
        .ok();
    assert_eq!(a["changes"], json!(false));
    assert!(!has_mark(&a["doc"], "historyInsert"), "{}", a["doc"]);
    assert!(has_text(&a["doc"], TYPED), "{}", a["doc"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_an_older_builder_built_answers_409_rebuilding() {
    let fx = fixture().await;
    let (doc, eid) = edited_page(&fx).await;
    sqlx::query("UPDATE doc_history_state SET builder_version = $2 WHERE doc_id = $1")
        .bind(doc)
        .bind(BUILDER_VERSION - 1)
        .execute(&fx.pool)
        .await
        .unwrap();
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    view_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .error(StatusCode::CONFLICT, "history.rebuilding");
    assert_eq!(count(&renders), 0);

    let listed = send(
        &fx,
        Some(&fx.owner.session),
        "GET",
        &format!("/api/docs/{doc}/history/entries"),
        None,
    )
    .await
    .ok();
    let ids: Vec<i64> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&eid), "{listed}");
}

// ---------------------------------------------------------------------------
// Render slots
// ---------------------------------------------------------------------------

/// Views on their own two compute permits whose renders each take 1 s,
/// waiting at most 200 ms for a slot or a permit.
fn slow_views(fx: &Fixture) -> Router {
    router_with_views(fx, history_permits(2), |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.permit_wait = Duration::from_millis(200);
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn one_user_renders_one_version_at_a_time() {
    let fx = fixture().await;
    let (doc, ids) = typed_page(&fx, &[fx.owner.id, fx.editor.id, fx.viewer.id]).await;
    let app = slow_views(&fx);
    let editor = Some(&fx.editor.session);

    let (e0, e1, owner) = tokio::join!(
        view_on(&app, editor, doc, ids[0], ""),
        view_on(&app, editor, doc, ids[1], ""),
        view_on(&app, Some(&fx.owner.session), doc, ids[2], ""),
    );
    owner.ok();
    let (served, refused) = match (e0.status, e1.status) {
        (StatusCode::OK, _) => (e0, e1),
        _ => (e1, e0),
    };
    served.ok();
    refused.busy();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_colleague_opening_the_same_version_is_not_blocked() {
    let fx = fixture().await;
    let (doc, ids) = typed_page(&fx, &[fx.owner.id, fx.editor.id]).await;
    let (e1, e2) = (ids[0], ids[1]);
    let app = slow_views(&fx);
    let editor = Some(&fx.editor.session);

    let (first, second, colleague) = tokio::join!(
        view_on(&app, editor, doc, e1, ""),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            view_on(&app, editor, doc, e2, "").await
        },
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            view_on(&app, Some(&fx.owner.session), doc, e2, "").await
        },
    );
    first.ok();
    // The editor's own slot is held by its first render.
    second.busy();
    // The owner waits on no one else's slot.
    let body = colleague.ok();
    assert_eq!(body["entry_id"], json!(e2));
}
