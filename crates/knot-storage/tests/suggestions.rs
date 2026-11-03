//! Integration tests for `PgSuggestionStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use chrono::{Duration, Utc};
use knot_storage::{
    DocStore, NewNotification, NewSuggestion, NotificationKind, NotificationStore, PgDocStore,
    PgNotificationStore, PgSuggestionStore, PgUserStore, PgWorkspaceStore, SuggestionKind,
    SuggestionStatus, SuggestionStore, SuggestionStoreError, UserStore, WorkspaceRole,
    WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

/// Returns (store, workspace_id, doc_id, alice_id, bob_id). Alice owns the
/// workspace and the doc; Bob is an editor who suggests.
async fn setup() -> (PgSuggestionStore, Uuid, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob Smith", "$h$")
        .await
        .unwrap();
    let ws_store = PgWorkspaceStore::new(pool.clone());
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let sk = sort_key_between(None, None);
    let doc = docs
        .create(ws.id, None, "Doc", &sk, alice.id)
        .await
        .unwrap();
    (
        PgSuggestionStore::new(pool),
        ws.id,
        doc.id,
        alice.id,
        bob.id,
    )
}

/// A replace suggestion by `author` on `doc`.
fn replace_on(doc: Uuid, author: Uuid, proposed: &str) -> NewSuggestion {
    NewSuggestion {
        doc_id: doc,
        author_id: author,
        kind: SuggestionKind::Replace,
        position_y: vec![1, 2, 3],
        position_y_end: vec![4, 5, 6],
        original_text: "within 15 minutes".into(),
        context_before: "Respond ".into(),
        proposed_text: proposed.into(),
        reason: Some("SLA changed".into()),
    }
}

/// Backdate a suggestion's timestamps, issued directly against the pool
/// rather than through a test-only store method.
async fn backdate(store: &PgSuggestionStore, id: Uuid, minutes: i64) {
    sqlx::query(
        "UPDATE suggestions \
         SET created_at = now() - ($2 || ' minutes')::interval, \
             updated_at = now() - ($2 || ' minutes')::interval \
         WHERE id = $1",
    )
    .bind(id)
    .bind(minutes.to_string())
    .execute(store.pool())
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn create_get_list_roundtrip() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let a = store
        .create(&replace_on(doc, bob, "within 30 minutes"))
        .await
        .unwrap();
    assert_eq!(a.doc_id, doc);
    assert_eq!(a.author_id, bob);
    assert_eq!(a.kind, SuggestionKind::Replace);
    assert_eq!(a.status, SuggestionStatus::Pending);
    assert_eq!(a.position_y, vec![1, 2, 3]);
    assert_eq!(a.position_y_end, vec![4, 5, 6]);
    assert_eq!(a.original_text, "within 15 minutes");
    assert_eq!(a.context_before, "Respond ");
    assert_eq!(a.proposed_text, "within 30 minutes");
    assert_eq!(a.reason.as_deref(), Some("SLA changed"));
    assert!(a.decided_by.is_none() && a.decided_at.is_none());

    let got = store.get(a.id).await.unwrap();
    assert_eq!(got.id, a.id);
    assert_eq!(got.created_at, a.created_at);

    let insert = store
        .create(&NewSuggestion {
            kind: SuggestionKind::Insert,
            original_text: String::new(),
            context_before: String::new(),
            proposed_text: "€ and 😀".into(),
            reason: None,
            ..replace_on(doc, bob, "")
        })
        .await
        .unwrap();
    assert_eq!(insert.kind, SuggestionKind::Insert);
    assert_eq!(insert.proposed_text, "€ and 😀");
    assert!(insert.reason.is_none());

    let rejected = store
        .create(&replace_on(doc, bob, "within 1 hour"))
        .await
        .unwrap();
    store.reject(rejected.id, alice).await.unwrap();

    let open: Vec<Uuid> = store
        .list(doc, false)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(open, vec![a.id, insert.id], "rejected ones are hidden");

    let all: Vec<Uuid> = store
        .list(doc, true)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(all, vec![a.id, insert.id, rejected.id]);

    assert!(matches!(
        store.get(Uuid::new_v4()).await,
        Err(SuggestionStoreError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn serializes_anchors_as_base64_and_enums_lowercase() {
    let (store, _ws, doc, _alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["position_y"], "AQID");
    assert_eq!(v["position_y_end"], "BAUG");
    assert_eq!(v["kind"], "replace");
    assert_eq!(v["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn claim_is_exclusive() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let (r1, r2) = tokio::join!(
        store.claim_for_accept(s.id, alice),
        store.claim_for_accept(s.id, bob),
    );
    let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let not_pending = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(SuggestionStoreError::NotPending)))
        .count();
    assert_eq!((oks, not_pending), (1, 1), "{r1:?} / {r2:?}");
    let claimed = r1.or(r2).unwrap();
    assert_eq!(claimed.status, SuggestionStatus::Applying);
    assert!(claimed.decided_by.is_some());
    assert!(claimed.decided_at.is_none());

    assert!(matches!(
        store.claim_for_accept(Uuid::new_v4(), alice).await,
        Err(SuggestionStoreError::NotFound)
    ));
}

/// The claim mints the token the room records as the doc's applied marker.
/// It is kept across a release, so a re-accept of a suggestion whose text
/// landed after its row was released sees its own marker; it is read back
/// by the sweep's listing and never serialized.
#[tokio::test(flavor = "multi_thread")]
async fn claim_mints_a_hidden_apply_token_kept_across_release() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    assert_eq!(s.apply_token, None);

    let claimed = store.claim_for_accept(s.id, alice).await.unwrap();
    let token = claimed.apply_token.expect("the claim sets a token");
    let v = serde_json::to_value(&claimed).unwrap();
    assert!(v.get("apply_token").is_none(), "token serialized: {v}");
    assert!(
        !v.to_string().contains(&token.to_string()),
        "token leaked: {v}"
    );

    backdate(&store, s.id, 10).await;
    let stuck = store
        .list_stuck_applying(Utc::now() - Duration::seconds(120))
        .await
        .unwrap();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].apply_token, Some(token));

    store.release(s.id).await.unwrap();
    let again = store.claim_for_accept(s.id, alice).await.unwrap();
    assert_eq!(again.apply_token, Some(token));

    // Another row gets its own.
    let other = store.create(&replace_on(doc, bob, "y")).await.unwrap();
    let other = store.claim_for_accept(other.id, alice).await.unwrap();
    assert_ne!(other.apply_token, Some(token));
    assert!(other.apply_token.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn release_returns_to_pending() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let claimed = store.claim_for_accept(s.id, alice).await.unwrap();
    assert_eq!(claimed.decided_by, Some(alice));

    store.release(s.id).await.unwrap();
    let back = store.get(s.id).await.unwrap();
    assert_eq!(back.status, SuggestionStatus::Pending);
    assert!(back.decided_by.is_none());
    assert!(back.updated_at >= claimed.updated_at);

    // Released once already: no longer applying.
    assert!(matches!(
        store.release(s.id).await,
        Err(SuggestionStoreError::NotPending)
    ));
    // It can be claimed again.
    store.claim_for_accept(s.id, alice).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn finish_from_applying_or_pending_only() {
    let (store, _ws, doc, alice, bob) = setup().await;

    // applying → accepted.
    let a = store.create(&replace_on(doc, bob, "a")).await.unwrap();
    store.claim_for_accept(a.id, alice).await.unwrap();
    let done = store.finish_accept(a.id, alice).await.unwrap();
    assert_eq!(done.status, SuggestionStatus::Accepted);
    assert_eq!(done.decided_by, Some(alice));
    assert!(done.decided_at.is_some());

    // pending → accepted (a sweep released it meanwhile).
    let b = store.create(&replace_on(doc, bob, "b")).await.unwrap();
    store.claim_for_accept(b.id, alice).await.unwrap();
    store.release(b.id).await.unwrap();
    let done = store.finish_accept(b.id, alice).await.unwrap();
    assert_eq!(done.status, SuggestionStatus::Accepted);
    assert_eq!(done.decided_by, Some(alice));

    // Accepted, rejected or withdrawn rows cannot be finished.
    assert!(matches!(
        store.finish_accept(a.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));
    let c = store.create(&replace_on(doc, bob, "c")).await.unwrap();
    store.reject(c.id, alice).await.unwrap();
    assert!(matches!(
        store.finish_accept(c.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));
    let d = store.create(&replace_on(doc, bob, "d")).await.unwrap();
    store.withdraw(d.id).await.unwrap();
    assert!(matches!(
        store.finish_accept(d.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));
    assert!(matches!(
        store.finish_accept(Uuid::new_v4(), alice).await,
        Err(SuggestionStoreError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn reject_and_withdraw_only_from_pending() {
    let (store, _ws, doc, alice, bob) = setup().await;

    let r = store.create(&replace_on(doc, bob, "r")).await.unwrap();
    let rejected = store.reject(r.id, alice).await.unwrap();
    assert_eq!(rejected.status, SuggestionStatus::Rejected);
    assert_eq!(rejected.decided_by, Some(alice));
    assert!(rejected.decided_at.is_some());
    assert!(matches!(
        store.reject(r.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));
    assert!(matches!(
        store.withdraw(r.id).await,
        Err(SuggestionStoreError::NotPending)
    ));

    let w = store.create(&replace_on(doc, bob, "w")).await.unwrap();
    let withdrawn = store.withdraw(w.id).await.unwrap();
    assert_eq!(withdrawn.status, SuggestionStatus::Withdrawn);
    assert!(withdrawn.decided_at.is_some());
    assert!(matches!(
        store.withdraw(w.id).await,
        Err(SuggestionStoreError::NotPending)
    ));
    assert!(matches!(
        store.reject(w.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));

    // A row being applied cannot be rejected or withdrawn underneath it.
    let a = store.create(&replace_on(doc, bob, "a")).await.unwrap();
    store.claim_for_accept(a.id, alice).await.unwrap();
    assert!(matches!(
        store.reject(a.id, alice).await,
        Err(SuggestionStoreError::NotPending)
    ));
    assert!(matches!(
        store.withdraw(a.id).await,
        Err(SuggestionStoreError::NotPending)
    ));

    assert!(matches!(
        store.reject(Uuid::new_v4(), alice).await,
        Err(SuggestionStoreError::NotFound)
    ));
    assert!(matches!(
        store.withdraw(Uuid::new_v4()).await,
        Err(SuggestionStoreError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_stuck_applying_respects_age() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    // A pending row, however old, is not stuck.
    backdate(&store, s.id, 10).await;
    let cutoff = Utc::now() - Duration::seconds(120);
    assert!(store.list_stuck_applying(cutoff).await.unwrap().is_empty());

    // The claim bumps updated_at, so a fresh claim is not stuck either.
    store.claim_for_accept(s.id, alice).await.unwrap();
    assert!(store.list_stuck_applying(cutoff).await.unwrap().is_empty());

    // Once the claim itself is old, it is.
    backdate(&store, s.id, 10).await;
    let stuck: Vec<Uuid> = store
        .list_stuck_applying(cutoff)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(stuck, vec![s.id]);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_if_unchanged_is_compare_and_set() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let first = store.claim_for_accept(s.id, alice).await.unwrap();

    // The row moved on (released and claimed again): the stale CAS loses.
    store.release(s.id).await.unwrap();
    let second = store.claim_for_accept(s.id, alice).await.unwrap();
    assert_ne!(first.updated_at, second.updated_at);
    assert!(
        !store
            .release_if_unchanged(s.id, first.updated_at)
            .await
            .unwrap()
    );
    assert_eq!(
        store.get(s.id).await.unwrap().status,
        SuggestionStatus::Applying
    );

    // The current timestamp wins.
    assert!(
        store
            .release_if_unchanged(s.id, second.updated_at)
            .await
            .unwrap()
    );
    let back = store.get(s.id).await.unwrap();
    assert_eq!(back.status, SuggestionStatus::Pending);
    assert!(back.decided_by.is_none());

    // Not applying any more: nothing to release.
    assert!(
        !store
            .release_if_unchanged(s.id, back.updated_at)
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn too_long_proposed_text_is_rejected() {
    let (store, _ws, doc, _alice, bob) = setup().await;
    // Counted in characters, not bytes: 10 000 multi-byte chars fit.
    let max = "ä".repeat(10_000);
    store.create(&replace_on(doc, bob, &max)).await.unwrap();

    let over = "x".repeat(10_001);
    assert!(matches!(
        store.create(&replace_on(doc, bob, &over)).await,
        Err(SuggestionStoreError::TooLong)
    ));
    assert!(matches!(
        store
            .create(&NewSuggestion {
                original_text: over.clone(),
                ..replace_on(doc, bob, "y")
            })
            .await,
        Err(SuggestionStoreError::TooLong)
    ));
    assert!(matches!(
        store
            .create(&NewSuggestion {
                context_before: "c".repeat(201),
                ..replace_on(doc, bob, "y")
            })
            .await,
        Err(SuggestionStoreError::TooLong)
    ));
    assert!(matches!(
        store
            .create(&NewSuggestion {
                reason: Some("r".repeat(4_097)),
                ..replace_on(doc, bob, "y")
            })
            .await,
        Err(SuggestionStoreError::TooLong)
    ));
    assert_eq!(store.list(doc, true).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_crud_and_soft_delete() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let other = store.create(&replace_on(doc, bob, "y")).await.unwrap();

    let r1 = store.create_reply(s.id, alice, "Why?").await.unwrap();
    assert_eq!(r1.suggestion_id, s.id);
    assert_eq!(r1.author_id, alice);
    assert_eq!(r1.body, "Why?");
    let r2 = store.create_reply(other.id, bob, "Because.").await.unwrap();

    let got = store.get_reply(r1.id).await.unwrap();
    assert_eq!(got.body, "Why?");

    let edited = store.update_reply(r1.id, "Why, though?").await.unwrap();
    assert_eq!(edited.body, "Why, though?");
    assert!(edited.updated_at >= r1.updated_at);

    let listed: Vec<Uuid> = store
        .list_replies(doc)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(
        listed,
        vec![r1.id, r2.id],
        "every suggestion's, oldest first"
    );

    store.delete_reply(r1.id).await.unwrap();
    // Idempotent: a second delete and an unknown id are both Ok.
    store.delete_reply(r1.id).await.unwrap();
    store.delete_reply(Uuid::new_v4()).await.unwrap();
    assert!(matches!(
        store.get_reply(r1.id).await,
        Err(SuggestionStoreError::NotFound)
    ));
    assert!(matches!(
        store.update_reply(r1.id, "again").await,
        Err(SuggestionStoreError::NotFound)
    ));
    let listed: Vec<Uuid> = store
        .list_replies(doc)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, vec![r2.id]);

    // The row stays, soft-deleted.
    let (deleted,): (bool,) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM suggestion_replies WHERE id = $1")
            .bind(r1.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert!(deleted);

    assert!(matches!(
        store.create_reply(s.id, alice, &"b".repeat(4_097)).await,
        Err(SuggestionStoreError::TooLong)
    ));
    assert!(matches!(
        store.create_reply(Uuid::new_v4(), alice, "orphan").await,
        Err(SuggestionStoreError::NotFound)
    ));
    assert!(matches!(
        store.update_reply(r2.id, &"b".repeat(4_097)).await,
        Err(SuggestionStoreError::TooLong)
    ));
    // The lower bound is a typed error too, not the DB CHECK's sqlx error.
    assert!(matches!(
        store.create_reply(s.id, alice, "").await,
        Err(SuggestionStoreError::EmptyBody)
    ));
    assert!(matches!(
        store.update_reply(r2.id, "").await,
        Err(SuggestionStoreError::EmptyBody)
    ));
    // Exactly at the limit is fine.
    store.update_reply(r2.id, &"b".repeat(4_096)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_purge_cascades() {
    let (store, _ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let r = store.create_reply(s.id, alice, "ok").await.unwrap();

    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc)
        .execute(store.pool())
        .await
        .unwrap();

    assert!(matches!(
        store.get(s.id).await,
        Err(SuggestionStoreError::NotFound)
    ));
    let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM suggestion_replies WHERE id = $1")
        .bind(r.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_notification_kinds_insert() {
    let (store, ws, doc, alice, bob) = setup().await;
    let s = store.create(&replace_on(doc, bob, "x")).await.unwrap();
    let notes = PgNotificationStore::new(store.pool().clone());
    let note = |user_id, actor, kind, key: String| NewNotification {
        workspace_id: ws,
        user_id,
        actor_id: Some(actor),
        kind,
        doc_id: Some(doc),
        target_kind: "suggestion".into(),
        target_id: s.id.to_string(),
        dedupe_key: key,
        data: serde_json::json!({
            "suggestion_id": s.id,
            "doc_title": "Doc",
            "excerpt": "within 15 minutes",
        }),
    };
    assert!(
        notes
            .emit(&note(
                alice,
                bob,
                NotificationKind::Suggestion,
                format!("suggestion:{}", s.id),
            ))
            .await
            .unwrap()
    );
    assert!(
        notes
            .emit(&note(
                bob,
                alice,
                NotificationKind::SuggestionDecided,
                format!("suggestion_decided:{}", s.id),
            ))
            .await
            .unwrap()
    );
    let alice_rows = notes.list(alice, ws, &[], false, 50, None).await.unwrap();
    assert_eq!(alice_rows.len(), 1);
    assert_eq!(alice_rows[0].kind, "suggestion");
    let bob_rows = notes.list(bob, ws, &[], false, 50, None).await.unwrap();
    assert_eq!(bob_rows.len(), 1);
    assert_eq!(bob_rows[0].kind, "suggestion_decided");
}
