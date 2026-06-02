use knot_storage::{PgUserStore, UserStore, UserStoreError};

async fn fresh_store() -> PgUserStore {
    PgUserStore::new(knot_test_support::fresh_db().await.pool)
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
