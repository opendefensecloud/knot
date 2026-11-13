//! Move to subpage (`POST /api/docs/{id}/move-to-subpage`,
//! `2026-11-13-move-to-subpage-design.md`): the top-level blocks a selection
//! touches go to a new child page, a link to it takes their place, and the
//! comments and suggestions anchored entirely inside them move along.
//!
//! The source's room holds the doc throughout ([`knot_crdt::Event::
//! MoveBlocksOut`]): it builds the new page's content, waits while the
//! commit below stores the page and moves the rows in one transaction, then
//! cuts the blocks. It all runs on a task of its own, so a request timeout
//! or a client going away can't stop it halfway.

use knot_crdt::move_blocks::{self, Commit, Link, MoveBlocksError, Prepared};
use knot_storage::move_to_subpage::{SubpageError, SubpagePlan, create_subpage};
use knot_storage::{Document, WorkspaceRole};
use tokio::sync::oneshot;
use tracing::Instrument;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;

#[derive(Debug)]
pub enum MoveError {
    EditorRequired,
    NotFound,
    IsTemplate,
    Archived,
    /// The user may not add a page under the source; the answer to send.
    Destination(axum::response::Response),
    /// An anchor doesn't resolve to live content of the page.
    AnchorLost,
    SuggestionApplying,
    Internal(String),
}

/// A finished move.
#[derive(Debug)]
pub struct Moved {
    pub doc: Document,
    /// Threads and suggestions moved.
    pub comments: usize,
    pub suggestions: usize,
}

fn internal(e: impl std::fmt::Display) -> MoveError {
    MoveError::Internal(e.to_string())
}

/// Whether `ctx`'s user, whose role on the page is `doc_role`, may move
/// content of `source_id` into a new child: an editor of a live,
/// non-template page who may add a page under it.
pub async fn check(
    state: &AppState,
    ctx: &AuthContext,
    doc_role: WorkspaceRole,
    source_id: Uuid,
) -> Result<Document, MoveError> {
    if doc_role == WorkspaceRole::Viewer {
        return Err(MoveError::EditorRequired);
    }
    let docs = state
        .docs
        .as_ref()
        .ok_or_else(|| internal("docs not configured"))?;
    let source = match docs.get(source_id).await.map_err(internal)? {
        Some(doc) if doc.workspace_id == ctx.workspace_id => doc,
        _ => return Err(MoveError::NotFound),
    };
    if source.is_template {
        return Err(MoveError::IsTemplate);
    }
    if source.archived_at.is_some() {
        return Err(MoveError::Archived);
    }
    crate::restricted::require_editable_parent(state, ctx, source_id)
        .await
        .map_err(MoveError::Destination)?;
    if let Some(response) =
        crate::bots::deny_bot_restricted_destination(state, ctx, Some(source_id)).await
    {
        return Err(MoveError::Destination(response));
    }
    Ok(source)
}

/// Runs [`run`] on a task of its own; the receiver gets its answer.
pub fn start(
    state: AppState,
    ctx: AuthContext,
    source_id: Uuid,
    anchors: (Vec<u8>, Vec<u8>),
    title: String,
) -> oneshot::Receiver<Result<Moved, MoveError>> {
    let (answer, answered) = oneshot::channel();
    tokio::spawn(
        async move {
            let _ = answer.send(run(state, ctx, source_id, anchors, title).await);
        }
        .in_current_span(),
    );
    answered
}

async fn run(
    state: AppState,
    ctx: AuthContext,
    source_id: Uuid,
    (start, end): (Vec<u8>, Vec<u8>),
    title: String,
) -> Result<Moved, MoveError> {
    let pool = state
        .pool
        .clone()
        .ok_or_else(|| internal("pool not configured"))?;
    let rooms = state
        .rooms_v2
        .clone()
        .ok_or_else(|| internal("rooms not configured"))?;
    let room = rooms.acquire(source_id).await.map_err(internal)?;
    let new_id = Uuid::new_v4();
    let plan = SubpagePlan {
        ws: ctx.workspace_id,
        actor: ctx.user_id,
        source_id,
        new_id,
        title: title.clone(),
        state: Vec::new(),
    };
    // The commit's own outcome, beside the room's answer: the room only
    // learns whether it may cut.
    let (stored, outcome) = oneshot::channel::<Result<_, MoveError>>();
    let commit: Commit = Box::new(move |Prepared { child_state }| {
        Box::pin(async move {
            let result = async {
                let child = move_blocks::load(&child_state).map_err(internal)?;
                let plan = SubpagePlan {
                    state: child_state,
                    ..plan
                };
                create_subpage(&pool, &plan, |s, e| {
                    move_blocks::anchors_inside(&child, s, e)
                })
                .await
                .map_err(|e| match e {
                    SubpageError::SuggestionApplying => MoveError::SuggestionApplying,
                    e => internal(e),
                })
            }
            .await;
            let answer = match &result {
                Ok(_) => Ok(Link {
                    title,
                    href: format!("{}{new_id}", knot_markdown::DOC_URL_PREFIX),
                    meta: serde_json::json!({ "moved_to": new_id }),
                }),
                Err(e) => Err(format!("{e:?}")),
            };
            let _ = stored.send(result);
            answer
        })
    });
    let (reply, replied) = oneshot::channel();
    room.tx
        .send(knot_crdt::Event::MoveBlocksOut {
            start,
            end,
            by_user: Some(ctx.user_id),
            commit,
            reply,
        })
        .await
        .map_err(|_| internal("room closed"))?;
    let cut = replied
        .await
        .map_err(|_| internal("room dropped the move"))?;
    match cut {
        Ok(_) => {}
        Err(MoveBlocksError::AnchorLost | MoveBlocksError::Decode(_)) => {
            return Err(MoveError::AnchorLost);
        }
        // The commit's own error says why.
        Err(MoveBlocksError::Refused(_)) => {
            return Err(match outcome.await {
                Ok(Err(e)) => e,
                _ => internal("move refused"),
            });
        }
        // ponytail: the new page holds the blocks and their discussion, the
        // source still holds the blocks; no compensation. Add one if this is
        // ever seen.
        Err(e @ MoveBlocksError::Persist(_)) => return Err(internal(e)),
    }
    let stored = outcome
        .await
        .map_err(|_| internal("commit dropped"))?
        .map_err(|_| internal("commit failed after the cut"))?;
    for doc_id in [source_id, new_id] {
        crate::routes::api::comments::notify_comment_change(&state, doc_id);
        crate::routes::api::suggestions::notify_suggestion_change(&state, doc_id);
    }
    Ok(Moved {
        doc: stored.doc,
        comments: stored.comments,
        suggestions: stored.suggestions,
    })
}
