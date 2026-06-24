//! Collab WebSocket authz tests.
//!
//! T1: A VIEWER-sent y-sync-update is DROPPED by the server (the owner
//!     connection does NOT receive it within the polling window).
//! T2: An OWNER-sent update DOES propagate to the viewer (proves the
//!     harness works and only the viewer-write path is gated).

use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, WorkspaceRole};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a complete, authed AppState with rooms_v2 wired to MemBus +
/// real Postgres-backed update/snapshot stores. Mirrors `login_state`
/// from docs_integration.rs.
async fn full_state_with_rooms(email: &str, password: &str) -> (AppState, String) {
    let db = knot_test_support::fresh_db().await;

    let mut s = AppState::with_pool(db.pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s.cookie_secure = false; // plain HTTP in tests

    // Wire MemBus + Rooms so collab_upgrade can acquire rooms.
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    let updates: Arc<dyn knot_storage::UpdatesStore> =
        Arc::new(PgUpdatesStore::new(db.pool.clone()));
    let snapshots: Arc<dyn knot_storage::SnapshotStore> =
        Arc::new(PgSnapshotStore::new(db.pool.clone()));
    let rooms = Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        bus,
        updates,
        snapshots,
        knot_crdt::SnapshotPolicy { every_n: 100, idle: Duration::from_secs(60) },
        Duration::from_secs(300),
    ));
    s.rooms_v2 = Some(rooms);

    let hash = s.hasher.hash(password).unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let u = s
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    // Log in and capture the session cookie.
    let app = router_with_state(s.clone());
    let r = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid_kv = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf_kv = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let cookie_header = format!("{sid_kv}; {csrf_kv}");
    let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();

    (s, format!("{cookie_header}|{csrf_token}"))
}

fn split_cookie_csrf(joined: &str) -> (&str, &str) {
    joined.split_once('|').unwrap()
}

/// Open an authenticated WebSocket to the collab endpoint for `doc_id`,
/// passing the `sid` cookie so the session loader can authenticate the
/// connection.
async fn open_authed_ws(
    addr: std::net::SocketAddr,
    doc_id: uuid::Uuid,
    sid_cookie: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("ws://{addr}/collab/doc/{doc_id}");
    let mut req = url.into_client_request().expect("valid ws url");
    req.headers_mut().insert(
        "cookie",
        tungstenite::http::HeaderValue::from_str(sid_cookie).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect");
    ws
}

/// Build a y-sync SYNC_UPDATE frame carrying a real Yrs update.
fn make_yrs_update_frame() -> Vec<u8> {
    use yrs::updates::decoder::Decode;
    use yrs::updates::encoder::Encode;
    use yrs::{Doc, ReadTxn, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    let doc = Doc::new();
    let sv_empty = {
        let txn = doc.transact();
        txn.state_vector().encode_v1()
    };
    {
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("convergence test"));
    }
    let update = {
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::decode_v1(&sv_empty).unwrap())
    };
    // MSG_SYNC (0) + SYNC_UPDATE (2) + varuint(len) + payload
    let mut frame = vec![0u8, 2u8];
    append_var_uint(&mut frame, update.len() as u64);
    frame.extend_from_slice(&update);
    frame
}

fn append_var_uint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Drain all messages currently queued on `ws`, discarding them.
/// Used to consume the initial sync-step-2 frame the server sends on connect.
async fn drain_initial(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    // The server sends exactly one sync-step-2 on join. Read it.
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
}

