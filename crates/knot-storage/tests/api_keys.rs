//! Integration tests for ApiKeyStore against an ephemeral Postgres.

use chrono::{Duration, Utc};
use knot_storage::{
    ApiKey, ApiKeyStore, ApiKeyStoreError, NewApiKey, PgApiKeyStore, PgSessionStore, PgUserStore,
    PgWorkspaceStore, Scope, Scopes, SessionStore, UserStore, WorkspaceRole, WorkspaceStore,
};
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    pool: PgPool,
    store: PgApiKeyStore,
    ws: Uuid,
    alice: Uuid,
    bob: Uuid,
}

async fn setup() -> Fx {
    let pool = knot_test_support::fresh_db().await.pool;
    let wss = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());
    let ws = wss.create("acme", "Acme").await.unwrap();
    let alice = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    let bob = users.create_local("b@x.test", "B", "$h$").await.unwrap();
    wss.add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    wss.add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    Fx {
        store: PgApiKeyStore::new(pool.clone()),
        pool,
        ws: ws.id,
        alice: alice.id,
        bob: bob.id,
    }
}

fn scopes(names: &[&str]) -> Scopes {
    let v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    Scopes::from_names(&v).unwrap()
}

/// A key `user_id` mints for itself.
async fn mint(
    fx: &Fx,
    user_id: Uuid,
    name: &str,
    hash: &[u8],
    expires_at: Option<chrono::DateTime<Utc>>,
) -> Result<ApiKey, ApiKeyStoreError> {
    mint_by(fx, user_id, user_id, name, hash, expires_at, None).await
}

async fn mint_by(
    fx: &Fx,
    user_id: Uuid,
    created_by: Uuid,
    name: &str,
    hash: &[u8],
    expires_at: Option<chrono::DateTime<Utc>>,
    session_hash: Option<&[u8]>,
) -> Result<ApiKey, ApiKeyStoreError> {
    let s = scopes(&["read", "update"]);
    fx.store
        .create(NewApiKey {
            workspace_id: fx.ws,
            user_id,
            name,
            key_hash: hash,
            scopes: &s,
            created_by,
            expires_at,
            session_hash,
        })
        .await
}

/// A bot member of the fixture's workspace.
async fn add_bot(fx: &Fx, email: &str) -> Uuid {
    let bot: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name, kind) VALUES ($1, 'Bot', 'bot') RETURNING id",
    )
    .bind(email)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    PgWorkspaceStore::new(fx.pool.clone())
        .add_member(fx.ws, bot, WorkspaceRole::Editor)
        .await
        .unwrap();
    bot
}

#[tokio::test(flavor = "multi_thread")]
async fn create_then_list_round_trips_without_hash_leak() {
    let fx = setup().await;
    let exp = Utc::now() + Duration::days(60);
    let first = mint(&fx, fx.alice, "ci", &[1u8; 32], Some(exp))
        .await
        .unwrap();
    let second = mint(&fx, fx.alice, "agent", &[2u8; 32], None)
        .await
        .unwrap();
    assert_eq!(first.name, "ci");
    assert_eq!(first.user_id, fx.alice);
    assert_eq!(first.workspace_id, fx.ws);
    assert!(first.scopes.contains(Scope::Read));
    assert!(first.scopes.contains(Scope::Update));
    assert!(!first.scopes.contains(Scope::Create));
    assert!(!first.scopes.contains(Scope::Delete));
    assert_eq!(first.scopes.names(), vec!["read", "update"]);
    assert!(first.revoked_at.is_none() && first.last_used_at.is_none());
    // Postgres stores microseconds; compare at that precision.
    assert_eq!(
        first.expires_at.unwrap().timestamp_micros(),
        exp.timestamp_micros()
    );

    let listed = fx.store.list_for_user(fx.ws, fx.alice).await.unwrap();
    // Newest first.
    assert_eq!(listed, vec![second.clone(), first.clone()]);
    assert!(
        fx.store
            .list_for_user(fx.ws, fx.bob)
            .await
            .unwrap()
            .is_empty()
    );

    // Exhaustive destructure: adding a hash field to ApiKey breaks this.
    let ApiKey {
        id: _,
        workspace_id: _,
        user_id: _,
        name: _,
        scopes: _,
        created_by,
        created_at: _,
        expires_at: _,
        last_used_at: _,
        revoked_at: _,
    } = first;
    assert_eq!(created_by, fx.alice);
    // And the hash bytes never appear in the Debug rendering of a row.
    let dbg = format!("{listed:?}");
    assert!(!dbg.contains("key_hash"));
}

