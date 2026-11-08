//! `GET /api/docs/{id}/history/entries` (page history M1.3): who may list a
//! page's history, its pages newest first across the log and baseline
//! sections, the flags that say a list is not final yet, and what each
//! entry's `meta` shows. Entries are built by the real builder
//! (`Builder::build_page`) from rows the stores or the real routes wrote.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_history::builder::{
    PASS_MAX_ROW_BYTES, PASS_MAX_ROWS, PASS_ROWS_BUDGET, PASS_WORDS_BUDGET, REPLAY_TIMEOUT,
    WORDS_TIMEOUT,
};
use knot_history::{
    BuildLimits, Builder, CHECKPOINT_FLOOR_BYTES, PassResult, TestHooks, history_permits,
};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    NewUpdate, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdateSource, UpdatesStore,
    WorkspaceRole,
};
use serde_json::{Value, json};
use tokio::time::Instant;
use tower::ServiceExt;
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::updates::encoder::Encode;
use yrs::{
    Assoc, Doc, ID, ReadTxn, StateVector, StickyIndex, Transact, XmlElementPrelim, XmlFragment,
    XmlTextPrelim,
};

const PASSWORD: &str = "hunter22";
/// Every wait polls toward this and returns as soon as its condition holds.
const DEADLINE: Duration = Duration::from_secs(10);

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
    name: &'static str,
    session: Session,
}

