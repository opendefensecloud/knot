//! Duplicate's storage: the live subtree it copies, the one transaction that
//! stores the copy, the blob-store copy, and the cleanup of objects copied
//! for a duplicate that never committed. Against a fresh database per test.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use knot_storage::blobs::Result as BlobResult;
use knot_storage::duplicate::{
    BlobCopy, BoardCopy, DuplicatePlan, PageCopy, apply_duplicate, purge_stale_copies,
    record_pending_copies,
};
use knot_storage::{
    BlobMeta, BlobMetadata, BlobStore, BlobStoreError, BoardStore, DocStore, DocStoreError,
    PgBoardStore, PgBytesStore, PgDocStore, PgUserStore, PgWorkspaceStore, Placement, SubtreeRow,
    UserStore, WorkspaceRole, WorkspaceStore, lexorank,
};
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

struct World {
    store: PgDocStore,
    pool: PgPool,
    ws: Uuid,
    /// Owns the workspace and wrote the source pages.
    owner: Uuid,
    /// Duplicates them.
    actor: Uuid,
}

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());
    let ws = workspaces.create("default", "W").await.unwrap();
    let owner = users.create_local("o@x.test", "O", "$h$").await.unwrap();
    let actor = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    workspaces
        .add_member(ws.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(ws.id, actor.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    World {
        store: PgDocStore::new(pool.clone()),
        pool,
        ws: ws.id,
        owner: owner.id,
        actor: actor.id,
    }
}

impl World {
    /// A page inserted with raw SQL, so the test chooses its key.
    async fn seed(&self, parent: Option<Uuid>, title: &str, key: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(self.ws)
        .bind(parent)
        .bind(title)
        .bind(key)
        .bind(self.owner)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A board on `doc`, with some history.
    async fn board(&self, doc: Uuid) -> Uuid {
        let boards = PgBoardStore::new(self.pool.clone());
        let board = boards
            .create(doc, self.owner, Some("Sketch".into()))
            .await
            .unwrap();
        boards.append_update(board.id, b"stroke").await.unwrap();
        board.id
    }

    /// A blob attached to `doc`. With `in_db`, its bytes are stored in
    /// `blob_bytes`, as the Postgres backend does.
    async fn blob(&self, doc: Uuid, bytes: &[u8], in_db: bool) -> BlobMetadata {
        let meta = BlobMetadata {
            id: Uuid::new_v4(),
            workspace_id: self.ws,
            doc_id: doc,
            content_type: "image/png".into(),
            byte_size: bytes.len() as i64,
            sha256: vec![7; 32],
            original_name: Some("cat.png".into()),
            created_by: self.owner,
            created_at: chrono::Utc::now(),
        };
        BlobMeta::new(self.pool.clone())
            .insert(&meta)
            .await
            .unwrap();
        if in_db {
            PgBytesStore::new(self.pool.clone())
                .put(meta.id, bytes, &meta.content_type)
                .await
                .unwrap();
        }
        BlobMeta::new(self.pool.clone())
            .find(meta.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// `(title, sort_key)` of the whole group under `parent`, in the
    /// sidebar's order.
    async fn group(&self, parent: Option<Uuid>) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT title, sort_key FROM documents
              WHERE workspace_id = $1 AND parent_id IS NOT DISTINCT FROM $2
              ORDER BY sort_key COLLATE \"C\", id",
        )
        .bind(self.ws)
        .bind(parent)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// The row count of every table a duplicate writes to.
    async fn counts(&self) -> Vec<(&'static str, i64)> {
        let mut counts = Vec::new();
        for table in [
            "documents",
            "doc_updates",
            "doc_contributors",
            "boards",
            "board_updates",
            "blobs",
            "blob_bytes",
            "blob_copy_pending",
            "audit_events",
            "acl_invalidations",
        ] {
            let n: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&self.pool)
                .await
                .unwrap();
            counts.push((table, n));
        }
        counts
    }
}

fn page(parent: Option<Uuid>, title: &str, state: &[u8]) -> PageCopy {
    PageCopy {
        new_id: Uuid::new_v4(),
        parent,
        title: title.into(),
        icon: Some("📄".into()),
        state: state.to_vec(),
    }
}

/// A plan that copies `source`, under `parent`, without subpages.
fn plan_for(w: &World, source: Uuid, parent: Option<Uuid>) -> DuplicatePlan {
    DuplicatePlan {
        ws: w.ws,
        actor: w.actor,
        source_id: source,
        top: page(parent, "S (copy)", b"top"),
        pages: Vec::new(),
        boards: Vec::new(),
        blobs: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_duplicate_inserts_everything_atomically() {
    let w = world().await;
    let s = w.seed(None, "S", "g").await;
    w.seed(None, "T", "m").await;
    let c1 = w.seed(Some(s), "C1", "a").await;
    let c2 = w.seed(Some(s), "C2", "b").await;
    let g = w.seed(Some(c1), "G", "a").await;
    let board_s = w.board(s).await;
    let board_g = w.board(g).await;
    let in_db = w.blob(s, b"png bytes", true).await;
    let in_s3 = w.blob(c2, b"s3 bytes", false).await;

    let top = page(None, "S (copy)", b"state of S");
    let c1_copy = page(Some(top.new_id), "C1", b"state of C1");
    let c2_copy = page(Some(top.new_id), "C2", b"state of C2");
    let g_copy = page(Some(c1_copy.new_id), "G", b"state of G");
    let boards = vec![
        BoardCopy {
            new_id: Uuid::new_v4(),
            new_doc_id: top.new_id,
            label: Some("Sketch".into()),
            state: b"board state of S".to_vec(),
            svg: Some(b"<svg/>".to_vec()),
        },
        BoardCopy {
            new_id: Uuid::new_v4(),
            new_doc_id: g_copy.new_id,
            label: None,
            state: b"board state of G".to_vec(),
            svg: None,
        },
    ];
    let blobs = vec![
        BlobCopy {
            new_id: Uuid::new_v4(),
            new_doc_id: top.new_id,
            source: in_db.clone(),
            bytes_in_db: true,
        },
        BlobCopy {
            new_id: Uuid::new_v4(),
            new_doc_id: c2_copy.new_id,
            source: in_s3.clone(),
            bytes_in_db: false,
        },
    ];
    // The route records every object it copies outside the database first.
    record_pending_copies(&w.pool, &[blobs[1].new_id])
        .await
        .unwrap();
    let plan = DuplicatePlan {
        ws: w.ws,
        actor: w.actor,
        source_id: s,
        top,
        pages: vec![c1_copy, c2_copy, g_copy],
        boards,
        blobs,
    };

    let outcome = apply_duplicate(&w.pool, &plan).await.unwrap();

    // The top page sits right after the source, with a valid key.
    let copy = &outcome.top;
    assert_eq!(copy.id, plan.top.new_id);
    assert_eq!(copy.workspace_id, w.ws);
    assert_eq!(copy.parent_id, None);
    assert_eq!(copy.title, "S (copy)");
    assert_eq!(copy.icon.as_deref(), Some("📄"));
    assert_eq!(copy.created_by, w.actor);
    assert!(!copy.is_template && copy.archived_at.is_none());
    let top_level = w.group(None).await;
    let titles: Vec<&str> = top_level.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(titles, ["S", "S (copy)", "T"]);
    assert!(lexorank::is_valid(&copy.sort_key), "{top_level:?}");
    assert_eq!(top_level[1].1, copy.sort_key);
    assert_eq!(top_level[0].1, "g", "the source keeps its key");

    // Subpages keep their structure, each group with `spread` keys.
    let spread2 = lexorank::spread(2);
    assert_eq!(
        w.group(Some(plan.top.new_id)).await,
        [
            ("C1".to_string(), spread2[0].clone()),
            ("C2".to_string(), spread2[1].clone()),
        ]
    );
    assert_eq!(
        w.group(Some(plan.pages[0].new_id)).await,
        [("G".to_string(), lexorank::spread(1)[0].clone())]
    );
    let all: Vec<Uuid> = std::iter::once(&plan.top)
        .chain(&plan.pages)
        .map(|p| p.new_id)
        .collect();
    for (p, id) in std::iter::once(&plan.top).chain(&plan.pages).zip(&all) {
        let doc = w.store.get(*id).await.unwrap().unwrap();
        assert_eq!((doc.created_by, doc.icon.as_deref()), (w.actor, Some("📄")));
        assert_eq!(doc.workspace_id, w.ws);
        assert_eq!(doc.title, p.title);
    }

    // One update per page, by the actor, who is its only contributor.
    assert_eq!(
        outcome.pages.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        all,
        "the top page first, then the subpages in plan order"
    );
    for (p, (id, seq)) in std::iter::once(&plan.top)
        .chain(&plan.pages)
        .zip(&outcome.pages)
    {
        let updates: Vec<(i64, Option<Uuid>, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, by_user_id, update_bytes FROM doc_updates WHERE doc_id = $1",
        )
        .bind(id)
        .fetch_all(&w.pool)
        .await
        .unwrap();
        assert_eq!(updates, [(*seq, Some(w.actor), p.state.clone())]);
        let contributors: Vec<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM doc_contributors WHERE doc_id = $1")
                .bind(id)
                .fetch_all(&w.pool)
                .await
                .unwrap();
        assert_eq!(contributors, [w.actor]);
    }

    // Boards: a row each, with the state as one update and the SVG.
    for b in &plan.boards {
        let row: (Uuid, Uuid, Option<String>, Option<Vec<u8>>) = sqlx::query_as(
            "SELECT doc_id, created_by, label, svg_cached FROM boards WHERE id = $1",
        )
        .bind(b.new_id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
        assert_eq!(row, (b.new_doc_id, w.actor, b.label.clone(), b.svg.clone()));
        let updates: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT bytes FROM board_updates WHERE board_id = $1")
                .bind(b.new_id)
                .fetch_all(&w.pool)
                .await
                .unwrap();
        assert_eq!(updates, std::slice::from_ref(&b.state));
    }
    let boards = PgBoardStore::new(w.pool.clone());
    assert_eq!(boards.load_updates(board_s).await.unwrap(), [b"stroke"]);
    assert_eq!(boards.load_updates(board_g).await.unwrap(), [b"stroke"]);

    // Blobs: the source's metadata on the copy's page; bytes held in the
    // database are copied, other stores' objects were copied by the route.
    let meta = BlobMeta::new(w.pool.clone());
    for b in &plan.blobs {
        let copy = meta.find(b.new_id).await.unwrap().unwrap();
        assert_eq!(
            (copy.workspace_id, copy.doc_id, copy.created_by),
            (w.ws, b.new_doc_id, w.actor)
        );
        assert_eq!(
            (
                &copy.content_type,
                copy.byte_size,
                &copy.sha256,
                &copy.original_name
            ),
            (
                &b.source.content_type,
                b.source.byte_size,
                &b.source.sha256,
                &b.source.original_name
            )
        );
    }
    let pg = PgBytesStore::new(w.pool.clone());
    assert_eq!(pg.get(plan.blobs[0].new_id).await.unwrap(), b"png bytes");
    assert_eq!(pg.get(in_db.id).await.unwrap(), b"png bytes");
    assert!(matches!(
        pg.get(plan.blobs[1].new_id).await,
        Err(BlobStoreError::NotFound)
    ));
    assert_eq!(
        pending(&w.pool).await,
        0,
        "the commit takes the copies' pending entries"
    );

    // One audit entry naming the source, and the invalidation of a create.
    let audit: Vec<(Option<Uuid>, String, Option<String>)> = sqlx::query_as(
        "SELECT actor_id, target_kind, data->>'source_id' FROM audit_events
          WHERE action = 'doc.duplicate' AND target_id = $1",
    )
    .bind(plan.top.new_id)
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        [(Some(w.actor), "doc".to_string(), Some(s.to_string()))]
    );
    let reasons: Vec<String> =
        sqlx::query_scalar("SELECT reason FROM acl_invalidations WHERE doc_id = $1")
            .bind(plan.top.new_id)
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(reasons, ["create"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_partial_rows_on_failure() {
    let w = world().await;
    let s = w.seed(None, "S", "g").await;
    let in_db = w.blob(s, b"png bytes", true).await;
    let in_s3 = w.blob(s, b"s3 bytes", false).await;
    let mut plan = plan_for(&w, s, None);
    plan.pages = vec![page(Some(plan.top.new_id), "C", b"c")];
    plan.boards = vec![BoardCopy {
        new_id: Uuid::new_v4(),
        new_doc_id: plan.top.new_id,
        label: None,
        state: b"board".to_vec(),
        svg: Some(b"<svg/>".to_vec()),
    }];
    let pending = Uuid::new_v4();
    plan.blobs = vec![
        BlobCopy {
            new_id: pending,
            new_doc_id: plan.top.new_id,
            source: in_s3.clone(),
            bytes_in_db: false,
        },
        // The last row the transaction inserts takes an id that exists.
        BlobCopy {
            new_id: in_db.id,
            new_doc_id: plan.top.new_id,
            source: in_db.clone(),
            bytes_in_db: true,
        },
    ];
    record_pending_copies(&w.pool, &[pending]).await.unwrap();
    let before = w.counts().await;

    let result = apply_duplicate(&w.pool, &plan).await;

    assert!(result.is_err(), "{:?}", result.map(|o| o.top));
    assert_eq!(w.counts().await, before);
    assert_eq!(w.group(None).await, [("S".to_string(), "g".to_string())]);
    let kept: Vec<Uuid> = sqlx::query_scalar("SELECT blob_id FROM blob_copy_pending")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert_eq!(kept, [pending], "cleanup is left to the purge job");
}

#[tokio::test(flavor = "multi_thread")]
async fn db_bytes_missing_is_a_conflict() {
    let w = world().await;
    let s = w.seed(None, "S", "g").await;
    let in_db = w.blob(s, b"png bytes", true).await;
    let mut plan = plan_for(&w, s, None);
    plan.blobs = vec![BlobCopy {
        new_id: Uuid::new_v4(),
        new_doc_id: plan.top.new_id,
        source: in_db.clone(),
        bytes_in_db: true,
    }];
    // The source's bytes went after the plan was made (its page was purged).
    sqlx::query("DELETE FROM blob_bytes WHERE blob_id = $1")
        .bind(in_db.id)
        .execute(&w.pool)
        .await
        .unwrap();
    let before = w.counts().await;

    let result = apply_duplicate(&w.pool, &plan).await;

    assert!(
        matches!(result, Err(DocStoreError::DuplicateConflict)),
        "{:?}",
        result.map(|o| o.top)
    );
    assert_eq!(w.counts().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn source_moved_or_archived_meanwhile_is_a_conflict() {
    // Each case changes the tree after the plan was made, so the copy can't
    // go right after the source any more.
    for case in [
        "source archived",
        "source moved",
        "parent archived",
        "parent purged",
    ] {
        let w = world().await;
        let p = w.seed(None, "P", "g").await;
        let q = w.seed(None, "Q", "m").await;
        let s = w.seed(Some(p), "S", "g").await;
        let mut plan = plan_for(&w, s, Some(p));
        plan.pages = vec![page(Some(plan.top.new_id), "C", b"c")];
        match case {
            "source archived" => {
                w.store.archive(w.ws, s, w.owner).await.unwrap();
            }
            "source moved" => {
                w.store
                    .move_placed(w.ws, s, w.owner, Some(q), Placement::Last)
                    .await
                    .unwrap();
            }
            "parent archived" => {
                w.store.archive(w.ws, p, w.owner).await.unwrap();
            }
            "parent purged" => {
                w.store.archive(w.ws, p, w.owner).await.unwrap();
                w.store.purge(w.ws, p, Some(w.owner)).await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = w.counts().await;

        let result = apply_duplicate(&w.pool, &plan).await;

        assert!(
            matches!(result, Err(DocStoreError::DuplicateConflict)),
            "{case}: {:?}",
            result.map(|o| o.top)
        );
        assert_eq!(w.counts().await, before, "{case}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_subtree_skips_archived_and_template_subtrees() {
    let w = world().await;
    let root = w.seed(None, "R", "m").await;
    // Siblings come in code-unit order: "M" (an old release's key) sorts
    // before "g".
    let a = w.seed(Some(root), "A", "m").await;
    let b = w.seed(Some(root), "B", "g").await;
    let z = w.seed(Some(root), "Z", "M").await;
    let b2 = w.seed(Some(b), "B2", "z").await;
    let b1 = w.seed(Some(b), "B1", "a").await;
    let a1 = w.seed(Some(a), "A1", "m").await;
    sqlx::query("UPDATE documents SET icon = '🌲' WHERE id = $1")
        .bind(a1)
        .execute(&w.pool)
        .await
        .unwrap();
    // Neither an archived page nor a template is copied, and nothing under
    // them is, even a live page.
    let archived = w.seed(Some(root), "Archived", "h").await;
    sqlx::query("UPDATE documents SET archived_at = now(), archive_root_id = id WHERE id = $1")
        .bind(archived)
        .execute(&w.pool)
        .await
        .unwrap();
    w.seed(Some(archived), "Under archived", "m").await;
    let template = w.seed(Some(root), "Template", "i").await;
    sqlx::query("UPDATE documents SET is_template = true WHERE id = $1")
        .bind(template)
        .execute(&w.pool)
        .await
        .unwrap();
    w.seed(Some(template), "Under template", "m").await;
    let archived_leaf = w.seed(Some(a), "Archived leaf", "a").await;
    w.store.archive(w.ws, archived_leaf, w.owner).await.unwrap();
    // Outside the subtree.
    w.seed(None, "Sibling of R", "t").await;

    let rows = w.store.live_subtree(w.ws, root).await.unwrap();

    // Depth first, in the sidebar's order, the root left out.
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, [z, b, b1, b2, a, a1], "{rows:?}");
    let a1_row = &rows[5];
    assert_eq!(
        a1_row,
        &SubtreeRow {
            id: a1,
            parent_id: a,
            title: "A1".into(),
            icon: Some("🌲".into()),
            sort_key: "m".into(),
        }
    );
    assert_eq!(rows[0].parent_id, root);
    assert!(w.store.live_subtree(w.ws, a1).await.unwrap().is_empty());
}

/// A blob store that keeps objects in memory, without a `copy` of its own.
#[derive(Default)]
struct MemoryBlobs {
    objects: Mutex<HashMap<Uuid, (Vec<u8>, String)>>,
    deleted: Mutex<Vec<Uuid>>,
}

#[async_trait]
impl BlobStore for MemoryBlobs {
    async fn put(&self, id: Uuid, bytes: &[u8], content_type: &str) -> BlobResult<()> {
        self.objects
            .lock()
            .unwrap()
            .insert(id, (bytes.to_vec(), content_type.to_string()));
        Ok(())
    }

    async fn get(&self, id: Uuid) -> BlobResult<Vec<u8>> {
        let objects = self.objects.lock().unwrap();
        let (bytes, _) = objects.get(&id).ok_or(BlobStoreError::NotFound)?;
        Ok(bytes.clone())
    }

    async fn delete(&self, id: Uuid) -> BlobResult<()> {
        self.objects
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or(BlobStoreError::NotFound)?;
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }
}

#[tokio::test]
async fn copy_default_impl_copies_bytes() {
    let store = MemoryBlobs::default();
    let (src, dst) = (Uuid::new_v4(), Uuid::new_v4());
    store.put(src, b"bytes", "image/gif").await.unwrap();

    store.copy(src, dst, "image/png").await.unwrap();

    {
        let objects = store.objects.lock().unwrap();
        assert_eq!(objects[&dst], (b"bytes".to_vec(), "image/png".to_string()));
        assert_eq!(objects[&src], (b"bytes".to_vec(), "image/gif".to_string()));
    }
    let missing = store
        .copy(Uuid::new_v4(), Uuid::new_v4(), "image/png")
        .await;
    assert!(
        matches!(missing, Err(BlobStoreError::NotFound)),
        "{missing:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_stale_copies_deletes_unreferenced_objects_only() {
    let w = world().await;
    let s = w.seed(None, "S", "g").await;
    let store = MemoryBlobs::default();
    // A copy whose duplicate committed: its `blobs` row exists.
    let referenced = w.blob(s, b"kept", false).await.id;
    store.put(referenced, b"kept", "image/png").await.unwrap();
    // A copy whose duplicate never committed.
    let orphan = Uuid::new_v4();
    store.put(orphan, b"orphan", "image/png").await.unwrap();
    // One whose copy never arrived: there is nothing to delete.
    let never_copied = Uuid::new_v4();
    record_pending_copies(&w.pool, &[referenced, orphan, never_copied])
        .await
        .unwrap();

    // A duplicate may still be running for a recent entry.
    let removed = purge_stale_copies(&w.pool, &store, Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(removed, 0);
    assert_eq!(pending(&w.pool).await, 3);
    assert!(store.deleted.lock().unwrap().is_empty());

    let removed = purge_stale_copies(&w.pool, &store, Duration::ZERO)
        .await
        .unwrap();

    assert_eq!(removed, 2, "the orphan, and the copy that never arrived");
    assert_eq!(*store.deleted.lock().unwrap(), [orphan]);
    assert!(store.objects.lock().unwrap().contains_key(&referenced));
    assert_eq!(pending(&w.pool).await, 0);
}

/// How many entries `blob_copy_pending` holds.
async fn pending(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM blob_copy_pending")
        .fetch_one(pool)
        .await
        .unwrap()
}
