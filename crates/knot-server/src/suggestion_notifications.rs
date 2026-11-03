//! Inbox rows for change suggestions.
//!
//! - `suggestion`, on create, to the page's owners except the author (see
//!   [`suggestion_owner_recipients`] for who counts as an owner here);
//! - `suggestion_decided`, on accept or reject, to the author when someone
//!   else decided;
//! - `reply` / `mention`, on a reply, as comment replies do.
//!
//! Every row has `target_kind = "suggestion"`, `target_id` = the suggestion
//! id, and `data` with `suggestion_id`, `doc_title` and a 140-character
//! `excerpt`. Recipients who cannot read the page are dropped, so a
//! notification never reveals a restricted page.
//!
//! Emitters run after the write has committed and never fail the request:
//! errors are logged and the notification is lost, the same at-most-once
//! behaviour comment notifications have.

use std::collections::HashSet;

use knot_storage::{
    Document, NewNotification, NotificationKind, Suggestion, SuggestionKind, SuggestionReply,
    WorkspaceRole,
};
use uuid::Uuid;

use crate::AppState;
use crate::routes::api::comments::extract_mentions;

/// Characters of text quoted in a notification's `excerpt`.
const EXCERPT_CHARS: usize = 140;

/// Distinguishes a new reply from an edit of one, for
/// [`emit_suggestion_reply`]. An edit says nothing new to the thread, so it
/// fans out no `reply` rows; it can still add `mention` rows, since the
/// edited body may name someone for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyWrite {
    Created,
    Edited,
}

/// The first [`EXCERPT_CHARS`] characters of `text`.
fn excerpt(text: &str) -> String {
    text.chars().take(EXCERPT_CHARS).collect()
}

/// The text a `suggestion` or `suggestion_decided` row quotes: the proposed
/// text, or for a deletion the text it removes.
fn suggestion_excerpt(s: &Suggestion) -> String {
    match s.kind {
        SuggestionKind::Delete => excerpt(&s.original_text),
        SuggestionKind::Replace | SuggestionKind::Insert => excerpt(&s.proposed_text),
    }
}

/// The page, or `None` (logged) when it cannot be read.
async fn load_doc(state: &AppState, doc_id: Uuid) -> Option<Document> {
    let docs = state.docs.as_ref()?;
    match docs.get(doc_id).await {
        Ok(Some(doc)) => Some(doc),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error=?e, %doc_id, "suggestion notifications: doc lookup");
            None
        }
    }
}

/// The `data` every suggestion notification carries.
fn base_data(suggestion_id: Uuid, doc_title: &str, excerpt: String) -> serde_json::Value {
    serde_json::json!({
        "suggestion_id": suggestion_id.to_string(),
        "doc_title": doc_title,
        "excerpt": excerpt,
    })
}

/// Keep the ids whose effective role on the doc satisfies `keep`, in order.
/// A lookup error drops the id: a missed notification is cheaper than a
/// leaked title.
async fn filter_by_role(
    state: &AppState,
    workspace_id: Uuid,
    doc_id: Uuid,
    ids: Vec<Uuid>,
    keep: impl Fn(Option<WorkspaceRole>) -> bool,
) -> Vec<Uuid> {
    let Some(acl) = state.acl.clone() else {
        return Vec::new();
    };
    let roles = futures::future::join_all(ids.iter().map(|&user_id| {
        let acl = acl.clone();
        async move { acl.effective_role(workspace_id, doc_id, user_id).await }
    }))
    .await;
    ids.into_iter()
        .zip(roles)
        .filter_map(|(id, role)| match role {
            Ok(role) if keep(role) => Some(id),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error=?e, %doc_id, user_id=%id, "suggestion notifications: acl lookup");
                None
            }
        })
        .collect()
}

