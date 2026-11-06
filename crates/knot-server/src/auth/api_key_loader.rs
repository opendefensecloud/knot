//! Bearer authentication with scoped API keys, for `/api/*` only.
//!
//! Every other path (the `/collab/*` websocket upgrade, `/auth/password`,
//! `/auth/logout`, `/p/*`) stays cookie-only, so a key can never change the
//! account password or edit over a websocket the per-request scope check
//! does not cover.
//!
//! Neither the presented secret nor its hash is ever logged or traced.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header::AUTHORIZATION},
    response::Response,
};
use knot_storage::Scopes;
use uuid::Uuid;

use super::{context::AuthContext, session_loader::SessionDeps};
use crate::http_error::json_err;

/// The API key that authenticated this request. Inserted into request
/// extensions next to [`AuthContext`]; its presence marks the request as
/// key-authenticated (no session).
#[derive(Debug, Clone, Copy)]
pub struct ApiPrincipal {
    pub key_id: Uuid,
    pub scopes: Scopes,
}

const API_PREFIX: &str = "/api/";
const BEARER: &[u8] = b"bearer ";

/// One answer for every way a presented key can be unusable: malformed,
/// unknown, expired, revoked, or its user no longer a member. No oracle.
fn invalid() -> Response {
    json_err(StatusCode::UNAUTHORIZED, "api_key.invalid", "")
}

/// `true` when the header uses the Bearer scheme (case-insensitive per
/// RFC 7235). Checked on raw bytes so a non-ASCII value cannot hide one.
fn is_bearer(v: &HeaderValue) -> bool {
    let b = v.as_bytes();
    b.len() >= BEARER.len() && b[..BEARER.len()].eq_ignore_ascii_case(BEARER)
}

/// Bearer authentication for /api/* only. Ok(None) means "no usable
/// Authorization header, fall through to the cookie path".
///
/// A header with another scheme (e.g. Basic added by a reverse proxy) is not
/// an API-key attempt and also falls through. Once a Bearer header is
/// present, any failure is a 401 `api_key.invalid`; there is never a
/// fallback to the cookie, so a broken key cannot silently downgrade to
/// another identity.
///
/// On success returns the context and the principal (the key is touched
/// fire-and-forget, like a session).
///
/// Not an `async fn`: the header is read synchronously so the returned
/// future does not borrow the request (`Body` is not `Sync`, so holding
/// `&Request<Body>` across an await would make the middleware `!Send`).
pub fn authenticate<'a>(
    deps: &'a SessionDeps,
    req: &Request<Body>,
) -> impl Future<Output = Result<Option<(AuthContext, ApiPrincipal)>, Response>> + Send + 'a {
    let presented = presented_key(deps, req);
    lookup(deps, presented)
}

/// What the request's Authorization header presents.
enum Presented {
    /// Not an API-key attempt: fall through to the cookie.
    Nothing,
    /// A Bearer header that cannot be a key.
    Malformed,
    Key(String),
}

/// The Bearer secret, if this request carries one we must judge.
fn presented_key(deps: &SessionDeps, req: &Request<Body>) -> Presented {
    if deps.api_keys.is_none() || !req.uri().path().starts_with(API_PREFIX) {
        return Presented::Nothing;
    }
    let values: Vec<&HeaderValue> = req.headers().get_all(AUTHORIZATION).iter().collect();
    if !values.iter().any(|v| is_bearer(v)) {
        return Presented::Nothing;
    }
    // A Bearer header next to another Authorization header is ambiguous.
    let [only] = values.as_slice() else {
        return Presented::Malformed;
    };
    let Ok(raw) = only.to_str() else {
        return Presented::Malformed;
    };
    let secret = raw[BEARER.len()..].trim();
    if !knot_auth::api_key::looks_like_api_key(secret) {
        return Presented::Malformed;
    }
    Presented::Key(secret.to_owned())
}

