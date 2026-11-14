use knot_storage::user_store::UserKind;
use knot_storage::{
    PgUserStore, PgWorkspaceStore, UserStore, UserStoreError, WorkspaceRole, WorkspaceStore,
};

async fn fresh_store() -> PgUserStore {
    PgUserStore::new(knot_test_support::fresh_db().await.pool)
}

/// Catches the user insert committing apart from the membership insert: the
/// failed invite would leave a user no workspace lists, and its email taken.
/// The two failures are real ones: the KB001 trigger and the workspace FK.
#[tokio::test(flavor = "multi_thread")]
async fn create_bot_member_leaves_no_user_when_the_membership_insert_fails() {
    let pool = knot_test_support::fresh_db().await.pool;
    let users = PgUserStore::new(pool.clone());
    let wss = PgWorkspaceStore::new(pool);
    let ws = wss.create("acme", "Acme").await.unwrap();
    let email = "bot@example.com";

    users
        .create_bot_member(ws.id, email, "Bot", WorkspaceRole::Owner)
        .await
        .expect_err("KB001 refuses a bot owner");
    users
        .create_bot_member(uuid::Uuid::new_v4(), email, "Bot", WorkspaceRole::Editor)
        .await
        .expect_err("no such workspace");
    assert_eq!(users.find_by_email(email).await.unwrap(), None);
    assert_eq!(users.count().await.unwrap(), 0);

    // The email is not burned: the retry succeeds, as a bot member.
    let bot = users
        .create_bot_member(ws.id, email, "Bot", WorkspaceRole::Viewer)
        .await
        .unwrap();
    assert_eq!(bot.kind, UserKind::Bot);
    assert_eq!(
        (bot.email.as_str(), bot.display_name.as_str()),
        (email, "Bot")
    );
    assert_eq!(bot.password_hash, None);
    assert_eq!(
        wss.get_member_role(ws.id, bot.id).await.unwrap(),
        Some(WorkspaceRole::Viewer)
    );
}

/// Catches the email conflict losing its variant inside the transaction: the
/// handler answers 409 `workspace.email_taken` from it.
#[tokio::test(flavor = "multi_thread")]
async fn create_bot_member_reports_a_taken_email() {
    let pool = knot_test_support::fresh_db().await.pool;
    let users = PgUserStore::new(pool.clone());
    let wss = PgWorkspaceStore::new(pool);
    let ws = wss.create("acme", "Acme").await.unwrap();
    users
        .create_local("taken@example.com", "T", "$h$")
        .await
        .unwrap();

    let err = users
        .create_bot_member(ws.id, "TAKEN@example.com", "Bot", WorkspaceRole::Editor)
        .await
        .expect_err("citext email already exists");
    assert!(matches!(err, UserStoreError::EmailExists), "{err:?}");
    assert_eq!(users.count().await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn local_user_lifecycle() {
    let s = fresh_store().await;
    assert_eq!(s.count().await.unwrap(), 0);

    let u = s
        .create_local("alice@example.com", "Alice", "$argon2id$dummy")
        .await
        .expect("create");
    assert_eq!(u.email, "alice@example.com");
    assert_eq!(u.display_name, "Alice");
    assert!(u.password_hash.is_some());

    // citext: lookup is case-insensitive.
    let found = s.find_by_email("ALICE@example.com").await.unwrap();
    assert_eq!(found.map(|f| f.id), Some(u.id));

    // find_by_id works.
    let by_id = s.find_by_id(u.id).await.unwrap();
    assert_eq!(
        by_id.map(|f| f.email),
        Some("alice@example.com".to_string())
    );

    assert_eq!(s.count().await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_email_rejected() {
    let s = fresh_store().await;
    s.create_local("a@x.test", "A", "$h$").await.unwrap();
    let err = s
        .create_local("a@x.test", "A2", "$h$")
        .await
        .expect_err("must fail");
    assert!(matches!(err, UserStoreError::EmailExists), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oidc_user_lookup() {
    let s = fresh_store().await;
    let u = s
        .create_oidc("alice@example.com", "Alice", "http://dex/dex", "08a86")
        .await
        .unwrap();
    assert!(
        u.password_hash.is_none(),
        "OIDC users must have NULL password_hash"
    );
    let found = s.find_by_oidc("http://dex/dex", "08a86").await.unwrap();
    let found_user = found.expect("oidc user found");
    assert!(
        found_user.password_hash.is_none(),
        "read-back must preserve NULL"
    );
    assert_eq!(found_user.oidc_issuer.as_deref(), Some("http://dex/dex"));
    assert_eq!(found_user.oidc_subject.as_deref(), Some("08a86"));
    let found = Some(found_user);
    assert_eq!(found.map(|f| f.id), Some(u.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_oidc_rejected() {
    let s = fresh_store().await;
    s.create_oidc("a@x.test", "A", "iss", "sub").await.unwrap();
    let err = s
        .create_oidc("b@x.test", "B", "iss", "sub")
        .await
        .expect_err("must fail");
    assert!(matches!(err, UserStoreError::OidcExists), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_oidc_email_collision_reports_email_exists() {
    // Regression: create_oidc with an email that's already taken by a local
    // user must report EmailExists, not OidcExists.
    let s = fresh_store().await;
    s.create_local("clash@x.test", "Local", "$h$")
        .await
        .unwrap();
    let err = s
        .create_oidc("clash@x.test", "OIDC", "iss", "sub")
        .await
        .expect_err("must fail");
    assert!(
        matches!(err, UserStoreError::EmailExists),
        "got {err:?} — expected EmailExists because email constraint wins"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn last_seen_version_starts_null_and_is_compare_and_set() {
    let s = fresh_store().await;
    let u = s.create_local("a@x.test", "A", "$h$").await.unwrap();
    // Users created after the migration have seen nothing yet.
    assert_eq!(s.last_seen_version(u.id).await.unwrap(), None);

    assert!(
        s.set_last_seen_version(u.id, None, "0.8.1").await.unwrap(),
        "NULL -> 0.8.1 must be written"
    );
    assert_eq!(
        s.last_seen_version(u.id).await.unwrap().as_deref(),
        Some("0.8.1")
    );

    // A writer that read a stale value loses: nothing is overwritten.
    assert!(!s.set_last_seen_version(u.id, None, "0.9.0").await.unwrap());
    assert!(
        !s.set_last_seen_version(u.id, Some("0.8.0"), "0.9.0")
            .await
            .unwrap()
    );
    assert_eq!(
        s.last_seen_version(u.id).await.unwrap().as_deref(),
        Some("0.8.1")
    );

    assert!(
        s.set_last_seen_version(u.id, Some("0.8.1"), "0.9.0")
            .await
            .unwrap()
    );
    assert_eq!(
        s.last_seen_version(u.id).await.unwrap().as_deref(),
        Some("0.9.0")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn last_seen_version_of_unknown_user_is_none() {
    let s = fresh_store().await;
    let ghost = uuid::Uuid::new_v4();
    assert_eq!(s.last_seen_version(ghost).await.unwrap(), None);
    assert!(!s.set_last_seen_version(ghost, None, "0.8.1").await.unwrap());
}