struct Fixture {
    app: axum::Router,
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
        ("olga@entries.test", "Olga", WorkspaceRole::Owner),
        ("eddie@entries.test", "Eddie", WorkspaceRole::Editor),
        ("vera@entries.test", "Vera", WorkspaceRole::Viewer),
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
            name,
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

async fn login(app: &axum::Router, email: &str) -> Session {
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

/// A response: status, `Cache-Control` and body text.
struct Answer {
    status: StatusCode,
    cache_control: Option<String>,
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

/// `method uri` as `session` (anonymous when `None`), with a JSON `body`.
async fn send(
    fx: &Fixture,
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
    let r = fx.app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let cache_control = r
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        cache_control,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// `GET /api/docs/{doc}/history/entries?{query}` as `session`.
async fn entries(fx: &Fixture, session: Option<&Session>, doc: Uuid, query: &str) -> Answer {
    send(
        fx,
        session,
        "GET",
        &format!("/api/docs/{doc}/history/entries?{query}"),
        None,
    )
    .await
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

/// `POST /api/docs/{doc}/markdown` (append) as the owner.
async fn import(fx: &Fixture, doc: Uuid, md: &str) {
    let r = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc}/markdown"))
                .header("cookie", &fx.owner.session.cookie)
                .header("x-csrf-token", &fx.owner.session.csrf)
                .header("content-type", "text/markdown")
                .body(Body::from(md.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "import into {doc}");
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

fn kinds(listed: &Value) -> Vec<String> {
    listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

fn ids(listed: &Value) -> Vec<i64> {
    listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect()
}

/// The ids of `doc`'s stored entries matching `sql_order` (a filter and an
/// order), as the list must show them.
async fn stored(fx: &Fixture, doc: Uuid, sql_order: &str) -> Vec<i64> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM doc_history_entries WHERE doc_id = $1 {sql_order}"
    )))
    .bind(doc)
    .fetch_all(&fx.pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Access
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn editors_list_entries_and_viewers_are_refused() {
    let fx = fixture().await;
    let doc = page(&fx, "Open").await;
    append(&fx, doc, fx.editor.id, vec![Typist::new(1).para("typed")]).await;
    build(&fx, doc).await;

    for m in [&fx.editor, &fx.owner] {
        let a = entries(&fx, Some(&m.session), doc, "").await;
        let listed = a.ok();
        assert_eq!(a.cache_control.as_deref(), Some("private, no-store"));
        assert_eq!(kinds(&listed), ["session"]);
        assert_eq!(
            listed["entries"][0]["author"],
            json!({"key": fx.editor.id, "name": "Eddie", "former": false})
        );
    }
    entries(&fx, Some(&fx.viewer.session), doc, "")
        .await
        .error(StatusCode::FORBIDDEN, "acl.editor_required");
    entries(&fx, None, doc, "")
        .await
        .error(StatusCode::UNAUTHORIZED, "auth.session_required");

    // Archived: read-only for editors and owners, gone for viewers.
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "DELETE",
        &format!("/api/docs/{doc}"),
        None,
    )
    .await;
    assert!(a.status.is_success(), "archive: {} {}", a.status, a.body);
    for m in [&fx.editor, &fx.owner] {
        assert_eq!(
            kinds(&entries(&fx, Some(&m.session), doc, "").await.ok()),
            ["session"]
        );
    }
    entries(&fx, Some(&fx.viewer.session), doc, "")
        .await
        .error(StatusCode::NOT_FOUND, "doc.not_found");

    // A restricted page the editor has no grant on.
    let restricted = page(&fx, "Restricted").await;
    append(
        &fx,
        restricted,
        fx.owner.id,
        vec![Typist::new(2).para("private")],
    )
    .await;
    build(&fx, restricted).await;
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{restricted}/restricted"),
        Some(json!({"restricted": true})),
    )
    .await;
    assert!(a.status.is_success(), "restrict: {} {}", a.status, a.body);
    assert_eq!(
        kinds(
            &entries(&fx, Some(&fx.owner.session), restricted, "")
                .await
                .ok()
        ),
        ["session"]
    );
    let a = entries(&fx, Some(&fx.editor.session), restricted, "").await;
    a.error(StatusCode::FORBIDDEN, "acl.no_grant");
    assert!(!a.body.contains("entries"), "{}", a.body);
    assert!(!a.body.contains(&fx.owner.id.to_string()), "{}", a.body);
    assert!(!a.body.contains(fx.owner.name), "{}", a.body);
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

/// A page created a day before its `history_since` (which is a day ago),
/// with two baselines, a legacy row (so genesis differs from the newest
/// baseline: a rollout entry) and `sessions` sessions by the editor, an
/// hour apart after `history_since`.
async fn page_with_both_sections(fx: &Fixture, sessions: usize) -> Uuid {
    let doc = page(fx, "Sections").await;
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
    t.para("older");
    let older = t.state();
    t.para("newer");
    let newer = t.state();
    for (seq, state, hours) in [(1_i64, &older, 10), (2, &newer, 9)] {
        sqlx::query(
            "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
             VALUES ($1, $2, $3, '\\x00', $4)",
        )
        .bind(doc)
        .bind(seq)
        .bind(state)
        .bind(since - chrono::Duration::hours(hours))
        .execute(&fx.pool)
        .await
        .unwrap();
    }
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
    for i in 1..=sessions {
        let bytes = t.para(&format!("session {i}"));
        insert_row(
            fx,
            doc,
            Some(fx.editor.id),
            &bytes,
            Some(UpdateSource::Edit),
            since + chrono::Duration::hours(i as i64),
        )
        .await;
    }
    build(fx, doc).await;
    doc
}

/// Lists `doc` page by page with `limit`, as the editor.
async fn walk(fx: &Fixture, doc: Uuid, limit: i64) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut query = format!("limit={limit}");
    loop {
        let listed = entries(fx, Some(&fx.editor.session), doc, &query)
            .await
            .ok();
        let next = listed["next_cursor"].clone();
        pages.push(listed);
        match next {
            Value::Null => return pages,
            Value::String(c) => query = format!("limit={limit}&cursor={c}"),
            other => panic!("next_cursor {other}"),
        }
        assert!(pages.len() < 10, "the walk never ended");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn entries_page_newest_first_across_both_sections() {
    let fx = fixture().await;

    let doc = page_with_both_sections(&fx, 3).await;
    // Newest first: sessions by end_rev, then rollout and baselines by
    // ended_at.
    let log = stored(
        &fx,
        doc,
        "AND kind <> 'baseline' ORDER BY end_rev DESC, id DESC",
    )
    .await;
    let earlier = stored(&fx, doc, "AND kind = 'baseline' ORDER BY ended_at DESC").await;
    assert_eq!((log.len(), earlier.len()), (3, 3));
    let pages = walk(&fx, doc, 2).await;
    assert_eq!(pages.len(), 3);
    assert_eq!(ids(&pages[0]), log[..2]);
    assert_eq!(ids(&pages[1]), [log[2], earlier[0]]);
    assert_eq!(ids(&pages[2]), earlier[1..]);
    assert_eq!(kinds(&pages[1]), ["session", "baseline"]);
    assert_eq!(pages[1]["entries"][1]["meta"], json!({"rollout": true}));
    assert_eq!(pages[1]["entries"][1]["author"], Value::Null);
    for e in pages[2]["entries"].as_array().unwrap() {
        assert_eq!(e["meta"], json!({"rollout": false}));
    }
    for p in &pages {
        assert_eq!(p["history_complete"], json!(false));
        assert_eq!(p["catching_up"], json!(false));
        assert_eq!(p["unavailable"], json!(false));
    }
    let since = history_since(&fx, doc).await;
    let shown: DateTime<Utc> = pages[0]["history_since"].as_str().unwrap().parse().unwrap();
    assert_eq!(shown, since);
    // The sealed sessions carry their word counts.
    let s3 = &pages[0]["entries"][0];
    assert_eq!(
        (&s3["open"], &s3["words_added"], &s3["words_removed"]),
        (&json!(false), &json!(2), &json!(0))
    );
    // One big page holds the same entries in the same order.
    let all = entries(&fx, Some(&fx.editor.session), doc, "").await.ok();
    assert_eq!(ids(&all), [log.clone(), earlier.clone()].concat());
    assert_eq!(all["next_cursor"], Value::Null);

    // The log section ends exactly at the page boundary: the next page
    // starts the baseline section.
    let twin = page_with_both_sections(&fx, 2).await;
    let log = stored(
        &fx,
        twin,
        "AND kind <> 'baseline' ORDER BY end_rev DESC, id DESC",
    )
    .await;
    let earlier = stored(&fx, twin, "AND kind = 'baseline' ORDER BY ended_at DESC").await;
    let pages = walk(&fx, twin, 2).await;
    assert_eq!(pages.len(), 3);
    assert_eq!(ids(&pages[0]), log);
    assert_eq!(
        pages[0]["next_cursor"],
        json!(URL_SAFE_NO_PAD.encode("B.start"))
    );
    assert_eq!(ids(&pages[1]), earlier[..2]);
    assert_eq!(ids(&pages[2]), earlier[2..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_limit_is_a_400() {
    let fx = fixture().await;
    let doc = page(&fx, "Limits").await;
    for limit in ["abc", "0", "201", "-1", "1.5"] {
        entries(
            &fx,
            Some(&fx.editor.session),
            doc,
            &format!("limit={limit}"),
        )
        .await
        .error(StatusCode::BAD_REQUEST, "history.bad_request");
    }
    for query in ["limit=1", "limit=200", "", "limit=&cursor="] {
        entries(&fx, Some(&fx.editor.session), doc, query)
            .await
            .ok();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_cursor_is_a_400() {
    let fx = fixture().await;
    let doc = page(&fx, "Cursors").await;
    let mut cursors = vec!["!!".to_string()];
    for plain in ["Q.1.2", "L.1", "B.start.1", "B.9223372036854775807.1"] {
        cursors.push(URL_SAFE_NO_PAD.encode(plain));
    }
    for c in cursors {
        entries(&fx, Some(&fx.editor.session), doc, &format!("cursor={c}"))
            .await
            .error(StatusCode::BAD_REQUEST, "history.bad_request");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_session_is_listed_in_progress_without_counts() {
    let fx = fixture().await;
    let doc = page(&fx, "Open session").await;
    exec(
        &fx,
        "UPDATE documents SET history_since = now() - interval '1 day',
                              created_at = now() - interval '1 day'
          WHERE id = $1",
        doc,
    )
    .await;
    let since = history_since(&fx, doc).await;
    let t = Typist::new(5);
    insert_row(
        &fx,
        doc,
        Some(fx.owner.id),
        &t.para("one two three"),
        Some(UpdateSource::Edit),
        since + chrono::Duration::hours(1),
    )
    .await;
    append(&fx, doc, fx.editor.id, vec![t.para("still typing")]).await;
    build(&fx, doc).await;

    let listed = entries(&fx, Some(&fx.owner.session), doc, "").await.ok();
    assert_eq!(listed["history_complete"], json!(true));
    let e = listed["entries"].as_array().unwrap();
    assert_eq!(e.len(), 2, "{listed}");
    assert_eq!(e[0]["author"]["key"], json!(fx.editor.id));
    assert_eq!(e[0]["open"], json!(true));
    assert_eq!(e[0]["words_added"], Value::Null);
    assert_eq!(e[0]["words_removed"], Value::Null);
    assert_eq!(e[1]["author"]["key"], json!(fx.owner.id));
    assert_eq!(e[1]["open"], json!(false));
    assert_eq!(e[1]["words_added"], json!(3));
    assert_eq!(e[1]["words_removed"], json!(0));
}

// ---------------------------------------------------------------------------
// Catching up
// ---------------------------------------------------------------------------

/// `(catching_up, unavailable, entry count)` of `doc`'s list.
async fn flags(fx: &Fixture, doc: Uuid) -> (bool, bool, usize) {
    let listed = entries(fx, Some(&fx.owner.session), doc, "").await.ok();
    (
        listed["catching_up"].as_bool().unwrap(),
        listed["unavailable"].as_bool().unwrap(),
        listed["entries"].as_array().unwrap().len(),
    )
}

fn paragraphs(client: u64, n: usize) -> Vec<Vec<u8>> {
    let t = Typist::new(client);
    (0..n).map(|i| t.para(&format!("r{i}"))).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn catching_up_is_set_while_the_builder_lags() {
    let fx = fixture().await;

    // Never built, far behind.
    let busy = page(&fx, "Busy").await;
    append(&fx, busy, fx.editor.id, paragraphs(1, 600)).await;
    assert_eq!(flags(&fx, busy).await, (true, false, 0));

    // Never built, a few rows: no state row, heads dirty.
    let few = page(&fx, "Few").await;
    append(&fx, few, fx.editor.id, paragraphs(2, 3)).await;
    assert_eq!(flags(&fx, few).await, (true, false, 0));
    build(&fx, few).await;
    assert_eq!(flags(&fx, few).await, (false, false, 1));

    // Never edited: neither a heads nor a state row.
    let untouched = page(&fx, "Untouched").await;
    assert_eq!(flags(&fx, untouched).await, (false, false, 0));

    // A rebuild: genesis not built yet while it computes.
    exec(
        &fx,
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        few,
    )
    .await;
    let mut held = limits();
    held.hooks.hold_compute = Duration::from_secs(2);
    held.hooks.hold_compute_doc = Some(few);
    let b = builder(&fx, held);
    let rebuild = tokio::spawn(async move { b.build_page(few).await });
    let deadline = Instant::now() + DEADLINE;
    loop {
        let reset: bool = sqlx::query_scalar(
            "SELECT genesis_rev IS NULL FROM doc_history_state WHERE doc_id = $1",
        )
        .bind(few)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        if reset {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the rebuild never reset the page"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(flags(&fx, few).await.0, "a rebuild is catching up");
    assert!(
        matches!(rebuild.await.unwrap(), PassResult::Genesis { .. }),
        "the rebuild writes genesis"
    );
    assert!(!flags(&fx, few).await.0, "genesis is built again");

    // The 500-rev rule alone: genesis built, one pass to the head, then
    // 501 more rows.
    let lagging = page(&fx, "Lagging").await;
    let t = Typist::new(3);
    append(&fx, lagging, fx.editor.id, vec![t.para("first")]).await;
    build(&fx, lagging).await;
    assert_eq!(flags(&fx, lagging).await, (false, false, 1));
    let more: Vec<Vec<u8>> = (0..501).map(|i| t.para(&format!("m{i}"))).collect();
    append(&fx, lagging, fx.editor.id, more).await;
    assert_eq!(flags(&fx, lagging).await, (true, false, 1));
    exec(
        &fx,
        "UPDATE doc_history_state
            SET built_rev = (SELECT head_rev FROM doc_history_heads WHERE doc_id = $1) - 500
          WHERE doc_id = $1",
        lagging,
    )
    .await;
    assert_eq!(flags(&fx, lagging).await, (false, false, 1));

    // Given up: unavailable, never catching up, however far behind.
    exec(
        &fx,
        "UPDATE doc_history_state SET retry_after = 'infinity', built_rev = 0 WHERE doc_id = $1",
        lagging,
    )
    .await;
    assert_eq!(flags(&fx, lagging).await, (false, true, 1));
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

/// The only entry of `kind` in `doc`'s list as `m`, and the whole body.
async fn entry_of(fx: &Fixture, m: &Member, doc: Uuid, kind: &str) -> (Value, String) {
    let a = entries(fx, Some(&m.session), doc, "").await;
    let listed = a.ok();
    let found: Vec<&Value> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .collect();
    assert_eq!(found.len(), 1, "{listed}");
    (found[0].clone(), a.body)
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_and_template_sources_are_hidden_from_readers_who_cannot_see_them() {
    let fx = fixture().await;
    let docs = fx.state.docs.as_ref().unwrap();

    // A restricted template, instantiated into an open page by the owner.
    let template = page(&fx, "Secret template").await;
    import(&fx, template, "# Agenda\n").await;
    docs.set_template(fx.ws_id, template, fx.owner.id, true)
        .await
        .unwrap();
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{template}/restricted"),
        Some(json!({"restricted": true})),
    )
    .await;
    assert!(a.status.is_success(), "restrict: {} {}", a.status, a.body);
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "POST",
        &format!("/api/docs/from-template/{template}"),
        Some(json!({"title": "Monday"})),
    )
    .await;
    assert!(
        a.status.is_success(),
        "from template: {} {}",
        a.status,
        a.body
    );
    let monday: Uuid = a.json()["id"].as_str().unwrap().parse().unwrap();
    build(&fx, monday).await;

    let (e, body) = entry_of(&fx, &fx.editor, monday, "template").await;
    assert_eq!(e["meta"], json!({"template_hidden": true}));
    assert!(!body.contains(&template.to_string()), "{body}");
    assert!(!body.contains("Secret template"), "{body}");
    let (e, _) = entry_of(&fx, &fx.owner, monday, "template").await;
    assert_eq!(
        e["meta"],
        json!({"template_doc_id": template, "template_title": "Secret template"})
    );

    // A copy of a restricted page: restricted too, with only its actor as
    // owner, so the editor gets a grant on the copy (not on the source).
    let source = page(&fx, "Secret source").await;
    import(&fx, source, "# Plans\n").await;
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{source}/restricted"),
        Some(json!({"restricted": true})),
    )
    .await;
    assert!(a.status.is_success(), "restrict: {} {}", a.status, a.body);
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "POST",
        &format!("/api/docs/{source}/duplicate"),
        Some(json!({"include_subpages": false})),
    )
    .await;
    assert!(a.status.is_success(), "duplicate: {} {}", a.status, a.body);
    let copy: Uuid = a.json()["id"].as_str().unwrap().parse().unwrap();
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "PUT",
        &format!("/api/docs/{copy}/grants/user:{}", fx.editor.id),
        Some(json!({"role": "editor", "inherit": true})),
    )
    .await;
    assert!(a.status.is_success(), "grant: {} {}", a.status, a.body);
    build(&fx, copy).await;

    let (e, body) = entry_of(&fx, &fx.editor, copy, "copy").await;
    assert_eq!(e["meta"], json!({"source_hidden": true}));
    assert!(!body.contains(&source.to_string()), "{body}");
    assert!(!body.contains("Secret source"), "{body}");
    let (e, _) = entry_of(&fx, &fx.owner, copy, "copy").await;
    assert_eq!(
        e["meta"],
        json!({"source_doc_id": source, "source_title": "Secret source", "version_at": null})
    );

    // A purged source is hidden from everyone.
    for (method, uri) in [
        ("DELETE", format!("/api/docs/{source}")),
        ("POST", format!("/api/docs/{source}/purge")),
    ] {
        let a = send(&fx, Some(&fx.owner.session), method, &uri, None).await;
        assert!(
            a.status.is_success(),
            "{method} {uri}: {} {}",
            a.status,
            a.body
        );
    }
    for m in [&fx.owner, &fx.editor] {
        let (e, body) = entry_of(&fx, m, copy, "copy").await;
        assert_eq!(e["meta"], json!({"source_hidden": true}), "{}", m.name);
        assert!(!body.contains("Secret source"), "{body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_and_accepted_suggestion_entries_carry_their_meta() {
    let fx = fixture().await;
    let doc = page(&fx, "Restored").await;

    // The owner restores a snapshot through the route.
    let (_, bytes) = knot_markdown::from_markdown::parse("# Snapshot\n\nOld text.").unwrap();
    let engine = knot_crdt::YrsEngine;
    let d = knot_crdt::Engine::new_doc(&engine);
    knot_crdt::Engine::apply_update(&engine, &d, &bytes).unwrap();
    let state = knot_crdt::Engine::encode_state_as_update(&engine, &d, None).unwrap();
    let sv = knot_crdt::Engine::encode_state_vector(&engine, &d).unwrap();
    let snapshots = PgSnapshotStore::new(fx.pool.clone());
    snapshots.insert(doc, 1, &state, &sv).await.unwrap();
    let taken = snapshots.by_seq(doc, 1).await.unwrap().unwrap();
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "POST",
        &format!("/api/docs/{doc}/history/1/restore"),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "restore: {}", a.body);

    // The editor types "Hello world" (client 7: "Hello" is clocks 2..=6),
    // suggests "Howdy" for "Hello", and the owner accepts it.
    let seed = {
        let t = Typist::new(7);
        t.para("Hello world");
        t.state()
    };
    let room = fx
        .state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(doc)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::ApplyUpdate {
            update_bytes: seed,
            by_user: Some(fx.editor.id),
            source: UpdateSource::Edit,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("seed");
    let anchor = |clock: u32, assoc: Assoc| {
        STANDARD.encode(StickyIndex::from_id(ID::new(ClientID::new(7), clock), assoc).encode_v1())
    };
    let a = send(
        &fx,
        Some(&fx.editor.session),
        "POST",
        &format!("/api/docs/{doc}/suggestions"),
        Some(json!({
            "kind": "replace",
            "position_y": anchor(2, Assoc::After),
            "position_y_end": anchor(6, Assoc::Before),
            "original_text": "Hello",
            "context_before": "",
            "proposed_text": "Howdy",
        })),
    )
    .await;
    assert!(a.status.is_success(), "suggest: {} {}", a.status, a.body);
    let suggestion: Uuid = a.json()["id"].as_str().unwrap().parse().unwrap();
    let a = send(
        &fx,
        Some(&fx.owner.session),
        "POST",
        &format!("/api/docs/{doc}/suggestions/{suggestion}/accept"),
        Some(json!({"seen_text": "Hello"})),
    )
    .await;
    assert!(a.status.is_success(), "accept: {} {}", a.status, a.body);
    // The accepted suggestion's row is in before the build.
    let deadline = Instant::now() + DEADLINE;
    while !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM doc_updates WHERE doc_id = $1 AND source = 'suggestion')",
    )
    .bind(doc)
    .fetch_one(&fx.pool)
    .await
    .unwrap()
    {
        assert!(
            Instant::now() < deadline,
            "the accepted suggestion has no row"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    build(&fx, doc).await;

    let (restore, body) = entry_of(&fx, &fx.owner, doc, "restore").await;
    assert_eq!(
        restore["author"],
        json!({"key": fx.owner.id, "name": "Olga", "former": false})
    );
    assert_eq!(
        restore["meta"],
        json!({
            "target_ended_at": taken.created_at.to_rfc3339(),
            "target_entry_id": null,
            "fallback": null,
        })
    );
    assert!(!body.contains("target_snapshot_seq"), "{body}");
    let (session, body) = entry_of(&fx, &fx.owner, doc, "session").await;
    assert_eq!(session["author"]["key"], json!(fx.editor.id));
    assert_eq!(
        session["meta"],
        json!({"accepted_by": [{"key": fx.owner.id, "name": "Olga", "former": false}]})
    );
    assert!(!body.contains(&suggestion.to_string()), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_old_snapshot_list_still_answers() {
    let fx = fixture().await;
    let doc = page(&fx, "Old drawer").await;
    let t = Typist::new(9);
    t.para("snap");
    PgSnapshotStore::new(fx.pool.clone())
        .insert(doc, 1, &t.state(), &[0])
        .await
        .unwrap();
    let listed = send(
        &fx,
        Some(&fx.editor.session),
        "GET",
        &format!("/api/docs/{doc}/history"),
        None,
    )
    .await
    .ok();
    let snapshots = listed.as_array().expect("the SnapshotMeta array");
    assert_eq!(snapshots.len(), 1, "{listed}");
    assert_eq!(snapshots[0]["snapshot_seq"], json!(1));
}
