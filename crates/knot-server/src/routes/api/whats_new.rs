//! What's new since the user's last visit, and the release notes:
//! GET  /api/whats-new      → 200 {version, commit, released_on, dev,
//!                                 last_seen_version, unseen: [version]}
//! POST /api/whats-new/seen → 204, records the running version as seen
//! GET  /api/release-notes  → 200 {releases: [{version, date,
//!                                 sections: [{title, html}]}]}
//!
//! The web client shows its "knot was updated" card while `unseen` is not
//! empty. Seen state is per user (`users.last_seen_version`), so the card
//! shows once across devices, and it only ever moves forward. A user with
//! nothing stored yet starts caught up. A build whose version is not semver
//! ("dev") is a development build: nothing is unseen and nothing is written.

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use knot_storage::{UserStore, UserStoreError};
use semver::Version;
use serde::Serialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;
use crate::release_notes::{self, RELEASES, Release};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/whats-new", get(whats_new))
        .route("/api/whats-new/seen", post(mark_seen))
        .route("/api/release-notes", get(release_notes))
}

#[derive(Serialize)]
struct WhatsNew {
    version: &'static str,
    commit: &'static str,
    released_on: Option<&'static str>,
    dev: bool,
    last_seen_version: Option<String>,
    unseen: Vec<&'static Version>,
}

async fn whats_new(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(users) = state.users.clone() else {
        return internal();
    };
    let running = Version::parse(state.build_version).ok();
    let last_seen = match last_seen_or_init(users.as_ref(), ctx.user_id, running.as_ref()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error=?e, "whats_new");
            return internal();
        }
    };
    let releases: &'static [Release] = &RELEASES;
    let seen = last_seen.as_deref().and_then(|v| Version::parse(v).ok());
    let unseen = match (&running, &seen) {
        (Some(running), Some(seen)) => release_notes::unseen(releases, seen, running),
        _ => Vec::new(),
    };
    Json(WhatsNew {
        version: state.build_version,
        commit: env!("KNOT_BUILD_COMMIT"),
        released_on: releases
            .iter()
            .find(|r| running.as_ref() == Some(&r.version))
            .and_then(|r| r.date.as_deref()),
        dev: running.is_none(),
        last_seen_version: last_seen,
        unseen,
    })
    .into_response()
}

async fn mark_seen(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Ok(running) = Version::parse(state.build_version) else {
        // A development build is no release to have seen.
        return StatusCode::NO_CONTENT.into_response();
    };
    let Some(users) = state.users.clone() else {
        return internal();
    };
    match advance_last_seen(users.as_ref(), ctx.user_id, &running).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "whats_new mark_seen");
            internal()
        }
    }
}

#[derive(Serialize)]
struct ReleaseNotes {
    releases: &'static [Release],
}

async fn release_notes(req: Request) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    Json(ReleaseNotes {
        releases: &RELEASES,
    })
    .into_response()
}

/// The user's last seen version. With nothing stored yet (a user created
/// after this shipped) it is first set to the running version, so they start
/// caught up rather than with every release since 0.1.0. `running` is
/// `None` on a dev build, which never writes.
async fn last_seen_or_init(
    users: &dyn UserStore,
    user_id: Uuid,
    running: Option<&Version>,
) -> Result<Option<String>, UserStoreError> {
    let stored = users.last_seen_version(user_id).await?;
    let (None, Some(running)) = (&stored, running) else {
        return Ok(stored);
    };
    let version = running.to_string();
    if users.set_last_seen_version(user_id, None, &version).await? {
        Ok(Some(version))
    } else {
        // Another request set it first.
        users.last_seen_version(user_id).await
    }
}

/// Raises the user's last seen version to `running`, never lowers it: a
/// server rolled back, or an old pod during a rolling upgrade, must not
/// bring the card back for a release the user has already seen.
async fn advance_last_seen(
    users: &dyn UserStore,
    user_id: Uuid,
    running: &Version,
) -> Result<(), UserStoreError> {
    let version = running.to_string();
    // Each lost compare-and-set means another request wrote in between, and
    // a few rounds settle that. The bound also ends the loop for a user
    // deleted meanwhile, whose row never matches.
    for _ in 0..3 {
        let stored = users.last_seen_version(user_id).await?;
        let seen = stored.as_deref().and_then(|v| Version::parse(v).ok());
        if seen.is_some_and(|seen| seen >= *running) {
            return Ok(());
        }
        if users
            .set_last_seen_version(user_id, stored.as_deref(), &version)
            .await?
        {
            return Ok(());
        }
    }
    Ok(())
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
