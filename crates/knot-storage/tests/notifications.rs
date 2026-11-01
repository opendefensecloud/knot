//! Integration tests for `PgNotificationStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use knot_storage::{
    DocStore, DocTaskInput, NewNotification, Notification, NotificationKind, NotificationStore,
    PgDocStore, PgNotificationStore, PgTaskStore, PgUserStore, PgWorkspaceStore, TaskStore,
    TaskWrite, UserStore, WorkspaceRole, WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

/// Returns (store, workspace_id, doc_id, alice_id, bob_id).
async fn setup() -> (PgNotificationStore, Uuid, Uuid, Uuid, Uuid) {
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
        PgNotificationStore::new(pool),
        ws.id,
        doc.id,
        alice.id,
        bob.id,
    )
}

/// Backdate a row and optionally mark it read, so `prune` can be exercised
/// without waiting 90 days. Issued directly against the pool rather than
/// through a test-only method on `PgNotificationStore`, since that store is
/// production API surface.
async fn backdate(store: &PgNotificationStore, id: i64, days_old: i64, read: bool) {
    sqlx::query(
        "UPDATE notifications \
         SET created_at = now() - ($2 || ' days')::interval, \
             read_at = CASE WHEN $3 THEN now() - ($2 || ' days')::interval ELSE NULL END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(days_old.to_string())
    .bind(read)
    .execute(store.pool())
    .await
    .unwrap();
}

/// Hard-delete a document to prove the FK cascade, issued directly against
/// the pool rather than through a test-only method on `PgNotificationStore`.
async fn hard_delete_doc(store: &PgNotificationStore, doc_id: Uuid) {
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc_id)
        .execute(store.pool())
        .await
        .unwrap();
}