#[tokio::test(flavor = "multi_thread")]
async fn find_live_by_hash_ignores_revoked_expired_and_unknown() {
    let fx = setup().await;
    let live = mint(&fx, fx.alice, "live", &[1u8; 32], None).await.unwrap();
    let revoked = mint(&fx, fx.alice, "revoked", &[2u8; 32], None)
        .await
        .unwrap();
    let expired = mint(&fx, fx.alice, "expired", &[3u8; 32], None)
        .await
        .unwrap();

    assert!(fx.store.revoke(fx.ws, fx.alice, revoked.id).await.unwrap());
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(expired.id)
        .execute(&fx.pool)
        .await
        .unwrap();

    let found = fx.store.find_live_by_hash(&[1u8; 32]).await.unwrap();
    assert_eq!(found.map(|k| k.id), Some(live.id));
    assert!(
        fx.store
            .find_live_by_hash(&[2u8; 32])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.store
            .find_live_by_hash(&[3u8; 32])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.store
            .find_live_by_hash(&[9u8; 32])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_is_scoped_to_the_owner() {
    let fx = setup().await;
    let k = mint(&fx, fx.alice, "mine", &[1u8; 32], None).await.unwrap();

    assert!(!fx.store.revoke(fx.ws, fx.bob, k.id).await.unwrap());
    assert!(
        !fx.store
            .revoke(fx.ws, fx.alice, Uuid::new_v4())
            .await
            .unwrap()
    );
    assert!(
        fx.store
            .find_live_by_hash(&[1u8; 32])
            .await
            .unwrap()
            .is_some()
    );

    assert!(fx.store.revoke(fx.ws, fx.alice, k.id).await.unwrap());
    // Revoking twice is not a second success.
    assert!(!fx.store.revoke(fx.ws, fx.alice, k.id).await.unwrap());
    assert!(
        fx.store
            .find_live_by_hash(&[1u8; 32])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_sets_last_used_at() {
    let fx = setup().await;
    let k = mint(&fx, fx.alice, "k", &[1u8; 32], None).await.unwrap();
    assert!(k.last_used_at.is_none());

    fx.store.touch(k.id).await.unwrap();
    let after = fx
        .store
        .find_live_by_hash(&[1u8; 32])
        .await
        .unwrap()
        .unwrap();
    let used = after.last_used_at.expect("last_used_at set");
    assert!(used >= k.created_at);

    // Debounced: a second touch within the minute leaves the row alone.
    fx.store.touch(k.id).await.unwrap();
    let again = fx
        .store
        .find_live_by_hash(&[1u8; 32])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.last_used_at, Some(used));

    // Unknown id is a no-op, not an error.
    fx.store.touch(Uuid::new_v4()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sso_user_ttl_rejection_maps_to_ttl_rejected() {
    let fx = setup().await;
    let sso = PgUserStore::new(fx.pool.clone())
        .create_oidc("sso@x.test", "Sso", "https://idp.test", "sub-1")
        .await
        .unwrap();
    PgWorkspaceStore::new(fx.pool.clone())
        .add_member(fx.ws, sso.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let bot = add_bot(&fx, "bot@x.test").await;

    let err = mint(&fx, sso.id, "forever", &[1u8; 32], None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::TtlRejected), "{err:?}");

    let err = mint(
        &fx,
        sso.id,
        "too-long",
        &[2u8; 32],
        Some(Utc::now() + Duration::days(91)),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::TtlRejected), "{err:?}");

    // Within the cap the OIDC user is fine.
    mint(
        &fx,
        sso.id,
        "ok",
        &[3u8; 32],
        Some(Utc::now() + Duration::days(60)),
    )
    .await
    .unwrap();

    // Bots have no credentials, so the OIDC rule never applies to them.
    let k = mint_by(&fx, bot, fx.alice, "bot-forever", &[4u8; 32], None, None)
        .await
        .unwrap();
    assert!(k.expires_at.is_none());

    // Other constraint violations are not TtlRejected (duplicate hash).
    let err = mint(&fx, fx.alice, "dup", &[4u8; 32], None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::Sqlx(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_and_revoke_are_scoped_to_the_workspace() {
    let fx = setup().await;
    let k = mint(&fx, fx.alice, "mine", &[1u8; 32], None).await.unwrap();
    let other_ws = PgWorkspaceStore::new(fx.pool.clone())
        .create("other", "Other")
        .await
        .unwrap()
        .id;
    assert!(
        fx.store
            .list_for_user(other_ws, fx.alice)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!fx.store.revoke(other_ws, fx.alice, k.id).await.unwrap());
    assert_eq!(
        fx.store.list_for_user(fx.ws, fx.alice).await.unwrap(),
        vec![k]
    );
}

/// The list shows live keys and those revoked or expired in the last 30
/// days, newest first; older dead keys drop out.
#[tokio::test(flavor = "multi_thread")]
async fn list_shows_live_and_recently_ended_keys_newest_first() {
    let fx = setup().await;
    let mut ids = Vec::new();
    for (i, name) in [
        "live",
        "revoked-recently",
        "expired-recently",
        "revoked-long-ago",
        "expired-long-ago",
        "revoked-recently-expired-long-ago",
    ]
    .iter()
    .enumerate()
    {
        let k = mint(&fx, fx.alice, name, &[i as u8 + 1; 32], None)
            .await
            .unwrap();
        // Distinct, ascending creation times: the last minted is newest.
        sqlx::query(
            "UPDATE api_keys SET created_at = now() - make_interval(hours => $2) WHERE id = $1",
        )
        .bind(k.id)
        .bind(100 - i as i32)
        .execute(&fx.pool)
        .await
        .unwrap();
        ids.push(k.id);
    }
    let set = |id: Uuid, sql: &'static str| {
        let pool = fx.pool.clone();
        async move {
            sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
        }
    };
    set(
        ids[1],
        "UPDATE api_keys SET revoked_at = now() - interval '29 days' WHERE id = $1",
    )
    .await;
    set(
        ids[2],
        "UPDATE api_keys SET expires_at = now() - interval '29 days' WHERE id = $1",
    )
    .await;
    set(
        ids[3],
        "UPDATE api_keys SET revoked_at = now() - interval '31 days' WHERE id = $1",
    )
    .await;
    set(
        ids[4],
        "UPDATE api_keys SET expires_at = now() - interval '31 days' WHERE id = $1",
    )
    .await;
    set(
        ids[5],
        "UPDATE api_keys SET revoked_at = now() - interval '1 day',
                             expires_at = now() - interval '31 days' WHERE id = $1",
    )
    .await;

    let listed: Vec<Uuid> = fx
        .store
        .list_for_user(fx.ws, fx.alice)
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.id)
        .collect();
    assert_eq!(listed, vec![ids[2], ids[1], ids[0]]);
}

/// At most 50 live keys per user; revoked and expired keys don't count.
#[tokio::test(flavor = "multi_thread")]
async fn live_keys_are_capped_per_user() {
    let fx = setup().await;
    assert_eq!(knot_storage::api_key_store::MAX_LIVE_KEYS_PER_USER, 50);
    let mut first = None;
    for i in 0..50u8 {
        let k = mint(&fx, fx.bob, "k", &[i; 32], None).await.unwrap();
        first.get_or_insert(k.id);
    }
    let err = mint(&fx, fx.bob, "k", &[200u8; 32], None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::LimitReached), "{err:?}");
    // Another user's allowance is separate.
    mint(&fx, fx.alice, "k", &[201u8; 32], None).await.unwrap();

    // A revoked key frees a slot, an expired one too.
    assert!(
        fx.store
            .revoke(fx.ws, fx.bob, first.unwrap())
            .await
            .unwrap()
    );
    let k = mint(&fx, fx.bob, "k", &[202u8; 32], None).await.unwrap();
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(k.id)
        .execute(&fx.pool)
        .await
        .unwrap();
    mint(&fx, fx.bob, "k", &[203u8; 32], None).await.unwrap();
    let err = mint(&fx, fx.bob, "k", &[204u8; 32], None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::LimitReached), "{err:?}");
}

/// The insert re-checks, in its transaction, everything that authorized it.
#[tokio::test(flavor = "multi_thread")]
async fn create_rechecks_session_membership_and_ownership() {
    let fx = setup().await;
    let sessions = PgSessionStore::new(fx.pool.clone());
    let exp = Utc::now() + Duration::days(1);
    sessions
        .create(&[7u8; 32], fx.bob, fx.ws, exp, None, None)
        .await
        .unwrap();

    // A live session of the creator: fine.
    mint_by(
        &fx,
        fx.bob,
        fx.bob,
        "ok",
        &[1u8; 32],
        None,
        Some(&[7u8; 32]),
    )
    .await
    .unwrap();
    // Unknown, someone else's, or expired session: refused.
    for (who, sid) in [(fx.bob, [8u8; 32]), (fx.alice, [7u8; 32])] {
        let err = mint_by(&fx, who, who, "k", &[2u8; 32], None, Some(&sid))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiKeyStoreError::SessionEnded), "{err:?}");
    }
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&fx.pool)
        .await
        .unwrap();
    let err = mint_by(&fx, fx.bob, fx.bob, "k", &[2u8; 32], None, Some(&[7u8; 32]))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::SessionEnded), "{err:?}");

    // A creator who is not a member.
    let outsider = PgUserStore::new(fx.pool.clone())
        .create_local("o@x.test", "O", "$h$")
        .await
        .unwrap()
        .id;
    let err = mint(&fx, outsider, "k", &[3u8; 32], None)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::SessionEnded), "{err:?}");

    // For someone else: only an owner, only for a bot member.
    let bot = add_bot(&fx, "bot@x.test").await;
    mint_by(&fx, bot, fx.alice, "ok", &[4u8; 32], None, None)
        .await
        .unwrap();
    let foreign_bot: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name, kind) VALUES ('fb@x.test', 'FB', 'bot') RETURNING id",
    )
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    for (user, by, why) in [
        (bot, fx.bob, "an editor minting for a bot"),
        (fx.bob, fx.alice, "an owner minting for a human"),
        (
            foreign_bot,
            fx.alice,
            "an owner minting for a non-member bot",
        ),
    ] {
        let err = mint_by(&fx, user, by, "k", &[5u8; 32], None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApiKeyStoreError::NotPermitted),
            "{why}: {err:?}"
        );
    }
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(live, 2, "only the two permitted mints wrote a row");
}

