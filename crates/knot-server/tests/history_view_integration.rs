//! `GET /api/docs/{id}/history/entries/{eid}/view` (page history M1.4):
//! who may view a past version of a page, the response it answers (a
//! rendered ProseMirror doc in a per-request envelope), how `?changes=`
//! is read, and how each `ViewError` reaches the client; and
//! `POST …/entries/{eid}/restore`, Restore here (M2): a verified revert
//! in place through the page's room. Entries are
//! built by the real builder (`Builder::build_page`) from rows the stores
//! wrote; a test that changes the views' limits or hooks swaps
//! `AppState::history_views` and builds a new router from that state.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
    ReplayMode, Replayer, RevertHooks, RevertTarget, TestHooks, ViewLimits, Views, history_permits,
    revert,
};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    NewUpdate, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdateSource, UpdatesStore,
    WorkspaceRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Assoc, ClientID, Doc, GetString, ID, IndexedSequence, Out, ReadTxn, StateVector,
    StickyIndex, Text, Transact, Update, Xml, XmlElementPrelim, XmlElementRef, XmlFragment,
    XmlFragmentRef, XmlOut, XmlTextPrelim,
};

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

    /// Appends typed content: a heading of level 2, a bullet list whose
    /// item is checked, a table cell 120 wide and a linked word; returns
    /// the update.
    fn rich(&self) -> Vec<u8> {
        let frag = self.0.get_or_insert_xml_fragment("default");
        let before = self.0.transact().state_vector();
        {
            let mut txn = self.0.transact_mut();
            let h = frag.push_back(&mut txn, XmlElementPrelim::empty("heading"));
            h.insert_attribute(&mut txn, "level", Any::Number(2.0));
            h.push_back(&mut txn, XmlTextPrelim::new("Heading"));
            let list = frag.push_back(&mut txn, XmlElementPrelim::empty("bullet_list"));
            let item = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            item.insert_attribute(&mut txn, "checked", Any::Bool(true));
            let p = item.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("task"));
            let table = frag.push_back(&mut txn, XmlElementPrelim::empty("table"));
            let row = table.push_back(&mut txn, XmlElementPrelim::empty("table_row"));
            let cell = row.push_back(&mut txn, XmlElementPrelim::empty("table_cell"));
            cell.insert_attribute(&mut txn, "colwidth", widths(120.0));
            let p = cell.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("cell"));
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            let site = p.push_back(&mut txn, XmlTextPrelim::new("site"));
            site.format(&mut txn, 0, 4, link("https://a.example"));
        }
        self.0.transact().encode_state_as_update_v1(&before)
    }

    /// Sets `key` to `value` on child `child` of `default`, or on its first
    /// descendant element that has `key` when the child itself has none;
    /// returns the update.
    fn set_attr(&self, child: u32, key: &str, value: Any) -> Vec<u8> {
        let frag = self.0.get_or_insert_xml_fragment("default");
        let before = self.0.transact().state_vector();
        {
            let mut txn = self.0.transact_mut();
            let Some(XmlOut::Element(top)) = frag.get(&txn, child) else {
                panic!("child {child} is not an element");
            };
            let mut stack = vec![top];
            let target = loop {
                let e = stack
                    .pop()
                    .unwrap_or_else(|| panic!("no {key} under {child}"));
                if e.get_attribute(&txn, key).is_some() {
                    break e;
                }
                for i in (0..e.len(&txn)).rev() {
                    if let Some(XmlOut::Element(c)) = e.get(&txn, i) {
                        stack.push(c);
                    }
                }
            };
            target.insert_attribute(&mut txn, key, value);
        }
        self.0.transact().encode_state_as_update_v1(&before)
    }

    /// Links the whole text of paragraph `child` of `default` to `href`;
    /// returns the update.
    fn relink(&self, child: u32, href: &str) -> Vec<u8> {
        let frag = self.0.get_or_insert_xml_fragment("default");
        let before = self.0.transact().state_vector();
        {
            let mut txn = self.0.transact_mut();
            let Some(XmlOut::Element(p)) = frag.get(&txn, child) else {
                panic!("child {child} is not an element");
            };
            let Some(XmlOut::Text(t)) = p.get(&txn, 0) else {
                panic!("child {child} holds no text");
            };
            let len = t.len(&txn);
            t.format(&mut txn, 0, len, link(href));
        }
        self.0.transact().encode_state_as_update_v1(&before)
    }
}

/// A `colwidth` value.
fn widths(w: f64) -> Any {
    Any::Array(Arc::from(vec![Any::Number(w)]))
}

/// The `link` mark to `href`.
fn link(href: &str) -> Attrs {
    let m: HashMap<String, Any> = [("href".to_string(), Any::from(href))].into();
    [("link".into(), Any::from(m))].into_iter().collect()
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
/// waiting at most 200 ms for a slot or a permit; `renders` counts the
/// render threads started.
fn slow_views(fx: &Fixture, renders: &Arc<AtomicUsize>) -> Router {
    let renders = renders.clone();
    router_with_views(fx, history_permits(2), |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.permit_wait = Duration::from_millis(200);
        l.hooks.renders = Some(renders);
    })
}