/// Who hears about a new suggestion on `doc`.
///
/// Candidates = explicit owner grants on the doc or inheriting ancestors
/// (`GrantStore::list_inherited`, role Owner, principal `user:<uuid>`) ∪
/// {`doc.created_by`}, kept only if their effective role is Owner. Only if
/// THAT set is empty: workspace members with role Owner, same filter.
/// `author` is removed last, so a creator suggesting on their own page
/// notifies nobody rather than every workspace owner.
///
/// Deliberately narrower than "everyone who may accept": on an unrestricted
/// page every workspace Owner is an effective Owner, and telling all of them
/// about every suggestion would be noise.
///
/// Takes the loaded `doc` (not its id) so the create path, which also needs
/// the title, reads the doc once.
pub(crate) async fn suggestion_owner_recipients(
    state: &AppState,
    workspace_id: Uuid,
    doc: &Document,
    author: Uuid,
) -> Vec<Uuid> {
    let (Some(grants), Some(workspaces)) = (state.grants.as_ref(), state.workspaces.as_ref())
    else {
        return Vec::new();
    };
    let doc_id = doc.id;

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    match grants.list_inherited(workspace_id, doc_id).await {
        Ok(gs) => {
            for g in gs.into_iter().filter(|g| g.role == WorkspaceRole::Owner) {
                if let Some(id) = g
                    .principal
                    .strip_prefix("user:")
                    .and_then(|u| Uuid::parse_str(u).ok())
                    && seen.insert(id)
                {
                    candidates.push(id);
                }
            }
        }
        // Without the grants the explicit set is unknown; falling back to
        // every workspace owner would be the noise this rule avoids.
        Err(e) => {
            tracing::warn!(error=?e, %doc_id, "suggestion recipients: grant lookup");
            return Vec::new();
        }
    }
    if seen.insert(doc.created_by) {
        candidates.push(doc.created_by);
    }
    let is_owner = |role: Option<WorkspaceRole>| role == Some(WorkspaceRole::Owner);
    let mut recipients = filter_by_role(state, workspace_id, doc_id, candidates, is_owner).await;

    if recipients.is_empty() {
        let members = match workspaces.list_members(workspace_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error=?e, %workspace_id, "suggestion recipients: member lookup");
                return Vec::new();
            }
        };
        let owners = members
            .into_iter()
            .filter(|m| m.role == WorkspaceRole::Owner)
            .map(|m| m.user_id)
            .collect();
        recipients = filter_by_role(state, workspace_id, doc_id, owners, is_owner).await;
    }

    recipients.retain(|&id| id != author);
    recipients
}

/// Write `batch`, logging (not returning) a failure.
async fn emit(state: &AppState, batch: &[NewNotification], suggestion_id: Uuid, what: &str) {
    if batch.is_empty() {
        return;
    }
    let Some(notifications) = state.notifications.as_ref() else {
        return;
    };
    if let Err(e) = notifications.emit_many(batch).await {
        tracing::warn!(error=?e, %suggestion_id, "emit {what} notifications");
    }
}

/// `suggestion` rows for a new suggestion, to
/// [`suggestion_owner_recipients`].
pub(crate) async fn emit_suggestion_created(state: &AppState, workspace_id: Uuid, s: &Suggestion) {
    let Some(doc) = load_doc(state, s.doc_id).await else {
        return;
    };
    let recipients = suggestion_owner_recipients(state, workspace_id, &doc, s.author_id).await;
    let data = base_data(s.id, &doc.title, suggestion_excerpt(s));
    let batch: Vec<NewNotification> = recipients
        .into_iter()
        .map(|user_id| NewNotification {
            workspace_id,
            user_id,
            actor_id: Some(s.author_id),
            kind: NotificationKind::Suggestion,
            doc_id: Some(s.doc_id),
            target_kind: "suggestion".into(),
            target_id: s.id.to_string(),
            dedupe_key: format!("suggestion:{}", s.id),
            data: data.clone(),
        })
        .collect();
    emit(state, &batch, s.id, "suggestion").await;
}

/// A `suggestion_decided` row to `s.author_id` when `s.decided_by` is
/// someone else. `data` also carries `"decision": "accepted" | "rejected"`.
/// The dedupe key makes a repeated accept of the same suggestion a no-op,
/// and an accept the route and the sweep both finish tell the author once.
pub(crate) async fn emit_suggestion_decided(state: &AppState, workspace_id: Uuid, s: &Suggestion) {
    if !decided_by_other(s) {
        return;
    }
    let Some(doc) = load_doc(state, s.doc_id).await else {
        return;
    };
    emit_decided(state, workspace_id, &doc.title, s).await;
}

/// [`emit_suggestion_decided`] for a caller without a request context: the
/// stuck-accept sweep, which takes the workspace from the doc.
pub(crate) async fn emit_suggestion_decided_from_sweep(state: &AppState, s: &Suggestion) {
    if !decided_by_other(s) {
        return;
    }
    let Some(doc) = load_doc(state, s.doc_id).await else {
        return;
    };
    emit_decided(state, doc.workspace_id, &doc.title, s).await;
}

/// Whether `s` was decided by someone other than its author.
fn decided_by_other(s: &Suggestion) -> bool {
    s.decided_by.is_some_and(|by| by != s.author_id)
}

async fn emit_decided(state: &AppState, workspace_id: Uuid, title: &str, s: &Suggestion) {
    let Some(decided_by) = s.decided_by else {
        return;
    };
    let readable = filter_by_role(state, workspace_id, s.doc_id, vec![s.author_id], |role| {
        role.is_some()
    })
    .await;
    let mut data = base_data(s.id, title, suggestion_excerpt(s));
    data["decision"] = s.status.as_str().into();
    let batch: Vec<NewNotification> = readable
        .into_iter()
        .map(|user_id| NewNotification {
            workspace_id,
            user_id,
            actor_id: Some(decided_by),
            kind: NotificationKind::SuggestionDecided,
            doc_id: Some(s.doc_id),
            target_kind: "suggestion".into(),
            target_id: s.id.to_string(),
            dedupe_key: format!("suggestion_decided:{}", s.id),
            data: data.clone(),
        })
        .collect();
    emit(state, &batch, s.id, "suggestion_decided").await;
}