/// Wait until `task` has blocked for a while, proving it waits on a lock.
async fn assert_blocked<T>(task: &tokio::task::JoinHandle<T>) {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the mint did not wait for the lock");
}

/// S8: a removal in flight (row deleted, not yet committed) makes the mint
/// wait for it, then refuse. Without the share lock the mint would read the
/// old membership, insert after the removal's trigger ran, and keep a key.
#[tokio::test(flavor = "multi_thread")]
async fn mint_waits_for_an_inflight_removal_and_is_refused() {
    let fx = setup().await;
    let bot = add_bot(&fx, "bot@x.test").await;
    let mut removal = knot_storage::begin(&fx.pool).await.unwrap();
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(fx.ws)
        .bind(fx.alice)
        .execute(&mut *removal)
        .await
        .unwrap();

    let store = fx.store.clone();
    let ws = fx.ws;
    let alice = fx.alice;
    let task = tokio::spawn(async move {
        let s = scopes(&["read"]);
        store
            .create(NewApiKey {
                workspace_id: ws,
                user_id: bot,
                name: "racing",
                key_hash: &[9u8; 32],
                scopes: &s,
                created_by: alice,
                expires_at: None,
                session_hash: None,
            })
            .await
    });
    assert_blocked(&task).await;
    removal.commit().await.unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::SessionEnded), "{err:?}");
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE revoked_at IS NULL")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(live, 0);
}

