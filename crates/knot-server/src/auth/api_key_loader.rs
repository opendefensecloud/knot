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
use chrono::{DateTime, Duration, Utc};
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
const BEARER: &[u8] = b"bearer";

/// One answer for every way a presented key can be unusable: malformed,
/// unknown, expired, revoked, or its user no longer a member. No oracle.
fn invalid() -> Response {
    json_err(StatusCode::UNAUTHORIZED, "api_key.invalid", "")
}

/// `true` when the header is an attempt to use a knot API key: the Bearer
/// scheme (case-insensitive per RFC 7235) with credentials that are empty or
/// hold a token starting with the key prefix (`knot_`, in any case). The HTTP
/// parser trims trailing whitespace, so a script with an unset key sends a
/// bare `Bearer`, and that is an attempt too. So is a key mangled on its way
/// into the header: tokens are split on whitespace and commas, and leading
/// quotes and non-ASCII bytes (a pasted zero-width space) are skipped, so
/// `Bearer Bearer knot_…`, a quoted key or two joined values still count.
/// Any other Bearer token is not ours: an auth proxy in front of knot
/// (oauth2-proxy, an Istio or Envoy JWT gateway) may forward its own JWT with
/// every request, and knot ignored the header before API keys existed.
/// Checked on raw bytes, since the value need not be valid UTF-8.
fn is_key_attempt(v: &HeaderValue) -> bool {
    let Some((scheme, rest)) = v.as_bytes().split_at_checked(BEARER.len()) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case(BEARER) || !matches!(rest.first(), None | Some(b' ' | b'\t')) {
        return false;
    }
    let prefix = knot_auth::api_key::KEY_PREFIX.as_bytes();
    let credentials = rest.trim_ascii();
    credentials.is_empty()
        || credentials
            .split(|byte| byte.is_ascii_whitespace() || *byte == b',')
            .map(|token| {
                let start = token
                    .iter()
                    .position(|byte| byte.is_ascii() && !matches!(byte, b'"' | b'\''))
                    .unwrap_or(token.len());
                &token[start..]
            })
            .any(|token| {
                token
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            })
}

/// Whether a request should spend a task and a pool connection on a touch.
/// Saves the round trip the SQL debounce in `ApiKeyStore::touch` would answer
/// with no update. `last_used_at` is the database clock and `now` this
/// server's: a value ahead of `now` is due, so skew never stops touches, and
/// the SQL debounce still decides.
fn touch_is_due(last_used_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_used_at.is_none_or(|t| !(Duration::zero()..Duration::minutes(1)).contains(&(now - t)))
}