#[tracing::instrument(skip_all, name = "auth.api_key")]
async fn lookup(
    deps: &SessionDeps,
    presented: Presented,
) -> Result<Option<(AuthContext, ApiPrincipal)>, Response> {
    let secret = match presented {
        Presented::Nothing => return Ok(None),
        Presented::Malformed => return Err(invalid()),
        Presented::Key(s) => s,
    };
    let Some(store) = deps.api_keys.as_ref() else {
        return Ok(None);
    };

    // No sleep or throttle on failure: 256 bits of entropy behind a unique
    // index makes guessing pointless.
    let hash = knot_auth::api_key::hash(&secret, &deps.session_key);
    drop(secret);
    let key = match store.find_live_by_hash(&hash).await {
        Ok(Some(k)) => k,
        Ok(None) => return Err(invalid()),
        Err(e) => {
            tracing::error!(error = ?e, "api key lookup failed");
            return Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""));
        }
    };
    // Membership is checked at auth time, so removing a member makes their
    // keys inert immediately.
    let role = match deps
        .workspaces
        .get_member_role(key.workspace_id, key.user_id)
        .await
    {
        Ok(Some(role)) => role,
        Ok(None) => return Err(invalid()),
        Err(e) => {
            tracing::error!(error = ?e, "api key member lookup failed");
            return Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""));
        }
    };

    let store = store.clone();
    let key_id = key.id;
    tokio::spawn(async move {
        if let Err(e) = store.touch(key_id).await {
            tracing::warn!(error = ?e, "api key touch failed");
        }
    });

    Ok(Some((
        AuthContext {
            user_id: key.user_id,
            workspace_id: key.workspace_id,
            role,
        },
        ApiPrincipal {
            key_id,
            scopes: key.scopes,
        },
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::Request;
    use knot_auth::api_key::{ApiKeySecret, hash};
    use knot_storage::{
        ApiKeyStore, NewApiKey, PgApiKeyStore, PgSessionStore, PgUserStore, PgWorkspaceStore,
        Scope, UserStore, WorkspaceRole, WorkspaceStore,
    };

    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    struct Fx {
        deps: SessionDeps,
        keys: Arc<PgApiKeyStore>,
        wss: Arc<PgWorkspaceStore>,
        ws: Uuid,
        user: Uuid,
    }

    async fn setup() -> Fx {
        let pool = knot_test_support::fresh_db().await.pool;
        let wss = Arc::new(PgWorkspaceStore::new(pool.clone()));
        let users = PgUserStore::new(pool.clone());
        let ws = wss.create("acme", "Acme").await.unwrap();
        let user = users.create_local("a@x.test", "A", "$h$").await.unwrap();
        wss.add_member(ws.id, user.id, WorkspaceRole::Editor)
            .await
            .unwrap();
        let keys = Arc::new(PgApiKeyStore::new(pool.clone()));
        Fx {
            deps: SessionDeps {
                sessions: Arc::new(PgSessionStore::new(pool.clone())),
                workspaces: wss.clone(),
                api_keys: Some(keys.clone()),
                session_key: KEY.to_vec(),
            },
            keys,
            wss,
            ws: ws.id,
            user: user.id,
        }
    }

    async fn mint(fx: &Fx) -> (ApiKeySecret, Uuid) {
        let secret = ApiKeySecret::generate();
        let scopes = Scopes::from_names(&["read".to_string()]).unwrap();
        let k = fx
            .keys
            .create(NewApiKey {
                workspace_id: fx.ws,
                user_id: fx.user,
                name: "ci",
                key_hash: &hash(secret.as_str(), KEY),
                scopes: &scopes,
                created_by: fx.user,
                expires_at: None,
                session_hash: None,
            })
            .await
            .unwrap();
        (secret, k.id)
    }

    fn req(path: &str, auth: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(path);
        if let Some(a) = auth {
            b = b.header(AUTHORIZATION, a);
        }
        b.body(Body::empty()).unwrap()
    }

    async fn assert_invalid(deps: &SessionDeps, r: Request<Body>) {
        let resp = authenticate(deps, &r).await.expect_err("refused");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "api_key.invalid");
    }

    #[tokio::test]
    async fn valid_bearer_key_authenticates_api_request_with_member_role() {
        let fx = setup().await;
        let (secret, id) = mint(&fx).await;
        let r = req("/api/docs", Some(&format!("Bearer {}", secret.as_str())));
        let (ctx, p) = authenticate(&fx.deps, &r).await.unwrap().unwrap();
        assert_eq!((ctx.user_id, ctx.workspace_id), (fx.user, fx.ws));
        assert_eq!(ctx.role, WorkspaceRole::Editor);
        assert_eq!(p.key_id, id);
        assert!(p.scopes.contains(Scope::Read) && !p.scopes.contains(Scope::Create));
    }

    #[tokio::test]
    async fn key_is_ignored_outside_api_paths() {
        let fx = setup().await;
        let (secret, _) = mint(&fx).await;
        let auth = format!("Bearer {}", secret.as_str());
        for path in [
            "/collab/doc/00000000-0000-0000-0000-000000000000",
            "/auth/password",
            "/auth/logout",
            "/p/sometoken",
            "/api",
        ] {
            let r = req(path, Some(&auth));
            assert!(
                authenticate(&fx.deps, &r).await.unwrap().is_none(),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn missing_or_non_bearer_header_falls_through_to_cookie() {
        let fx = setup().await;
        assert!(
            authenticate(&fx.deps, &req("/api/docs", None))
                .await
                .unwrap()
                .is_none()
        );
        let basic = req("/api/docs", Some("Basic dXNlcjpwYXNz"));
        assert!(authenticate(&fx.deps, &basic).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_unknown_revoked_and_removed_member_keys_all_get_the_same_401() {
        let fx = setup().await;
        assert_invalid(&fx.deps, req("/api/docs", Some("Bearer nope"))).await;
        assert_invalid(&fx.deps, req("/api/docs", Some("Bearer "))).await;
        let unknown = ApiKeySecret::generate();
        let auth = format!("Bearer {}", unknown.as_str());
        assert_invalid(&fx.deps, req("/api/docs", Some(&auth))).await;

        let (secret, id) = mint(&fx).await;
        let auth = format!("Bearer {}", secret.as_str());
        fx.keys.revoke(fx.ws, fx.user, id).await.unwrap();
        assert_invalid(&fx.deps, req("/api/docs", Some(&auth))).await;

        let (secret, _) = mint(&fx).await;
        let auth = format!("Bearer {}", secret.as_str());
        fx.wss.remove_member(fx.ws, fx.user).await.unwrap();
        assert_invalid(&fx.deps, req("/api/docs", Some(&auth))).await;
    }

    #[tokio::test]
    async fn bearer_next_to_another_authorization_header_is_refused() {
        let fx = setup().await;
        let (secret, _) = mint(&fx).await;
        let r = Request::builder()
            .uri("/api/docs")
            .header(AUTHORIZATION, format!("Bearer {}", secret.as_str()))
            .header(AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(Body::empty())
            .unwrap();
        assert_invalid(&fx.deps, r).await;
    }

    /// The membership check at auth time is a layer of its own: a live key
    /// whose user is no member (a missed trigger, a restore, a direct
    /// write) is refused, not authenticated with some default role.
    #[tokio::test]
    async fn live_key_of_a_user_who_is_not_a_member_is_refused() {
        let pool = knot_test_support::fresh_db().await.pool;
        let wss = Arc::new(PgWorkspaceStore::new(pool.clone()));
        let ws = wss.create("acme", "Acme").await.unwrap();
        let outsider = PgUserStore::new(pool.clone())
            .create_local("o@x.test", "O", "$h$")
            .await
            .unwrap();
        let keys = Arc::new(PgApiKeyStore::new(pool.clone()));
        let deps = SessionDeps {
            sessions: Arc::new(PgSessionStore::new(pool.clone())),
            workspaces: wss,
            api_keys: Some(keys.clone()),
            session_key: KEY.to_vec(),
        };
        let secret = ApiKeySecret::generate();
        // Written directly: the store refuses to mint for a non-member.
        sqlx::query(
            "INSERT INTO api_keys (workspace_id, user_id, name, key_hash, scopes, created_by)
             VALUES ($1, $2, 'orphan', $3, ARRAY['read'], $2)",
        )
        .bind(ws.id)
        .bind(outsider.id)
        .bind(hash(secret.as_str(), KEY))
        .execute(&pool)
        .await
        .unwrap();
        let auth = format!("Bearer {}", secret.as_str());
        assert!(
            keys.find_live_by_hash(&hash(secret.as_str(), KEY))
                .await
                .unwrap()
                .is_some(),
            "the key itself is live"
        );
        assert_invalid(&deps, req("/api/docs", Some(&auth))).await;
    }

    #[tokio::test]
    async fn no_api_key_store_means_cookie_only() {
        let mut fx = setup().await;
        let (secret, _) = mint(&fx).await;
        fx.deps.api_keys = None;
        let r = req("/api/docs", Some(&format!("Bearer {}", secret.as_str())));
        assert!(authenticate(&fx.deps, &r).await.unwrap().is_none());
    }
}