/// Waits until `renders` counts `n` render threads started.
async fn renders_started(renders: &Arc<AtomicUsize>, n: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while count(renders) < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the render to start");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_user_renders_one_version_at_a_time() {
    let fx = fixture().await;
    let (doc, ids) = typed_page(&fx, &[fx.owner.id, fx.editor.id, fx.viewer.id]).await;
    let app = slow_views(&fx, &counter());
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
    let renders = counter();
    let app = slow_views(&fx, &renders);
    let editor = Some(&fx.editor.session);

    // The editor's second request and the owner's start once the first
    // render runs, so it holds the editor's slot (not after a fixed sleep,
    // which a slow runner's auth and loads can outlast).
    let (first, second, colleague) = tokio::join!(
        view_on(&app, editor, doc, e1, ""),
        async {
            renders_started(&renders, 1).await;
            view_on(&app, editor, doc, e2, "").await
        },
        async {
            renders_started(&renders, 1).await;
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

// ---------------------------------------------------------------------------
// Restore here (M2: a verified revert in place)
// ---------------------------------------------------------------------------

fn restore_uri(doc: Uuid, eid: impl std::fmt::Display, query: &str) -> String {
    format!("/api/docs/{doc}/history/entries/{eid}/restore{query}")
}

/// `POST …/history/entries/{eid}/restore{query}` through `app` as `session`.
async fn restore_on(
    app: &Router,
    session: Option<&Session>,
    doc: Uuid,
    eid: impl std::fmt::Display,
    query: &str,
) -> Answer {
    send_on(app, session, "POST", &restore_uri(doc, eid, query), None).await
}

/// [`restore_on`] through the fixture's router.
async fn restore(
    fx: &Fixture,
    session: Option<&Session>,
    doc: Uuid,
    eid: impl std::fmt::Display,
    query: &str,
) -> Answer {
    restore_on(&fx.app, session, doc, eid, query).await
}

/// A 200 restore that wrote the version, by undo or by the whole-page
/// replace; its outcome.
#[track_caller]
fn restored(a: &Answer) -> String {
    let body = a.ok();
    let outcome = body["outcome"].as_str().unwrap_or_default().to_string();
    assert!(outcome == "restored" || outcome == "fallback", "{}", a.body);
    assert_eq!(body, json!({ "outcome": outcome }), "{}", a.body);
    assert!(
        a.header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{:?}",
        a.headers
    );
    outcome
}

/// A 200 restore whose outcome is exactly `outcome`.
#[track_caller]
fn restored_as(a: &Answer, outcome: &str) {
    assert_eq!(a.ok(), json!({ "outcome": outcome }), "{}", a.body);
}

/// The seq restore's answer: a 204 with no body.
#[track_caller]
fn seq_restored(a: &Answer) {
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    assert_eq!(a.body, "");
}

/// `knot_history_restore_total{outcome}`, from a recorder installed once
/// for this test process (each nextest test is a process of its own, and
/// the counters are emitted on the runtime's workers, which a thread-local
/// recorder misses). Tests read it before and after: deltas only.
fn restore_total(outcome: &str) -> u64 {
    restore_counts()
        .lock()
        .unwrap()
        .get(outcome)
        .map_or(0, |count| count.load(Ordering::Relaxed))
}

type RestoreCounts = Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>;

fn restore_counts() -> &'static RestoreCounts {
    static COUNTS: OnceLock<RestoreCounts> = OnceLock::new();
    COUNTS.get_or_init(|| {
        let counts = RestoreCounts::default();
        metrics::set_global_recorder(RestoreRecorder(Arc::clone(&counts)))
            .expect("no other recorder in this test process");
        counts
    })
}

struct RestoreRecorder(RestoreCounts);

impl metrics::Recorder for RestoreRecorder {
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
        if key.name() != "knot_history_restore_total" {
            return metrics::Counter::noop();
        }
        let outcome = key
            .labels()
            .find(|label| label.key() == "outcome")
            .map(|label| label.value().to_owned())
            .unwrap_or_default();
        let count = Arc::clone(self.0.lock().unwrap().entry(outcome).or_default());
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

/// The audit rows about `target`, oldest first.
async fn audit_rows(fx: &Fixture, target: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT action, data FROM audit_events WHERE target_id = $1 ORDER BY id")
        .bind(target)
        .fetch_all(&fx.pool)
        .await
        .unwrap()
}

/// One stored row of a page's log.
struct Row {
    source: Option<String>,
    by: Option<Uuid>,
    bytes: Vec<u8>,
}

/// `doc`'s rows, in rev order.
async fn rows(fx: &Fixture, doc: Uuid) -> Vec<Row> {
    let rows: Vec<(Option<String>, Option<Uuid>, Vec<u8>)> = sqlx::query_as(
        "SELECT source, by_user_id, update_bytes FROM doc_updates
          WHERE doc_id = $1 ORDER BY rev, seq",
    )
    .bind(doc)
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(source, by, bytes)| Row { source, by, bytes })
        .collect()
}

/// The page its rows store: a GC replay of them, as a room boots it.
async fn stored(fx: &Fixture, doc: Uuid) -> Replayer {
    let mut page = Replayer::new(ReplayMode::Gc);
    for row in rows(fx, doc).await {
        page.apply(&row.bytes);
    }
    page
}

/// What a verified revert must render: the stored page, rendered as
/// `RevertTarget::json` is (`revert::rendered`).
async fn stored_rendered(fx: &Fixture, doc: Uuid) -> Vec<u8> {
    revert::rendered(stored(fx, doc).await.doc()).unwrap()
}

/// Entry `eid`'s Restore here target, through the fixture's views.
async fn target_of(fx: &Fixture, doc: Uuid, eid: i64) -> RevertTarget {
    let views = fx.state.history_views.as_ref().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(27);
    match views
        .restore_target(fx.owner.id, doc, eid, None, deadline)
        .await
    {
        Ok(r) => r.value,
        Err(e) => panic!("restore_target {eid}: {e}"),
    }
}

/// Every client whose blocks `bytes` inserts, deleted ones included.
fn inserted_clients(bytes: &[u8]) -> HashSet<ClientID> {
    Update::decode_v1(bytes)
        .unwrap()
        .insertions(true)
        .client_ids()
        .collect()
}

/// The id of `needle`'s first character in the first text of `doc`'s
/// `default` that holds it.
fn char_id(doc: &Doc, needle: &str) -> ID {
    sticky_at(doc, needle)
        .id()
        .cloned()
        .expect("a relative index")
}

/// A sticky index on `needle`'s first character in the first text of
/// `doc`'s `default` that holds it.
fn sticky_at(doc: &Doc, needle: &str) -> StickyIndex {
    let frag = doc.get_or_insert_xml_fragment("default");
    let txn = doc.transact();
    let mut stack: Vec<XmlOut> = (0..frag.len(&txn))
        .rev()
        .filter_map(|i| frag.get(&txn, i))
        .collect();
    while let Some(node) = stack.pop() {
        match node {
            XmlOut::Element(e) => {
                for i in (0..e.len(&txn)).rev() {
                    if let Some(c) = e.get(&txn, i) {
                        stack.push(c);
                    }
                }
            }
            XmlOut::Text(t) => {
                if let Some(at) = t.get_string(&txn).find(needle) {
                    return t
                        .sticky_index(&txn, at as u32, Assoc::After)
                        .expect("an index inside the text");
                }
            }
            XmlOut::Fragment(_) => {}
        }
    }
    panic!("{needle:?} not in the page");
}

/// The element of `frag` (a page's `default`) at `path` (child indices).
fn element_at<T: ReadTxn>(frag: &XmlFragmentRef, txn: &T, path: &[u32]) -> XmlElementRef {
    let Some(XmlOut::Element(mut e)) = frag.get(txn, path[0]) else {
        panic!("no element at {path:?}");
    };
    for &i in &path[1..] {
        let Some(XmlOut::Element(c)) = e.get(txn, i) else {
            panic!("no element at {path:?}");
        };
        e = c;
    }
    e
}

/// The page's live content as Markdown, read by the owner.
async fn markdown(fx: &Fixture, doc: Uuid) -> String {
    let a = send(
        fx,
        Some(&fx.owner.session),
        "GET",
        &format!("/api/docs/{doc}/markdown"),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    a.body
}

/// `POST /api/docs/{doc}/markdown?mode=replace` as the owner.
async fn import_replace(fx: &Fixture, doc: Uuid, md: &str) {
    let r = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc}/markdown?mode=replace"))
                .header("cookie", &fx.owner.session.cookie)
                .header("x-csrf-token", &fx.owner.session.csrf)
                .header("content-type", "text/markdown")
                .body(Body::from(md.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let a = answer(r).await;
    assert!(a.status.is_success(), "import: {} {}", a.status, a.body);
}

/// The first page of `doc`'s history entries, as the owner reads it.
async fn entries(fx: &Fixture, doc: Uuid) -> Vec<Value> {
    let body = send(
        fx,
        Some(&fx.owner.session),
        "GET",
        &format!("/api/docs/{doc}/history/entries"),
        None,
    )
    .await
    .ok();
    body["entries"].as_array().unwrap().clone()
}

/// The listed entry `id` of `doc`.
async fn listed(fx: &Fixture, doc: Uuid, id: i64) -> Value {
    entries(fx, doc)
        .await
        .into_iter()
        .find(|e| e["id"] == json!(id))
        .unwrap_or_else(|| panic!("entry {id} not listed"))
}

/// A listed `ended_at`, as an instant.
fn instant(v: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(v.as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

/// `at` as a query value: a literal `+` would be read as a space.
fn query_value(at: &str) -> String {
    at.replace('+', "%2B")
}

/// The `meta` of `doc`'s newest restore row (`restore` or
/// `restore_fallback`), as the room stored it.
async fn stored_restore_meta(fx: &Fixture, doc: Uuid) -> Value {
    sqlx::query_scalar(
        "SELECT meta FROM doc_updates
          WHERE doc_id = $1 AND source IN ('restore', 'restore_fallback')
          ORDER BY rev DESC LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
}

/// The `source` of `doc`'s newest restore row.
async fn stored_restore_source(fx: &Fixture, doc: Uuid) -> String {
    sqlx::query_scalar(
        "SELECT source FROM doc_updates
          WHERE doc_id = $1 AND source IN ('restore', 'restore_fallback')
          ORDER BY rev DESC LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
}

/// How many of `doc`'s rows are restores.
async fn restore_rows(fx: &Fixture, doc: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM doc_updates
          WHERE doc_id = $1 AND source IN ('restore', 'restore_fallback')",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
}

/// The Markdown a full Y state reads as.
fn markdown_of_state(state: &[u8]) -> String {
    use knot_crdt::Engine;
    let engine = knot_crdt::YrsEngine;
    let doc = engine.new_doc();
    engine.apply_update(&doc, state).unwrap();
    knot_markdown::to_markdown::serialise(&engine, &doc).unwrap()
}

async fn baseline_state(fx: &Fixture, baseline_id: i64) -> Vec<u8> {
    sqlx::query_scalar("SELECT state_bytes FROM doc_history_baselines WHERE id = $1")
        .bind(baseline_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap()
}

/// A built page where the editor typed a paragraph (one session) and the
/// owner then replaced the content with `# other` by an import, which
/// seals it; returns the page and the session's id.
async fn replaced_page(fx: &Fixture) -> (Uuid, i64) {
    let doc = page(fx, "Replaced").await;
    let t = Typist::new(20);
    append(
        fx,
        doc,
        fx.editor.id,
        vec![t.para(&format!("first version: {TYPED}"))],
    )
    .await;
    import_replace(fx, doc, "# other\n").await;
    build(fx, doc).await;
    let sid = session_of(fx, doc, fx.editor.id).await;
    let md = markdown(fx, doc).await;
    assert!(
        md.contains("other") && !md.contains("first version"),
        "{md}"
    );
    (doc, sid)
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_session_brings_back_its_text() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let earlier: HashSet<ClientID> = rows(&fx, doc)
        .await
        .iter()
        .flat_map(|r| inserted_clients(&r.bytes))
        .collect();
    let audited = audit_rows(&fx, doc).await;

    restored_as(
        &restore(&fx, Some(&fx.editor.session), doc, sid, "").await,
        "restored",
    );
    let md = markdown(&fx, doc).await;
    assert!(md.contains("first version"), "{md}");
    assert!(!md.contains("other"), "{md}");
    assert_eq!(
        stored_rendered(&fx, doc).await,
        target_of(&fx, doc, sid).await.json
    );

    // One restore row, by the caller, its text under a client of its own.
    let newest = rows(&fx, doc).await.pop().unwrap();
    assert_eq!(newest.source.as_deref(), Some("restore"));
    assert_eq!(newest.by, Some(fx.editor.id));
    let clients = inserted_clients(&newest.bytes);
    assert!(!clients.is_empty());
    assert!(clients.is_disjoint(&earlier), "{clients:?} {earlier:?}");
    // A restore in place writes no audit row.
    assert_eq!(audit_rows(&fx, doc).await, audited);
}

/// A page where the editor typed "keep this", then the owner "and that"
/// (two sessions), built; returns it and the two sessions.
async fn kept_page(fx: &Fixture) -> (Uuid, i64, i64) {
    let doc = page(fx, "Kept").await;
    let t = Typist::new(50);
    append(fx, doc, fx.editor.id, vec![t.para("keep this")]).await;
    append(fx, doc, fx.owner.id, vec![t.para("and that")]).await;
    build(fx, doc).await;
    let a = session_of(fx, doc, fx.editor.id).await;
    let b = session_of(fx, doc, fx.owner.id).await;
    (doc, a, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_keeps_unchanged_text_and_its_anchors() {
    let fx = fixture().await;
    let (doc, a, _) = kept_page(&fx).await;
    let before = stored(&fx, doc).await;
    let keep = char_id(before.doc(), "keep");
    let anchor = sticky_at(before.doc(), "keep");

    restored_as(
        &restore(&fx, Some(&fx.editor.session), doc, a, "").await,
        "restored",
    );
    let after = stored(&fx, doc).await;
    assert_eq!(char_id(after.doc(), "keep"), keep);
    assert_eq!(anchor.id(), Some(&keep));
    assert!(anchor.get_offset(&after.doc().transact()).is_some());
    let md = markdown(&fx, doc).await;
    assert!(md.contains("keep this"), "{md}");
    assert!(!md.contains("and that"), "{md}");
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_the_entry_before_a_restore_undoes_it() {
    let fx = fixture().await;
    let (doc, a, b) = kept_page(&fx).await;
    let editor = Some(&fx.editor.session);
    let before = rows(&fx, doc).await;
    let clients_of = |who: Uuid| -> HashSet<ClientID> {
        before
            .iter()
            .filter(|r| r.by == Some(who))
            .flat_map(|r| inserted_clients(&r.bytes))
            .collect()
    };
    let b_inserted: Vec<Update> = before
        .iter()
        .filter(|r| r.by == Some(fx.owner.id))
        .map(|r| Update::decode_v1(&r.bytes).unwrap())
        .collect();
    let keep = char_id(stored(&fx, doc).await.doc(), "keep");

    restored_as(&restore(&fx, editor, doc, a, "").await, "restored");
    // The first restore only deletes B's paragraph: since A's version
    // nothing was deleted, so its undo has nothing to redo.
    let first = rows(&fx, doc).await.pop().unwrap();
    let first_update = Update::decode_v1(&first.bytes).unwrap();
    assert!(first_update.insertions(true).is_empty());
    let deleted = first_update.delete_set();
    assert!(!deleted.is_empty());
    for (client, ranges) in deleted.iter() {
        for range in ranges.iter() {
            for clock in range.clone() {
                let id = ID::new(*client, clock);
                assert!(
                    b_inserted.iter().any(|u| u.insertions(true).contains(&id)),
                    "{id:?} is not B's"
                );
            }
        }
    }

    build(&fx, doc).await;
    let listed = entries(&fx, doc).await;
    assert_eq!(listed[0]["kind"], "restore", "{listed:#?}");
    assert_eq!(listed[1]["id"], json!(b), "{listed:#?}");
    restored_as(&restore(&fx, editor, doc, b, "").await, "restored");
    assert_eq!(
        stored_rendered(&fx, doc).await,
        target_of(&fx, doc, b).await.json
    );
    let md = markdown(&fx, doc).await;
    assert!(md.contains("keep this") && md.contains("and that"), "{md}");

    // The second restore redoes B's paragraph under a client of its own.
    let second = rows(&fx, doc).await.pop().unwrap();
    let clients = inserted_clients(&second.bytes);
    assert_eq!(clients.len(), 1, "{clients:?}");
    let mut earlier = clients_of(fx.editor.id);
    earlier.extend(clients_of(fx.owner.id));
    earlier.extend(before.iter().flat_map(|r| inserted_clients(&r.bytes)));
    earlier.extend(inserted_clients(&first.bytes));
    assert!(clients.is_disjoint(&earlier), "{clients:?} {earlier:?}");
    // Text neither restore touched keeps its id.
    assert_eq!(char_id(stored(&fx, doc).await.doc(), "keep"), keep);
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_the_entry_before_a_fallback_restore_undoes_it() {
    let fx = fixture().await;
    let (doc, a, b) = kept_page(&fx).await;
    let editor = Some(&fx.editor.session);
    let fallback = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.revert.skip_undo = true;
    });
    restored_as(&restore_on(&fallback, editor, doc, a, "").await, "fallback");
    build(&fx, doc).await;
    let outcome = restored(&restore(&fx, editor, doc, b, "").await);
    println!("the restore of the entry before a fallback restore: {outcome}");
    assert_eq!(
        stored_rendered(&fx, doc).await,
        target_of(&fx, doc, b).await.json
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fallback_restore_is_recorded_and_listed() {
    let fx = fixture().await;
    let doc = page(&fx, "Rich").await;
    let t = Typist::new(80);
    append(&fx, doc, fx.editor.id, vec![t.rich()]).await;
    append(
        &fx,
        doc,
        fx.owner.id,
        vec![
            t.set_attr(0, "level", Any::Number(3.0)),
            t.set_attr(1, "checked", Any::Bool(false)),
            t.set_attr(2, "colwidth", widths(200.0)),
            t.relink(3, "https://b.example"),
        ],
    )
    .await;
    build(&fx, doc).await;
    let sid = session_of(&fx, doc, fx.editor.id).await;

    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.revert.skip_undo = true;
    });
    restored_as(
        &restore_on(&app, Some(&fx.editor.session), doc, sid, "").await,
        "fallback",
    );
    assert_eq!(stored_restore_source(&fx, doc).await, "restore_fallback");
    let page = stored(&fx, doc).await;
    {
        // The root first: taking it waits for every open transaction.
        let frag = page.doc().get_or_insert_xml_fragment("default");
        let txn = page.doc().transact();
        let attr = |path: &[u32], key: &str| element_at(&frag, &txn, path).get_attribute(&txn, key);
        assert_eq!(attr(&[0], "level"), Some(Out::Any(Any::Number(2.0))));
        assert_eq!(attr(&[1, 0], "checked"), Some(Out::Any(Any::Bool(true))));
        assert_eq!(attr(&[2, 0, 0], "colwidth"), Some(Out::Any(widths(120.0))));
    }
    assert_eq!(
        revert::rendered(page.doc()).unwrap(),
        target_of(&fx, doc, sid).await.json
    );

    build(&fx, doc).await;
    let newest = entries(&fx, doc).await[0].clone();
    assert_eq!(newest["kind"], "restore", "{newest}");
    assert_eq!(newest["meta"]["fallback"], json!(true), "{newest}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_nothing_verifies_writes_nothing() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let before = markdown(&fx, doc).await;
    let count = rows(&fx, doc).await.len();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.revert = RevertHooks {
            skip_undo: true,
            fail_fallback: true,
            ..Default::default()
        };
    });
    restore_on(&app, Some(&fx.editor.session), doc, sid, "")
        .await
        .error(StatusCode::INTERNAL_SERVER_ERROR, "history.restore_failed");
    assert_eq!(rows(&fx, doc).await.len(), count);
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_restores_by_one_person_run_one_at_a_time() {
    let fx = fixture().await;
    restore_total("busy");
    let mut pages = Vec::new();
    for _ in 0..3 {
        pages.push(typed_page(&fx, &[fx.editor.id, fx.owner.id]).await);
    }
    let renders = counter();
    let app = router_with_views(&fx, history_permits(2), |l| {
        l.permit_wait = Duration::from_millis(300);
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
    });
    let busy = restore_total("busy");
    let editor = Some(&fx.editor.session);
    let started = std::time::Instant::now();
    let by_editor = |(doc, ids): &(Uuid, Vec<i64>)| {
        let (doc, eid) = (*doc, ids[0]);
        let (app, renders) = (&app, &renders);
        async move {
            let a = restore_on(app, editor, doc, eid, "").await;
            (a, count(renders), started.elapsed())
        }
    };
    let (first, second, owner) = tokio::join!(
        by_editor(&pages[0]),
        by_editor(&pages[1]),
        restore_on(&app, Some(&fx.owner.session), pages[2].0, pages[2].1[0], ""),
    );
    let (served, refused, refused_page) = if first.0.status == StatusCode::OK {
        (first, second, &pages[1])
    } else {
        (second, first, &pages[0])
    };
    restored(&served.0);
    refused.0.busy();
    // Refused at its permit wait, before its replay, while the two other
    // targets were held.
    assert_eq!(refused.1, 2, "renders when the 503 arrived");
    assert!(
        refused.2 >= Duration::from_millis(250) && refused.2 < Duration::from_secs(1),
        "{:?}",
        refused.2
    );
    restored(&owner);
    assert_eq!(count(&renders), 4);
    assert_eq!(restore_total("busy"), busy);

    restored(&restore_on(&app, editor, refused_page.0, refused_page.1[0], "").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_the_current_version_changes_nothing() {
    let fx = fixture().await;
    let (doc, ids) = typed_page(&fx, &[fx.editor.id, fx.owner.id]).await;
    let count = rows(&fx, doc).await.len();
    restored_as(
        &restore(&fx, Some(&fx.editor.session), doc, ids[1], "").await,
        "unchanged",
    );
    assert_eq!(rows(&fx, doc).await.len(), count);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_too_large_to_revert_is_a_409() {
    let fx = fixture().await;
    restore_total("too_large");
    let (doc, sid) = replaced_page(&fx).await;
    let before = markdown(&fx, doc).await;
    let too_large = restore_total("too_large");
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_revert_input_bytes = 64;
    });
    restore_on(&app, Some(&fx.editor.session), doc, sid, "")
        .await
        .error(StatusCode::CONFLICT, "restore.too_large");
    assert_eq!(markdown(&fx, doc).await, before);
    assert_eq!(restore_total("too_large"), too_large + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_over_the_revert_cap_is_refused_before_any_export() {
    let fx = fixture().await;
    restore_total("too_large");
    let rooms = fx.state.rooms_v2.as_ref().unwrap();
    let editor = Some(&fx.editor.session);

    // The version alone over the cap: no acquire, so no export.
    let (doc, ids) = typed_page(&fx, &[fx.editor.id, fx.owner.id]).await;
    let target = target_of(&fx, doc, ids[0]).await.state.len();
    assert!(rooms.get_active(doc).is_none());
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_revert_input_bytes = target as i64 - 1;
        l.hooks.renders = Some(renders.clone());
    });
    restore_on(&app, editor, doc, ids[0], "")
        .await
        .error(StatusCode::CONFLICT, "restore.too_large");
    assert!(rooms.get_active(doc).is_none());
    assert_eq!(count(&renders), 1, "the target only");

    // The version with the live page over the cap: one export.
    let (doc2, ids2) = typed_page(&fx, &[fx.editor.id, fx.owner.id]).await;
    let target2 = target_of(&fx, doc2, ids2[0]).await.state.len();
    let live2 = stored(&fx, doc2).await.encode_state().len();
    assert!(rooms.get_active(doc2).is_none());
    let renders2 = counter();
    let app2 = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_revert_input_bytes = (target2 + live2 / 2) as i64;
        l.hooks.renders = Some(renders2.clone());
    });
    let too_large = restore_total("too_large");
    restore_on(&app2, editor, doc2, ids2[0], "")
        .await
        .error(StatusCode::CONFLICT, "restore.too_large");
    assert!(rooms.get_active(doc2).is_some());
    assert_eq!(count(&renders2), 1, "the target only");
    assert_eq!(restore_total("too_large"), too_large + 1);

    // Again on each: refused before the replay (the over-cap record).
    restore_on(&app, editor, doc, ids[0], "")
        .await
        .error(StatusCode::CONFLICT, "restore.too_large");
    assert_eq!(count(&renders), 1);
    let too_large = restore_total("too_large");
    restore_on(&app2, editor, doc2, ids2[0], "")
        .await
        .error(StatusCode::CONFLICT, "restore.too_large");
    assert_eq!(count(&renders2), 1);
    assert_eq!(restore_total("too_large"), too_large + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_is_listed_as_restored_to_its_target() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let session = listed(&fx, doc, sid).await;
    let ended_at = instant(&session["ended_at"]);
    let end_rev: i64 = sqlx::query_scalar("SELECT end_rev FROM doc_history_entries WHERE id = $1")
        .bind(sid)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    restored(&restore(&fx, Some(&fx.editor.session), doc, sid, "").await);
    build(&fx, doc).await;
    let newest = entries(&fx, doc).await[0].clone();
    assert_eq!(newest["kind"], "restore", "{newest}");
    assert_eq!(newest["meta"]["target_entry_id"], json!(sid), "{newest}");
    assert_eq!(instant(&newest["meta"]["target_ended_at"]), ended_at);
    // The list sends no target_rev; the stored row has it.
    let stored = stored_restore_meta(&fx, doc).await;
    assert_eq!(stored["target_rev"], json!(end_rev), "{stored}");
    assert_eq!(stored["target_entry_id"], json!(sid), "{stored}");

    // The same instant with another offset spelling, percent-encoded, on
    // a page that differs from the version again.
    import_replace(&fx, doc, "# changed again\n").await;
    assert_eq!(restore_rows(&fx, doc).await, 1);
    let plus = ended_at.to_rfc3339();
    assert!(plus.ends_with("+00:00"), "{plus}");
    let a = restore(
        &fx,
        Some(&fx.editor.session),
        doc,
        sid,
        &format!("?ended_at={}", query_value(&plus)),
    )
    .await;
    restored(&a);
    assert_eq!(restore_rows(&fx, doc).await, 2);
    let stored = stored_restore_meta(&fx, doc).await;
    assert_eq!(instant(&stored["target_ended_at"]), ended_at, "{stored}");

    // A literal "+" reaches the handler as a space.
    restore(
        &fx,
        Some(&fx.editor.session),
        doc,
        sid,
        &format!("?ended_at={plus}"),
    )
    .await
    .error(StatusCode::BAD_REQUEST, "history.bad_request");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewers_cannot_restore_and_archived_pages_refuse() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    restore(&fx, Some(&fx.viewer.session), doc, sid, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    restore(&fx, None, doc, sid, "")
        .await
        .error(StatusCode::UNAUTHORIZED, "auth.session_required");

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
        restore(&fx, Some(&m.session), doc, sid, "")
            .await
            .error(StatusCode::CONFLICT, "doc.archived");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_baseline_entry_works() {
    let fx = fixture().await;
    let (doc, eid, baseline_id) = legacy_page(&fx).await;
    let expected = markdown_of_state(&baseline_state(&fx, baseline_id).await);
    assert!(expected.contains("before the rollout"), "{expected}");
    // The live rows lack the baseline's own content (the fixture's legacy
    // row builds on it), so the page reads otherwise before the restore.
    assert_ne!(markdown(&fx, doc).await, expected);

    // The live room holds no item of the baseline's client (its legacy row
    // waits, pending, for the baseline's paragraph, and the export leaves
    // pending content out), so the undo captures nothing: the whole-page
    // replace restores it.
    restored_as(
        &restore(&fx, Some(&fx.editor.session), doc, eid, "").await,
        "fallback",
    );
    assert_eq!(stored_restore_source(&fx, doc).await, "restore_fallback");
    assert_eq!(markdown(&fx, doc).await, expected);
    let stored = stored_restore_meta(&fx, doc).await;
    assert_eq!(stored["target_rev"], Value::Null, "{stored}");
    assert_eq!(stored["target_entry_id"], json!(eid), "{stored}");

    // The rollout entry: the page as the rollout found it, at genesis.
    let (rollout, genesis_rev): (i64, i64) = sqlx::query_as(
        "SELECT e.id, s.genesis_rev FROM doc_history_entries e
           JOIN doc_history_state s ON s.doc_id = e.doc_id
          WHERE e.doc_id = $1 AND e.kind = 'baseline' AND e.baseline_id IS NULL",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    // The live page holds only the first restore's copy, under its own
    // client, which an undo cannot turn into the baseline client's text.
    restored_as(
        &restore(&fx, Some(&fx.editor.session), doc, rollout, "").await,
        "fallback",
    );
    assert_eq!(stored_restore_source(&fx, doc).await, "restore_fallback");
    let stored = stored_restore_meta(&fx, doc).await;
    assert_eq!(stored["target_rev"], json!(genesis_rev), "{stored}");
    let md = markdown(&fx, doc).await;
    assert!(
        md.contains("before the rollout") && md.contains("legacy row"),
        "{md}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_over_the_cap_is_a_409() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let before = markdown(&fx, doc).await;
    let editor = Some(&fx.editor.session);

    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_plain_input_bytes = 64;
    });
    restore_on(&app, editor, doc, sid, "")
        .await
        .error(StatusCode::CONFLICT, "history.too_large");
    assert_eq!(markdown(&fx, doc).await, before);

    // A restore uses the plain cap, never the diff cap.
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.max_changes_input_bytes = 64;
    });
    restored(&restore_on(&app, editor, doc, sid, "").await);
    assert!(markdown(&fx, doc).await.contains("first version"));
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_skipped_baseline_is_a_500() {
    let fx = fixture().await;
    let (doc, eid, baseline_id) = legacy_page(&fx).await;
    sqlx::query("UPDATE doc_history_state SET skip_baselines = ARRAY[$2] WHERE doc_id = $1")
        .bind(doc)
        .bind(baseline_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let before = markdown(&fx, doc).await;
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    restore_on(&app, Some(&fx.owner.session), doc, eid, "")
        .await
        .error(StatusCode::INTERNAL_SERVER_ERROR, "history.view_failed");
    assert_eq!(count(&renders), 0);
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_restore_is_a_503() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let before = markdown(&fx, doc).await;
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.permit_wait = Duration::from_millis(200);
    });
    let held = fx.state.history_permits.compute().await;
    restore_on(&app, Some(&fx.editor.session), doc, sid, "")
        .await
        .busy();
    drop(held);
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restricted_page_refuses_restore_without_a_grant() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{doc}/restricted"),
        Some(json!({"restricted": true})),
    )
    .await;
    assert!(a.status.is_success(), "restrict: {} {}", a.status, a.body);
    let before = markdown(&fx, doc).await;

    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    restore_on(&app, Some(&fx.editor.session), doc, sid, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.no_grant");
    assert_eq!(count(&renders), 0);
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_an_entry_of_another_page_is_a_404() {
    let fx = fixture().await;
    let (a, _) = replaced_page(&fx).await;
    let (_, of_b) = edited_page(&fx).await;
    let before = markdown(&fx, a).await;
    restore(&fx, Some(&fx.owner.session), a, of_b, "")
        .await
        .error(StatusCode::NOT_FOUND, "history.not_found");
    assert_eq!(markdown(&fx, a).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_of_a_moved_entry_is_a_409() {
    let fx = fixture().await;
    let editor = Some(&fx.editor.session);

    // The version the reader saw is the entry's: restored.
    let (doc, sid) = replaced_page(&fx).await;
    let seen = listed(&fx, doc, sid).await["ended_at"]
        .as_str()
        .unwrap()
        .to_string();
    let a = restore(
        &fx,
        editor,
        doc,
        sid,
        &format!("?ended_at={}", query_value(&seen)),
    )
    .await;
    restored(&a);
    assert!(markdown(&fx, doc).await.contains("first version"));

    // An open session that grew after the reader saw it.
    let doc = page(&fx, "Growing").await;
    let t = Typist::new(30);
    append(&fx, doc, fx.editor.id, vec![t.para("one")]).await;
    build(&fx, doc).await;
    let sid = session_of(&fx, doc, fx.editor.id).await;
    let open = listed(&fx, doc, sid).await;
    assert_eq!(open["open"], json!(true), "{open}");
    let t1 = open["ended_at"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(20)).await;
    append(&fx, doc, fx.editor.id, vec![t.para("two")]).await;
    build(&fx, doc).await;
    let grown = listed(&fx, doc, sid).await;
    assert_ne!(instant(&grown["ended_at"]), instant(&json!(t1)), "{grown}");

    let before = markdown(&fx, doc).await;
    let t1q = query_value(&t1);
    restore(&fx, editor, doc, sid, &format!("?ended_at={t1q}"))
        .await
        .error(StatusCode::CONFLICT, "history.moved");
    for query in [
        "?ended_at=garbage".to_string(),
        "?ended_at=".to_string(),
        format!("?ended_at={t1q}&ended_at={t1q}"),
    ] {
        restore(&fx, editor, doc, sid, &query)
            .await
            .error(StatusCode::BAD_REQUEST, "history.bad_request");
    }
    restore(&fx, editor, doc, "abc", "")
        .await
        .error(StatusCode::BAD_REQUEST, "history.bad_request");
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_given_up_at_genesis_restores_only_its_baselines() {
    let fx = fixture().await;
    let tiny = || {
        builder(
            &fx,
            BuildLimits {
                max_input_bytes: 1,
                ..limits()
            },
        )
    };
    let given_up_at_genesis = |doc: Uuid| {
        let fx = &fx;
        async move {
            let ok: bool = sqlx::query_scalar(
                "SELECT retry_after = 'infinity' AND genesis_rev IS NULL AND builder_version = $2
                   FROM doc_history_state WHERE doc_id = $1",
            )
            .bind(doc)
            .bind(BUILDER_VERSION)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
            assert!(ok, "{doc} is not given up at genesis");
        }
    };

    // A legacy page with one baseline and later typing.
    let doc = page(&fx, "Given up").await;
    exec(
        &fx,
        "UPDATE documents SET history_since = now() - interval '1 day',
                              created_at = now() - interval '2 days'
          WHERE id = $1",
        doc,
    )
    .await;
    let since = history_since(&fx, doc).await;
    let t = Typist::new(60);
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
    append(&fx, doc, fx.editor.id, vec![t.para("later typing")]).await;
    assert_eq!(tiny().build_page(doc).await, PassResult::Failed);
    given_up_at_genesis(doc).await;

    let listed = entries(&fx, doc).await;
    assert_eq!(listed.len(), 1, "{listed:#?}");
    assert_eq!(listed[0]["kind"], "baseline");
    let eid = listed[0]["id"].as_i64().unwrap();
    let baseline_entry: i64 = sqlx::query_scalar(
        "SELECT id FROM doc_history_entries WHERE doc_id = $1 AND baseline_id = $2",
    )
    .bind(doc)
    .bind(baseline_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(eid, baseline_entry);
    let v = view(&fx, Some(&fx.owner.session), doc, eid, "").await.ok();
    assert!(has_text(&v["doc"], "before the rollout"), "{}", v["doc"]);
    assert!(!has_text(&v["doc"], "later typing"), "{}", v["doc"]);
    let expected = markdown_of_state(&baseline_state(&fx, baseline_id).await);
    assert_ne!(markdown(&fx, doc).await, expected);
    restored(&restore(&fx, Some(&fx.editor.session), doc, eid, "").await);
    assert_eq!(markdown(&fx, doc).await, expected);

    // A page without a baseline, given up after its empty genesis: no
    // entry; the seq restore is the operator's workaround.
    let doc = page(&fx, "No entries").await;
    let t = Typist::new(70);
    append(&fx, doc, fx.editor.id, vec![t.para("one")]).await;
    let snapshot = t.state();
    let mut failed = false;
    for _ in 0..5 {
        match tiny().build_page(doc).await {
            PassResult::Failed => {
                failed = true;
                break;
            }
            PassResult::Genesis { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(failed, "never given up");
    let given_up: bool = sqlx::query_scalar(
        "SELECT retry_after = 'infinity' FROM doc_history_state WHERE doc_id = $1",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(given_up);
    assert_eq!(entries(&fx, doc).await, Vec::<Value>::new());

    let sv = {
        use yrs::updates::decoder::Decode;
        use yrs::updates::encoder::Encode;
        let update = yrs::Update::decode_v1(&snapshot).unwrap();
        update.state_vector().encode_v1()
    };
    PgSnapshotStore::new(fx.pool.clone())
        .insert(doc, 1, &snapshot, &sv)
        .await
        .unwrap();
    append(&fx, doc, fx.editor.id, vec![t.para("two")]).await;
    let a = send(
        &fx,
        Some(&fx.editor.session),
        "POST",
        &format!("/api/docs/{doc}/history/1/restore"),
        None,
    )
    .await;
    seq_restored(&a);
    let md = markdown(&fx, doc).await;
    assert!(md.contains("one") && !md.contains("two"), "{md}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_of_a_page_an_older_builder_built_is_a_409() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    sqlx::query("UPDATE doc_history_state SET builder_version = $2 WHERE doc_id = $1")
        .bind(doc)
        .bind(BUILDER_VERSION - 1)
        .execute(&fx.pool)
        .await
        .unwrap();
    let before = markdown(&fx, doc).await;
    let renders = counter();
    let app = router_with_views(&fx, fx.state.history_permits.clone(), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    restore_on(&app, Some(&fx.owner.session), doc, sid, "")
        .await
        .error(StatusCode::CONFLICT, "history.rebuilding");
    assert_eq!(count(&renders), 0);
    assert_eq!(markdown(&fx, doc).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_needs_the_update_scope() {
    let fx = fixture().await;
    let (doc, sid) = replaced_page(&fx).await;
    let before = markdown(&fx, doc).await;
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
    // No cookie and no CSRF header: a key alone.
    let with_key = |key: String| {
        let app = fx.app.clone();
        async move {
            answer(
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(restore_uri(doc, sid, ""))
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

    let a = with_key(key(vec!["read"]).await).await;
    a.error(StatusCode::FORBIDDEN, "api_key.scope_denied");
    assert_eq!(
        a.json()["error"]["details"]["scope"],
        "update",
        "{}",
        a.body
    );
    assert_eq!(markdown(&fx, doc).await, before);

    restored(&with_key(key(vec!["update"]).await).await);
    assert!(markdown(&fx, doc).await.contains("first version"));
    build(&fx, doc).await;
    let author: Option<Uuid> = sqlx::query_scalar(
        "SELECT author_id FROM doc_history_entries
          WHERE doc_id = $1 AND kind = 'restore' ORDER BY id DESC LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(author, Some(fx.owner.id));
}