/// Bearer authentication for /api/* only. Ok(None) means "no usable
/// Authorization header, fall through to the cookie path".
///
/// A header with another scheme (e.g. Basic added by a reverse proxy), or a
/// Bearer token that isn't a knot key (a proxy's JWT), is not an API-key
/// attempt and also falls through. Once a key attempt is present (`Bearer
/// knot_…`, or a bare `Bearer`), any failure is a 401 `api_key.invalid`;
/// there is never a fallback to the cookie, so a broken key cannot silently
/// downgrade to another identity.
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
    if !values.iter().any(|v| is_key_attempt(v)) {
        return Presented::Nothing;
    }
    // A key next to another Authorization header is ambiguous.
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
        Ok(None) => {
            tracing::warn!(
                key_id = %key.id,
                user_id = %key.user_id,
                "live api key of a non-member refused"
            );
            return Err(invalid());
        }
        Err(e) => {
            tracing::error!(error = ?e, "api key member lookup failed");
            return Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""));
        }
    };

    let key_id = key.id;
    if touch_is_due(key.last_used_at, Utc::now()) {
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(e) = store.touch(key_id).await {
                tracing::warn!(error = ?e, %key_id, "api key touch failed");
            }
        });
    }

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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::http::Request;
    use knot_auth::api_key::{ApiKeySecret, hash};
    use knot_storage::{
        ApiKey, ApiKeyStore, ApiKeyStoreError, NewApiKey, PgApiKeyStore, PgSessionStore,
        PgUserStore, PgWorkspaceStore, Scope, UserStore, WorkspaceRole, WorkspaceStore,
    };

    use super::*;
    use crate::auth::log_capture;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    struct Fx {
        pool: sqlx::PgPool,
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
            pool: pool.clone(),
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
        assert_invalid(&fx.deps, req("/api/docs", Some("Bearer knot_nope"))).await;
        assert_invalid(&fx.deps, req("/api/docs", Some("Bearer KNOT_nope"))).await;
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

    /// S9: HTTP parsers trim trailing whitespace, so an unset key reaches us
    /// as a bare `Bearer`, and that is still an attempt to use a key.
    #[tokio::test]
    async fn a_bare_or_tab_separated_bearer_scheme_is_judged_not_passed_to_the_cookie() {
        let fx = setup().await;
        for value in ["Bearer", "bearer", "BEARER", "Bearer\tknot_nope"] {
            assert_invalid(&fx.deps, req("/api/docs", Some(value))).await;
        }
    }

    /// A Bearer token that isn't a knot key, such as the JWT an auth proxy
    /// (oauth2-proxy, an Istio or Envoy JWT gateway) forwards with every
    /// request, is not an API-key attempt: before API keys knot ignored it
    /// and served the cookie, and it still does. Without this, putting knot
    /// behind such a proxy would fail every /api call, health checks too.
    #[tokio::test]
    async fn a_bearer_token_that_is_not_a_knot_key_falls_through_to_cookie() {
        let fx = setup().await;
        for value in [
            "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln",
            "bearer opaque-gateway-token",
            "Bearer\tya29.a0AfH6SMB",
        ] {
            for path in ["/api/docs", "/api/healthz"] {
                let r = req(path, Some(value));
                assert!(
                    authenticate(&fx.deps, &r).await.unwrap().is_none(),
                    "{value} on {path}"
                );
            }
        }
        // Next to another Authorization header it is still not ours.
        let r = Request::builder()
            .uri("/api/docs")
            .header(AUTHORIZATION, "Bearer eyJhbGciOiJSUzI1NiJ9.e30.c2ln")
            .header(AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(Body::empty())
            .unwrap();
        assert!(authenticate(&fx.deps, &r).await.unwrap().is_none());
    }

    /// A knot key mangled on its way into the header is still an attempt to
    /// use one, so still a 401, never the cookie: the scheme twice (the
    /// variable already held `Bearer …`), quotes kept by an env file, a
    /// zero-width space pasted in front, or a proxy joining two values.
    #[tokio::test]
    async fn a_mangled_knot_key_is_judged_not_passed_to_the_cookie() {
        let fx = setup().await;
        let (secret, _) = mint(&fx).await;
        let key = secret.as_str();
        for value in [
            format!("Bearer Bearer {key}"),
            format!("Bearer \"{key}\""),
            format!("Bearer '{key}'"),
            format!("Bearer \u{200b}{key}"),
            format!("Bearer eyJhbGciOiJSUzI1NiJ9.e30.c2ln, Bearer {key}"),
        ] {
            let r = Request::builder()
                .uri("/api/docs")
                .header(
                    AUTHORIZATION,
                    HeaderValue::from_bytes(value.as_bytes()).unwrap(),
                )
                .body(Body::empty())
                .unwrap();
            assert_invalid(&fx.deps, r).await;
        }
    }

    /// A knot key next to a proxy's Bearer token is ambiguous, so refused.
    #[tokio::test]
    async fn a_knot_key_next_to_a_foreign_bearer_token_is_refused() {
        let fx = setup().await;
        let (secret, _) = mint(&fx).await;
        let r = Request::builder()
            .uri("/api/docs")
            .header(AUTHORIZATION, "Bearer eyJhbGciOiJSUzI1NiJ9.e30.c2ln")
            .header(AUTHORIZATION, format!("Bearer {}", secret.as_str()))
            .body(Body::empty())
            .unwrap();
        assert_invalid(&fx.deps, r).await;
    }

    #[tokio::test]
    async fn a_scheme_that_only_starts_with_bearer_falls_through_to_cookie() {
        let fx = setup().await;
        let other = req("/api/docs", Some("Bearerish token"));
        assert!(authenticate(&fx.deps, &other).await.unwrap().is_none());
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
        let live = keys
            .find_live_by_hash(&hash(secret.as_str(), KEY))
            .await
            .unwrap()
            .expect("the key itself is live");
        let (logs, _guard) = log_capture::capture();
        assert_invalid(&deps, req("/api/docs", Some(&auth))).await;
        // Catches the refusal going silent: the 401 gives the holder no
        // reason, so the log is where an operator finds one.
        let logs = logs.text();
        assert!(
            logs.contains(&format!(
                "WARN auth.api_key: live api key of a non-member refused key_id={} user_id={}",
                live.id, outsider.id
            )),
            "{logs}"
        );
        assert!(!logs.contains(secret.as_str()), "{logs}");
    }

    /// Delegates to the real store, counting `touch` and optionally failing it.
    struct TouchProbe {
        inner: Arc<PgApiKeyStore>,
        touches: AtomicUsize,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ApiKeyStore for TouchProbe {
        async fn create(&self, k: NewApiKey<'_>) -> Result<ApiKey, ApiKeyStoreError> {
            self.inner.create(k).await
        }
        async fn find_live_by_hash(&self, hash: &[u8]) -> Result<Option<ApiKey>, ApiKeyStoreError> {
            self.inner.find_live_by_hash(hash).await
        }
        async fn list_for_user(
            &self,
            workspace_id: Uuid,
            user_id: Uuid,
        ) -> Result<Vec<ApiKey>, ApiKeyStoreError> {
            self.inner.list_for_user(workspace_id, user_id).await
        }
        async fn revoke(
            &self,
            workspace_id: Uuid,
            user_id: Uuid,
            key_id: Uuid,
        ) -> Result<bool, ApiKeyStoreError> {
            self.inner.revoke(workspace_id, user_id, key_id).await
        }
        async fn touch(&self, key_id: Uuid) -> Result<(), ApiKeyStoreError> {
            self.touches.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(ApiKeyStoreError::LimitReached);
            }
            self.inner.touch(key_id).await
        }
    }

    fn probe(fx: &mut Fx, fail: bool) -> Arc<TouchProbe> {
        let probe = Arc::new(TouchProbe {
            inner: fx.keys.clone(),
            touches: AtomicUsize::new(0),
            fail,
        });
        fx.deps.api_keys = Some(probe.clone());
        probe
    }

    /// Lets the spawned touch run: this is the current-thread runtime.
    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    /// Catches `lookup` spawning a touch per request, whatever `last_used_at`
    /// says, or never spawning one again for a value ahead of the clock.
    #[tokio::test]
    async fn only_a_key_due_for_a_touch_spawns_one() {
        let mut fx = setup().await;
        let probe = probe(&mut fx, false);
        let (secret, id) = mint(&fx).await;
        let r = || req("/api/docs", Some(&format!("Bearer {}", secret.as_str())));
        for (last_used_at, touches) in [
            ("NULL", 1),
            ("now() - interval '10 seconds'", 0),
            ("now() - interval '2 minutes'", 1),
            ("now() + interval '1 hour'", 1),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE api_keys SET last_used_at = {last_used_at} WHERE id = $1"
            )))
            .bind(id)
            .execute(&fx.pool)
            .await
            .unwrap();
            probe.touches.store(0, Ordering::SeqCst);
            authenticate(&fx.deps, &r()).await.unwrap().unwrap();
            settle().await;
            assert_eq!(
                probe.touches.load(Ordering::SeqCst),
                touches,
                "{last_used_at}"
            );
        }
    }

    /// Catches a failed touch that does not say which key it was for.
    #[tokio::test]
    async fn failed_touch_is_logged_with_the_key_id() {
        let mut fx = setup().await;
        let probe = probe(&mut fx, true);
        let (secret, id) = mint(&fx).await;
        let r = req("/api/docs", Some(&format!("Bearer {}", secret.as_str())));
        let (logs, _guard) = log_capture::capture();
        authenticate(&fx.deps, &r).await.unwrap().unwrap();
        settle().await;
        assert_eq!(probe.touches.load(Ordering::SeqCst), 1);
        let logs = logs.text();
        assert!(
            logs.contains(&format!(
                "WARN api key touch failed error=LimitReached key_id={id}"
            )),
            "{logs}"
        );
    }

    /// Catches a future `last_used_at` counting as fresh, which would stop
    /// touches for as long as the clock stays behind it, and an off-by-one
    /// at the minute the SQL debounce uses.
    #[test]
    fn touch_is_due_unless_last_used_under_a_minute_ago() {
        let now = DateTime::parse_from_rfc3339("2026-11-07T12:00:00Z")
            .unwrap()
            .to_utc();
        let at = |s: &str| Some(DateTime::parse_from_rfc3339(s).unwrap().to_utc());
        assert!(touch_is_due(None, now));
        assert!(!touch_is_due(at("2026-11-07T12:00:00Z"), now));
        assert!(!touch_is_due(at("2026-11-07T11:59:01Z"), now));
        assert!(touch_is_due(at("2026-11-07T11:59:00Z"), now));
        assert!(touch_is_due(at("2026-11-07T11:00:00Z"), now));
        assert!(touch_is_due(at("2026-11-07T12:00:01Z"), now));
        assert!(touch_is_due(at("2030-01-01T00:00:00Z"), now));
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