/// S5: a password change in flight (hash updated and sessions deleted, not
/// yet committed) makes the mint wait for it, then find its session gone.
/// Without the user-row lock the mint would read the old session, insert
/// after the revoking trigger ran, and keep a key.
#[tokio::test(flavor = "multi_thread")]
async fn mint_waits_for_an_inflight_password_change_and_is_refused() {
    let fx = setup().await;
    PgSessionStore::new(fx.pool.clone())
        .create(
            &[7u8; 32],
            fx.bob,
            fx.ws,
            Utc::now() + Duration::days(1),
            None,
            None,
        )
        .await
        .unwrap();
    let mut change = knot_storage::begin(&fx.pool).await.unwrap();
    sqlx::query("UPDATE users SET password_hash = 'new' WHERE id = $1")
        .bind(fx.bob)
        .execute(&mut *change)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(fx.bob)
        .execute(&mut *change)
        .await
        .unwrap();

    let store = fx.store.clone();
    let ws = fx.ws;
    let bob = fx.bob;
    let task = tokio::spawn(async move {
        let s = scopes(&["read"]);
        store
            .create(NewApiKey {
                workspace_id: ws,
                user_id: bob,
                name: "racing",
                key_hash: &[9u8; 32],
                scopes: &s,
                created_by: bob,
                expires_at: None,
                session_hash: Some(&[7u8; 32]),
            })
            .await
    });
    assert_blocked(&task).await;
    change.commit().await.unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::SessionEnded), "{err:?}");
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE revoked_at IS NULL")
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(live, 0);
}