fn mention_for(
    ws: Uuid,
    doc: Uuid,
    recipient: Uuid,
    actor: Uuid,
    comment: Uuid,
) -> NewNotification {
    NewNotification {
        workspace_id: ws,
        user_id: recipient,
        actor_id: Some(actor),
        kind: NotificationKind::Mention,
        doc_id: Some(doc),
        target_kind: "comment".into(),
        target_id: comment.to_string(),
        dedupe_key: format!("mention:{comment}"),
        data: serde_json::json!({ "excerpt": "hello @Bob Smith" }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn emit_then_list_returns_the_row_with_actor_and_doc_title() {
    let (store, ws, doc, alice, bob) = setup().await;
    let comment = Uuid::new_v4();
    assert!(
        store
            .emit(&mention_for(ws, doc, bob, alice, comment))
            .await
            .unwrap()
    );

    let rows = store.list(bob, ws, &[], false, 50, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "mention");
    assert_eq!(rows[0].actor_display_name.as_deref(), Some("Alice"));
    assert_eq!(rows[0].doc_title.as_deref(), Some("Doc"));
    assert!(rows[0].read_at.is_none());

    // Alice is the actor, not a recipient.
    assert!(
        store
            .list(alice, ws, &[], false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn emit_is_idempotent_on_dedupe_key() {
    let (store, ws, doc, alice, bob) = setup().await;
    let comment = Uuid::new_v4();
    let n = mention_for(ws, doc, bob, alice, comment);

    assert!(store.emit(&n).await.unwrap(), "first emit inserts");
    assert!(!store.emit(&n).await.unwrap(), "second emit is deduped");
    assert_eq!(
        store
            .list(bob, ws, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_notification_is_dropped() {
    let (store, ws, doc, alice, _bob) = setup().await;
    let comment = Uuid::new_v4();
    // Alice mentions herself.
    assert!(
        !store
            .emit(&mention_for(ws, doc, alice, alice, comment))
            .await
            .unwrap()
    );
    assert!(
        store
            .list(alice, ws, &[], false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_count_caps_and_mark_read_clears() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
    }
    assert_eq!(store.unread_count(bob, ws, &[], 100).await.unwrap(), 3);
    // The cap actually binds: with 3 unread rows, a cap below that count
    // must clip the result rather than merely bounding it from above.
    assert_eq!(store.unread_count(bob, ws, &[], 2).await.unwrap(), 2);

    let rows = store.list(bob, ws, &[], true, 50, None).await.unwrap();
    let first = rows[0].id;
    assert_eq!(store.mark_read(bob, &[first]).await.unwrap(), 1);
    assert_eq!(store.unread_count(bob, ws, &[], 100).await.unwrap(), 2);

    assert_eq!(store.mark_all_read(bob).await.unwrap(), 2);
    assert_eq!(store.unread_count(bob, ws, &[], 100).await.unwrap(), 0);
    // Reading does not delete.
    assert_eq!(
        store
            .list(bob, ws, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(
        store
            .list(bob, ws, &[], true, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_read_cannot_touch_another_users_rows() {
    let (store, ws, doc, alice, bob) = setup().await;
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    let bobs = store.list(bob, ws, &[], true, 50, None).await.unwrap();

    assert_eq!(store.mark_read(alice, &[bobs[0].id]).await.unwrap(), 0);
    assert_eq!(store.unread_count(bob, ws, &[], 100).await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_pages_backwards_on_the_cursor() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
    }
    let page1 = store.list(bob, ws, &[], false, 2, None).await.unwrap();
    assert_eq!(page1.len(), 2);
    let page2 = store
        .list(bob, ws, &[], false, 2, Some(page1[1].id))
        .await
        .unwrap();
    assert_eq!(page2.len(), 1);
    assert!(page2[0].id < page1[1].id, "ids descend across pages");
}

#[tokio::test(flavor = "multi_thread")]
async fn prune_drops_read_rows_past_ninety_days_and_everything_past_one_eighty() {
    let (store, ws, doc, alice, bob) = setup().await;
    for _ in 0..3 {
        store
            .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
            .await
            .unwrap();
    }
    let rows = store.list(bob, ws, &[], false, 50, None).await.unwrap();
    // Row 0: read, 100 days old  -> pruned.
    // Row 1: unread, 100 days old -> kept.
    // Row 2: unread, 200 days old -> pruned.
    backdate(&store, rows[0].id, 100, true).await;
    backdate(&store, rows[1].id, 100, false).await;
    backdate(&store, rows[2].id, 200, false).await;

    assert_eq!(store.prune(chrono::Utc::now()).await.unwrap(), 2);
    let left = store.list(bob, ws, &[], false, 50, None).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, rows[1].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn pruning_a_read_assignment_does_not_bring_it_back() {
    // Whether a task is newly assigned is read from the task rows the
    // write replaces, not from the notification, so retention may delete a
    // `task_assigned` row like any other: the next edit of the doc doesn't
    // notify the assignee again.
    let f = task_fixture().await;
    let ship = task(0, "ship it", f.bob, false);
    f.write(std::slice::from_ref(&ship)).await;
    let rows = f.inbox(f.bob).await;
    assert_eq!(rows.len(), 1);
    backdate(&f.notifications, rows[0].id, 200, true).await;

    assert_eq!(f.notifications.prune(chrono::Utc::now()).await.unwrap(), 1);
    f.write(&[ship, task(1, "an unrelated line", f.carol, true)])
        .await;
    assert!(f.inbox(f.bob).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_doc_cascades_its_notifications() {
    let (store, ws, doc, alice, bob) = setup().await;
    store
        .emit(&mention_for(ws, doc, bob, alice, Uuid::new_v4()))
        .await
        .unwrap();
    hard_delete_doc(&store, doc).await;
    assert!(
        store
            .list(bob, ws, &[], false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_notifies_once_through_a_reorder_and_an_edit() {
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
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    let carol = users
        .create_local("carol@x.test", "Carol", "$h$")
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
    ws_store
        .add_member(ws.id, carol.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let doc = docs
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    let tasks = PgTaskStore::new(pool.clone());
    let notifications = PgNotificationStore::new(pool.clone());

    // Different assignees per item on purpose: after the swap below, each
    // task id ("<doc_id>:<item_index>") is paired with a (text, assignee)
    // combination it has never held before. If both items shared one
    // assignee, an id-keyed dedupe scheme would report the same
    // notification counts as the content-keyed one and this test would
    // pass even against that regression.
    let ship = DocTaskInput {
        item_index: 0,
        text: "ship it".into(),
        assignee_user_id: Some(bob.id),
        checked: false,
        due_at: None,
    };
    let review = DocTaskInput {
        item_index: 1,
        text: "review it".into(),
        assignee_user_id: Some(carol.id),
        checked: false,
        due_at: None,
    };

    tasks
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[ship.clone(), review.clone()],
            TaskWrite::Edit(Some(alice.id)),
        )
        .await
        .unwrap();
    assert_eq!(
        notifications
            .list(bob.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        notifications
            .list(carol.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );

    // Swap the two items: "ship it" moves to index 1, "review it" moves to
    // index 0. Every task id changes, and — because the assignees differ —
    // each id is now paired with an assignee it never held before either
    // (id 0 was ship/bob, is now review/carol; id 1 was review/carol, is
    // now ship/bob). An id-keyed dedupe would treat both as brand-new
    // assignments and re-notify; content-keyed dedupe must not.
    let swapped = vec![
        DocTaskInput {
            item_index: 0,
            ..review.clone()
        },
        DocTaskInput {
            item_index: 1,
            ..ship.clone()
        },
    ];
    tasks
        .upsert_for_doc(ws.id, doc.id, &swapped, TaskWrite::Edit(Some(alice.id)))
        .await
        .unwrap();
    assert_eq!(
        notifications
            .list(bob.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "reordering a list must not re-notify"
    );
    assert_eq!(
        notifications
            .list(carol.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "reordering a list must not re-notify"
    );

    // Editing the text is still the same task: no second notification,
    // and the unread one shows the new text. "ship it" now lives at
    // item_index 1, still assigned to bob.
    let edited = vec![DocTaskInput {
        text: "ship it today".into(),
        ..swapped[1].clone()
    }];
    tasks
        .upsert_for_doc(ws.id, doc.id, &edited, TaskWrite::Edit(Some(alice.id)))
        .await
        .unwrap();
    let bobs = notifications
        .list(bob.id, ws.id, &[], false, 50, None)
        .await
        .unwrap();
    assert_eq!(bobs.len(), 1, "editing an assigned task must not re-notify");
    assert_eq!(bobs[0].data["excerpt"], "ship it today");
    assert_eq!(bobs[0].target_id, format!("{}:1", doc.id));
    assert_eq!(
        notifications
            .list(carol.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assignment_with_no_known_actor_still_notifies() {
    // Live edits arrive through the reindex worker (crates/knot-server/src/
    // reindex.rs), which only ever has a doc id, never an editor identity —
    // so `refresh_markdown_and_index` always calls `upsert_for_doc` with
    // `actor_id: None` on that path. The self-assignment guard compares
    // the assignee against `actor_id`, so with `actor_id: None` it can
    // never suppress: this pins that deliberate limitation, not a bug.
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "assigned to myself with no known actor".into(),
                assignee_user_id: Some(alice.id),
                checked: false,
                due_at: None,
            }],
            TaskWrite::Edit(None),
        )
        .await
        .unwrap();

    assert_eq!(
        PgNotificationStore::new(pool)
            .list(alice.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .len(),
        1,
        "actor_id: None can't suppress self-assignment — that's the live-edit path"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_a_task_to_yourself_notifies_nobody() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "mine".into(),
                assignee_user_id: Some(alice.id),
                checked: false,
                due_at: None,
            }],
            TaskWrite::Edit(Some(alice.id)),
        )
        .await
        .unwrap();

    assert!(
        PgNotificationStore::new(pool)
            .list(alice.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assigning_an_already_checked_task_notifies_nobody() {
    // Finding 1a: without a `checked` guard, a checklist item that was
    // completed months ago — and is only unchecked in the sense that it
    // was never notified about before this feature existed — would still
    // fire a `task_assigned` row the first time its document gets
    // reindexed. Landing a task as checked from the start (import, or any
    // path that never went through the unchecked state) must notify
    // nobody, matching a task that's simply done.
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = PgUserStore::new(pool.clone())
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = PgUserStore::new(pool.clone())
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), alice.id)
        .await
        .unwrap();

    PgTaskStore::new(pool.clone())
        .upsert_for_doc(
            ws.id,
            doc.id,
            &[DocTaskInput {
                item_index: 0,
                text: "already done on arrival".into(),
                assignee_user_id: Some(bob.id),
                checked: true,
                due_at: None,
            }],
            TaskWrite::Edit(Some(alice.id)),
        )
        .await
        .unwrap();

    assert!(
        PgNotificationStore::new(pool)
            .list(bob.id, ws.id, &[], false, 50, None)
            .await
            .unwrap()
            .is_empty(),
        "a task that lands checked must not notify its assignee"
    );
}

/// A doc Alice owns, with Bob and Carol as editors, for the `task_assigned`
/// tests below.
struct TaskFixture {
    ws: Uuid,
    doc: Uuid,
    alice: Uuid,
    bob: Uuid,
    carol: Uuid,
    tasks: PgTaskStore,
    notifications: PgNotificationStore,
}

impl TaskFixture {
    /// Index `items` as the doc's tasks, as the live-edit reindex does:
    /// with no known actor.
    async fn write(&self, items: &[DocTaskInput]) {
        self.tasks
            .upsert_for_doc(self.ws, self.doc, items, TaskWrite::Edit(None))
            .await
            .unwrap();
    }

    async fn inbox(&self, user: Uuid) -> Vec<Notification> {
        self.notifications
            .list(user, self.ws, &[], false, 50, None)
            .await
            .unwrap()
    }
}

async fn task_fixture() -> TaskFixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let ws_store = PgWorkspaceStore::new(pool.clone());
    let mut ids = Vec::new();
    for (email, name, role) in [
        ("alice@x.test", "Alice", WorkspaceRole::Owner),
        ("bob@x.test", "Bob", WorkspaceRole::Editor),
        ("carol@x.test", "Carol", WorkspaceRole::Editor),
    ] {
        let user = users.create_local(email, name, "$h$").await.unwrap();
        ws_store.add_member(ws.id, user.id, role).await.unwrap();
        ids.push(user.id);
    }
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sort_key_between(None, None), ids[0])
        .await
        .unwrap();
    TaskFixture {
        ws: ws.id,
        doc: doc.id,
        alice: ids[0],
        bob: ids[1],
        carol: ids[2],
        tasks: PgTaskStore::new(pool.clone()),
        notifications: PgNotificationStore::new(pool),
    }
}

fn task(item_index: i32, text: &str, assignee: Uuid, checked: bool) -> DocTaskInput {
    DocTaskInput {
        item_index,
        text: text.into(),
        assignee_user_id: Some(assignee),
        checked,
        due_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_an_assigned_task_notifies_once_with_the_text_it_ends_up_with() {
    // The reindex runs every 2s while someone types, so the assignee
    // usually lands before the text is finished. Each flush is an edit of
    // the same task, not a new one.
    let f = task_fixture().await;
    for text in [
        "",
        "Rev",
        "Review the",
        "Review the spec",
        "Review the spec.",
    ] {
        f.write(&[task(0, text, f.bob, false)]).await;
    }
    let rows = f.inbox(f.bob).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].data["excerpt"], "Review the spec.");
    assert!(rows[0].read_at.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn rewriting_an_assigned_task_into_another_one_notifies_again() {
    let f = task_fixture().await;
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    f.write(&[task(0, "book the venue for friday", f.bob, false)])
        .await;
    let rows = f.inbox(f.bob).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].data["excerpt"], "book the venue for friday");
    assert_eq!(rows[1].data["excerpt"], "ship it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_added_beside_an_edited_one_still_notifies() {
    let f = task_fixture().await;
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    f.write(&[
        task(0, "book the venue", f.bob, false),
        task(1, "ship it now", f.bob, false),
    ])
    .await;
    let excerpts: Vec<String> = f
        .inbox(f.bob)
        .await
        .iter()
        .map(|n| n.data["excerpt"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(excerpts, ["book the venue", "ship it now"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn reopening_a_completed_task_does_not_notify() {
    let f = task_fixture().await;
    f.write(&[task(0, "ship it", f.bob, true)]).await;
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    assert!(f.inbox(f.bob).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn handing_a_task_to_someone_else_notifies_them() {
    let f = task_fixture().await;
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    f.write(&[task(0, "ship it", f.carol, false)]).await;
    assert_eq!(f.inbox(f.bob).await.len(), 1);
    assert_eq!(f.inbox(f.carol).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_leaves_a_read_notification_as_it_was() {
    let f = task_fixture().await;
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    let id = f.inbox(f.bob).await[0].id;
    f.notifications.mark_read(f.bob, &[id]).await.unwrap();
    f.write(&[task(0, "ship it today", f.bob, false)]).await;
    let rows = f.inbox(f.bob).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].data["excerpt"], "ship it");
    assert!(rows[0].read_at.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_sync_notifies_nobody() {
    // The refresh sweep re-renders pages nobody edited; whatever it finds
    // assigned was assigned by an edit that already had its chance.
    let f = task_fixture().await;
    f.tasks
        .sync_for_doc_quiet(f.ws, f.doc, &[task(0, "ship it", f.bob, false)])
        .await
        .unwrap();
    assert!(f.inbox(f.bob).await.is_empty());
    // Nor does the next edit treat the swept assignment as new.
    f.write(&[task(0, "ship it", f.bob, false)]).await;
    assert!(f.inbox(f.bob).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_actor_on_an_assignment_is_whoever_edited() {
    let f = task_fixture().await;
    f.tasks
        .upsert_for_doc(
            f.ws,
            f.doc,
            &[task(0, "ship it", f.bob, false)],
            TaskWrite::Edit(Some(f.alice)),
        )
        .await
        .unwrap();
    let rows = f.inbox(f.bob).await;
    assert_eq!(rows[0].actor_id, Some(f.alice));
}