/// Inbox rows for a reply write: `mention` for every member named in the
/// body (picker ids ∪ `@name` matches, on create and edit) and, for a new
/// reply only, `reply` for the suggestion's author and the thread's other
/// repliers. A user who is both gets the mention only; the reply's author
/// gets neither. Recipients must still be able to read the page.
pub(crate) async fn emit_suggestion_reply(
    state: &AppState,
    workspace_id: Uuid,
    s: &Suggestion,
    reply: &SuggestionReply,
    write: ReplyWrite,
    explicit_mentions: &[Uuid],
) {
    let (Some(workspaces), Some(suggestions)) =
        (state.workspaces.as_ref(), state.suggestions.as_ref())
    else {
        return;
    };
    let Some(doc) = load_doc(state, s.doc_id).await else {
        return;
    };
    let members = match workspaces.list_members(workspace_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error=?e, suggestion_id=%s.id, "suggestion reply notifications: member lookup");
            return;
        }
    };

    // Membership decides: an id or handle for a non-member is dropped.
    let handles = extract_mentions(&reply.body);
    let mut seen: HashSet<Uuid> = HashSet::from([reply.author_id]);
    let mentioned: Vec<Uuid> = members
        .iter()
        .filter(|m| {
            explicit_mentions.contains(&m.user_id)
                || handles.contains(&m.display_name.to_lowercase())
        })
        .map(|m| m.user_id)
        .filter(|id| seen.insert(*id))
        .collect();

    let mut replied = Vec::new();
    if write == ReplyWrite::Created {
        let mut thread = vec![s.author_id];
        match suggestions.list_replies(s.doc_id).await {
            Ok(rs) => thread.extend(
                rs.into_iter()
                    .filter(|r| r.suggestion_id == s.id)
                    .map(|r| r.author_id),
            ),
            Err(e) => {
                tracing::warn!(error=?e, suggestion_id=%s.id, "suggestion reply notifications: thread lookup");
            }
        }
        replied = thread.into_iter().filter(|id| seen.insert(*id)).collect();
    }

    let readable = |role: Option<WorkspaceRole>| role.is_some();
    let mentioned = filter_by_role(state, workspace_id, s.doc_id, mentioned, readable).await;
    let replied = filter_by_role(state, workspace_id, s.doc_id, replied, readable).await;

    let data = base_data(s.id, &doc.title, excerpt(&reply.body));
    let row = |user_id: Uuid, kind: NotificationKind, dedupe_key: String| NewNotification {
        workspace_id,
        user_id,
        actor_id: Some(reply.author_id),
        kind,
        doc_id: Some(s.doc_id),
        target_kind: "suggestion".into(),
        target_id: s.id.to_string(),
        dedupe_key,
        data: data.clone(),
    };
    let batch: Vec<NewNotification> = mentioned
        .into_iter()
        .map(|id| {
            row(
                id,
                NotificationKind::Mention,
                format!("suggestion_mention:{}", reply.id),
            )
        })
        .chain(replied.into_iter().map(|id| {
            row(
                id,
                NotificationKind::Reply,
                format!("suggestion_reply:{}", reply.id),
            )
        }))
        .collect();
    emit(state, &batch, s.id, "suggestion reply").await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use knot_storage::SuggestionStatus;

    fn suggestion(kind: SuggestionKind, original: &str, proposed: &str) -> Suggestion {
        Suggestion {
            id: Uuid::new_v4(),
            doc_id: Uuid::new_v4(),
            author_id: Uuid::new_v4(),
            kind,
            position_y: Vec::new(),
            position_y_end: Vec::new(),
            original_text: original.into(),
            context_before: String::new(),
            proposed_text: proposed.into(),
            reason: None,
            status: SuggestionStatus::Pending,
            decided_by: None,
            decided_at: None,
            apply_token: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn excerpt_quotes_proposed_text_or_the_deleted_text() {
        let replace = suggestion(SuggestionKind::Replace, "old", "new");
        assert_eq!(suggestion_excerpt(&replace), "new");
        let insert = suggestion(SuggestionKind::Insert, "", "added");
        assert_eq!(suggestion_excerpt(&insert), "added");
        let delete = suggestion(SuggestionKind::Delete, "gone", "");
        assert_eq!(suggestion_excerpt(&delete), "gone");
    }

    #[test]
    fn excerpt_truncates_to_140_characters() {
        let long = "ü".repeat(200);
        let s = suggestion(SuggestionKind::Replace, "x", &long);
        assert_eq!(suggestion_excerpt(&s).chars().count(), EXCERPT_CHARS);
        assert_eq!(excerpt("short"), "short");
    }
}