/// The other order: a mint holding its locks makes the removal wait, and
/// the removal's trigger then revokes the committed key.
#[tokio::test(flavor = "multi_thread")]
async fn removal_waits_for_an_inflight_mint_and_revokes_its_key() {
    let fx = setup().await;
    let bot = add_bot(&fx, "bot@x.test").await;
    // Stand-in for the mint's transaction after its checks: the same locks.
    let mut mint = knot_storage::begin(&fx.pool).await.unwrap();
    sqlx::query(
        "SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = ANY($2) FOR SHARE",
    )
    .bind(fx.ws)
    .bind([fx.alice, bot].as_slice())
    .execute(&mut *mint)
    .await
    .unwrap();
    let key: Uuid = sqlx::query_scalar(
        "INSERT INTO api_keys (workspace_id, user_id, name, key_hash, scopes, created_by)
         VALUES ($1, $2, 'k', '\\x01', ARRAY['read'], $3) RETURNING id",
    )
    .bind(fx.ws)
    .bind(bot)
    .bind(fx.alice)
    .fetch_one(&mut *mint)
    .await
    .unwrap();

    let wss = PgWorkspaceStore::new(fx.pool.clone());
    let (ws, alice) = (fx.ws, fx.alice);
    let removal = tokio::spawn(async move { wss.remove_member(ws, alice).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !removal.is_finished(),
        "the removal did not wait for the mint"
    );
    mint.commit().await.unwrap();
    removal.await.unwrap().unwrap();
    let revoked: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = $1")
            .bind(key)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(revoked.is_some(), "the key minted under the lock survived");
}