// ---------------------------------------------------------------------------
// Test: viewer write is dropped; owner write propagates
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_write_owner_can() {
    // --- Setup: owner + viewer on the same workspace + doc -----------------
    let (state, owner_joined) =
        full_state_with_rooms("collab-owner@ws.test", "ownerpass").await;
    let (owner_cookie, owner_csrf) = split_cookie_csrf(&owner_joined);

    // We need to keep a reference to the inner SID cookie for the WS handshake.
    // owner_cookie is "sid=<v>; csrf=<v>".
    let owner_sid = owner_cookie; // full "sid=…; csrf=…" string works for the cookie header

    // Invite a viewer to the workspace.
    let viewer_password = "viewerpass";
    let viewer_email = "collab-viewer@ws.test";
    {
        let app = router_with_state(state.clone());
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/workspace/members")
                    .header("content-type", "application/json")
                    .header("cookie", owner_cookie)
                    .header("x-csrf-token", owner_csrf)
                    .body(Body::from(
                        serde_json::json!({
                            "email": viewer_email,
                            "role": "viewer",
                            "password": viewer_password
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "invite viewer");
        let _ = r.into_body().collect().await.unwrap();
    }

    // Log the viewer in via HTTP to get their SID cookie.
    let viewer_sid = {
        let app = router_with_state(state.clone());
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "email": viewer_email,
                            "password": viewer_password
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT, "viewer login");
        let cookies: Vec<String> = r
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        let sid_kv = cookies
            .iter()
            .find(|c| c.starts_with("sid="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let csrf_kv = cookies
            .iter()
            .find(|c| c.starts_with("csrf="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        format!("{sid_kv}; {csrf_kv}")
    };

    // Create a doc (owner's session needed).
    let doc_id: uuid::Uuid = {
        let app = router_with_state(state.clone());
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/docs")
                    .header("cookie", owner_cookie)
                    .header("x-csrf-token", owner_csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"title": "Collab Doc"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "create doc");
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v["id"].as_str().unwrap().parse().unwrap()
    };

    // --- Serve the router on a bound port ----------------------------------
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router_with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the server a moment to be ready.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // --- Open authenticated WS connections ---------------------------------
    let mut owner_ws = open_authed_ws(addr, doc_id, owner_sid).await;
    let mut viewer_ws = open_authed_ws(addr, doc_id, &viewer_sid).await;

    // Drain the initial sync-step-2 frame from each connection.
    drain_initial(&mut owner_ws).await;
    drain_initial(&mut viewer_ws).await;

    // --- T1: Viewer sends an update; owner should NOT receive it -----------
    // (The server drops inbound updates from can_write=false connections.)
    let update_frame = make_yrs_update_frame();
    viewer_ws
        .send(tungstenite::Message::Binary(update_frame))
        .await
        .unwrap();

    // Wait briefly; owner must NOT receive anything.
    let owner_got_viewer_update = tokio::time::timeout(
        Duration::from_millis(400),
        owner_ws.next(),
    )
    .await;

    // timeout = the server correctly dropped the update (expected path).
    // non-timeout = the server forwarded it (bug).
    assert!(
        owner_got_viewer_update.is_err(),
        "owner MUST NOT receive viewer's update: server should have dropped it"
    );

    // --- T2: Owner sends an update; viewer SHOULD receive it ---------------
    let owner_update_frame = make_yrs_update_frame();
    owner_ws
        .send(tungstenite::Message::Binary(owner_update_frame))
        .await
        .unwrap();

    let viewer_got_owner_update = tokio::time::timeout(
        Duration::from_secs(3),
        viewer_ws.next(),
    )
    .await;
    match viewer_got_owner_update {
        Err(_elapsed) => panic!("viewer timed out waiting for owner's update — propagation broken"),
        Ok(None) => panic!("viewer stream ended before receiving owner's update"),
        Ok(Some(Err(e))) => panic!("viewer received a WS error: {e}"),
        Ok(Some(Ok(tungstenite::Message::Binary(bytes)))) => {
            // Expect a sync-update frame (MSG_SYNC=0, SYNC_UPDATE=2).
            assert_eq!(bytes[0], 0, "expected MSG_SYNC byte, got {}", bytes[0]);
            assert_eq!(bytes[1], 2, "expected SYNC_UPDATE byte, got {}", bytes[1]);
        }
        Ok(Some(Ok(other))) => {
            panic!("viewer received unexpected WS message type: {other:?}");
        }
    }

    // Clean up connections.
    let _ = owner_ws.send(tungstenite::Message::Close(None)).await;
    let _ = viewer_ws.send(tungstenite::Message::Close(None)).await;
}
